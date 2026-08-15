//! The LSP front end.
//!
//! Zed extensions cannot register commands, toolbar buttons or status bar
//! items, so a language server is the only place an extension can put behaviour
//! with the lifetime of a project. Everything the user can trigger therefore
//! arrives here: as a code action on the file they are looking at, as a command
//! executed from one, or as a document change.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use tokio::sync::RwLock;
use tower_lsp::jsonrpc::{Error as RpcError, Result as RpcResult};
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer};

use crate::config::Config;
use crate::control::{self, Command as ControlCommand, Request as ControlRequest};
use crate::overlay::Overlay;
use crate::reload::classify;
use crate::server::{open_browser, LiveServer, Status};

pub const CMD_START: &str = "liveReload.start";
pub const CMD_STOP: &str = "liveReload.stop";
pub const CMD_RESTART: &str = "liveReload.restart";
pub const CMD_OPEN: &str = "liveReload.open";

/// Prefix for the progress tokens used to drive the status bar item.
///
/// Zed surfaces LSP progress in its status bar, which is the closest thing an
/// extension has to a status indicator of its own. The item is begun when the
/// server starts and ended when it stops, so the address stays visible for as
/// long as the server is up.
///
/// Each distinct piece of status text gets its own token, because the text a
/// client displays is the `Begin` title: a `Report` only carries a secondary
/// message, which Zed appends after the title rather than replacing it. Keeping
/// one token for the whole session therefore left the status bar reading
/// "Live Reload: starting…" for as long as the server was up, since the width
/// available in the status bar truncated everything after it.
const PROGRESS_TOKEN_PREFIX: &str = "live-reload/status";

/// The progress item currently on screen.
#[derive(Clone)]
struct StatusItem {
    token: String,
    text: String,
}

#[derive(Clone)]
pub struct Backend {
    client: Client,
    config: Arc<RwLock<Arc<Config>>>,
    overlay: Overlay,
    servers: Arc<RwLock<HashMap<PathBuf, Arc<LiveServer>>>>,
    /// The progress item currently displayed, if any.
    status: Arc<RwLock<Option<StatusItem>>>,
    /// Serial number making each progress token unique.
    status_serial: Arc<AtomicU64>,
    /// Counter used to coalesce unsaved-buffer changes. Each keystroke takes a
    /// number; a pending reload only fires if no later keystroke arrived.
    keystroke: Arc<AtomicU64>,
    /// State files written by the control channel, removed on shutdown.
    control_files: Arc<RwLock<Vec<PathBuf>>>,
}

impl Backend {
    pub fn new(client: Client) -> Self {
        Self {
            client,
            config: Arc::new(RwLock::new(Arc::new(Config::default()))),
            overlay: Overlay::default(),
            servers: Arc::new(RwLock::new(HashMap::new())),
            status: Arc::new(RwLock::new(None)),
            status_serial: Arc::new(AtomicU64::new(0)),
            keystroke: Arc::new(AtomicU64::new(0)),
            control_files: Arc::new(RwLock::new(Vec::new())),
        }
    }

    async fn config(&self) -> Arc<Config> {
        self.config.read().await.clone()
    }

    /// Finds the server owning a file, choosing the most deeply nested
    /// workspace when they are nested inside one another.
    async fn server_for(&self, file: &Path) -> Option<Arc<LiveServer>> {
        let servers = self.servers.read().await;
        servers
            .iter()
            .filter(|(root, _)| file.starts_with(root))
            .max_by_key(|(root, _)| root.as_os_str().len())
            .map(|(_, server)| server.clone())
    }

    async fn server_at(&self, root: &Path) -> Option<Arc<LiveServer>> {
        let servers = self.servers.read().await;
        if let Some(server) = servers.get(root) {
            return Some(server.clone());
        }

        // The control channel canonicalises the path it is given, while these
        // keys are however the editor spelled the workspace root. Those differ
        // whenever a root is reached through a symlink, and on macOS for
        // anything under the temporary directory. Falling back to a resolved
        // comparison keeps a control command from reporting "no workspace" for
        // a server that is plainly running.
        let target = root.canonicalize().ok()?;
        servers
            .iter()
            .find(|(key, _)| key.canonicalize().map(|key| key == target).unwrap_or(false))
            .map(|(_, server)| server.clone())
    }

    /* ----------------------------------------------------------- reporting */

    async fn log(&self, message: impl std::fmt::Display) {
        self.client.log_message(MessageType::INFO, message).await;
    }

    async fn warn(&self, message: impl std::fmt::Display) {
        self.client
            .show_message(MessageType::WARNING, message)
            .await;
    }

    /// Ends the progress item behind `item`, if the client ever showed one.
    async fn end_progress(&self, item: &StatusItem) {
        self.client
            .send_notification::<notification::Progress>(ProgressParams {
                token: NumberOrString::String(item.token.clone()),
                value: ProgressParamsValue::WorkDone(WorkDoneProgress::End(WorkDoneProgressEnd {
                    message: None,
                })),
            })
            .await;
    }

    /// Shows or updates the status bar item.
    ///
    /// Changing the text ends the current progress item and begins a fresh one,
    /// since the title is fixed for the life of a token and the title is what
    /// gets displayed.
    async fn show_status(&self, text: String) {
        // Checked before taking the lock, since clearing takes it too. Clearing
        // rather than simply returning matters when the setting is turned off
        // while an item is on screen.
        if !self.config().await.status_bar {
            self.clear_status().await;
            return;
        }

        let mut status = self.status.write().await;

        if let Some(current) = status.as_ref() {
            if current.text == text {
                return;
            }
            self.end_progress(current).await;
        }

        let serial = self.status_serial.fetch_add(1, Ordering::SeqCst);
        let token = format!("{PROGRESS_TOKEN_PREFIX}/{serial}");

        // The client may decline to create the token, in which case the
        // notification below is simply ignored. Not worth failing over.
        let _ = self
            .client
            .send_request::<request::WorkDoneProgressCreate>(WorkDoneProgressCreateParams {
                token: NumberOrString::String(token.clone()),
            })
            .await;

        self.client
            .send_notification::<notification::Progress>(ProgressParams {
                token: NumberOrString::String(token.clone()),
                value: ProgressParamsValue::WorkDone(WorkDoneProgress::Begin(
                    WorkDoneProgressBegin {
                        title: text.clone(),
                        cancellable: Some(false),
                        message: None,
                        percentage: None,
                    },
                )),
            })
            .await;

        *status = Some(StatusItem { token, text });
    }

    async fn clear_status(&self) {
        let mut status = self.status.write().await;
        let Some(current) = status.take() else {
            return;
        };
        self.end_progress(&current).await;
    }

    /// Recomputes the status text from every server we own.
    async fn refresh_status(&self) {
        let servers = self.servers.read().await;
        let mut running = Vec::new();
        for server in servers.values() {
            if let Status::Running { port, .. } = server.status().await {
                running.push(port);
            }
        }
        drop(servers);

        if running.is_empty() {
            self.clear_status().await;
            return;
        }

        running.sort_unstable();
        let ports = running
            .iter()
            .map(|port| port.to_string())
            .collect::<Vec<_>>()
            .join(", ");
        self.show_status(format!("Live Reload :{ports}")).await;
    }

    /* ------------------------------------------------------------ actions */

    /// Starts a server and narrates it in the status bar.
    ///
    /// Deliberately silent otherwise. A `window/showMessage` becomes a toast in
    /// the corner of the editor that has to be dismissed, which is too much
    /// ceremony for something that happens on every press of the toggle key.
    /// The status bar item is the feedback. Warnings and failures still speak
    /// up.
    ///
    /// This is a departure from the VS Code extension, which does announce
    /// "Server is Started at port : 5500" and offers `donotShowInfoMsg` to
    /// turn it off. Worth reconsidering as a setting of the same shape if
    /// anyone misses it.
    async fn start(&self, server: &LiveServer) {
        // Mirrors the VS Code Live Server button's wording, so the status bar
        // narrates the transition rather than sitting blank while a port is
        // being bound and a file watcher registered.
        self.show_status("Live Reload: starting\u{2026}".to_string())
            .await;

        let config = self.config().await;
        match server.start(config.clone()).await {
            Ok((port, warnings)) => {
                for warning in warnings {
                    self.warn(warning).await;
                }
                let reach = if config.is_public() {
                    " (reachable on your network)"
                } else {
                    ""
                };
                self.log(format!(
                    "serving {} on {}:{port}{reach}",
                    server.workspace().display(),
                    config.host
                ))
                .await;

                if config.open_browser {
                    if let Some(url) = server.url_for(None, &config).await {
                        if let Err(err) = open_browser(&url, config.browser.as_deref()) {
                            self.warn(err).await;
                        }
                    }
                }
            }
            Err(err) => {
                self.warn(format!("Live Reload could not start: {err}"))
                    .await
            }
        }
        self.refresh_status().await;
    }

    async fn stop(&self, server: &LiveServer) {
        // Only narrate a stop that has something to stop.
        if server.status().await == Status::Stopped {
            self.refresh_status().await;
            return;
        }

        self.show_status("Live Reload: disposing\u{2026}".to_string())
            .await;
        server.stop().await;
        self.refresh_status().await;
    }

    /// Starts the control channel for every workspace and the task that
    /// services it. Failure is reported but not fatal: without it the toggle
    /// command stops working, while the code actions carry on unaffected.
    async fn start_control_channel(&self) {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<ControlRequest>(16);

        let roots: Vec<PathBuf> = self.servers.read().await.keys().cloned().collect();
        for root in roots {
            match control::listen(root, tx.clone()).await {
                Ok(path) => self.control_files.write().await.push(path),
                Err(err) => self.log(err).await,
            }
        }

        let backend = self.clone();
        tokio::spawn(async move {
            while let Some(request) = rx.recv().await {
                let response = backend
                    .run_control(request.workspace, request.command)
                    .await;
                let _ = request.reply.send(response);
            }
        });
    }

    /// Runs one control command and describes the outcome for the caller's
    /// terminal, since there is no editor UI on that side.
    async fn run_control(&self, workspace: PathBuf, command: ControlCommand) -> String {
        let Some(server) = self.server_at(&workspace).await else {
            return format!("error: no workspace at {}", workspace.display());
        };
        let config = self.config().await;
        let running = server.status().await != Status::Stopped;

        let command = match command {
            // Resolved here so the reply describes what actually happened
            // rather than the word the user typed.
            ControlCommand::Toggle if running => ControlCommand::Stop,
            ControlCommand::Toggle => ControlCommand::Start,
            // `Go` is the same shape, except that the start half also opens a
            // browser, which `Open` already does.
            ControlCommand::Go if running => ControlCommand::Stop,
            ControlCommand::Go => ControlCommand::Open,
            other => other,
        };

        match command {
            ControlCommand::Stop => {
                self.stop(&server).await;
                "stopped".to_string()
            }
            ControlCommand::Start => {
                if running {
                    return match server.status().await {
                        Status::Running { port, .. } => format!("already running on :{port}"),
                        Status::Stopped => "stopped".to_string(),
                    };
                }
                self.start(&server).await;
                match server.status().await {
                    Status::Running { port, .. } => format!("started on :{port}"),
                    Status::Stopped => "error: failed to start".to_string(),
                }
            }
            ControlCommand::Open => {
                if !running {
                    self.start(&server).await;
                }
                match server.url_for(None, &config).await {
                    Some(url) => match open_browser(&url, config.browser.as_deref()) {
                        Ok(()) => format!("opened {url}"),
                        Err(err) => format!("error: {err}"),
                    },
                    None => "error: failed to start".to_string(),
                }
            }
            ControlCommand::Status => match server.status().await {
                Status::Running { host, port } => format!("running on {host}:{port}"),
                Status::Stopped => "stopped".to_string(),
            },
            // Already resolved above.
            ControlCommand::Toggle | ControlCommand::Go => {
                unreachable!("toggle and go are resolved before dispatch")
            }
        }
    }

    /* ---------------------------------------------------------- documents */

    /// Pushes a reload for an unsaved buffer change.
    ///
    /// Only reached when `live_changes` is on. Saved changes are picked up by
    /// the filesystem watcher instead, so that edits made outside the editor
    /// count too and a single save cannot reload the page twice.
    ///
    /// Changes are coalesced over `wait`, because this runs on every keystroke
    /// and reloading the page per character typed would be unusable.
    async fn buffer_changed(&self, file: &Path) {
        let config = self.config().await;
        if !config.live_changes {
            return;
        }
        let Some(server) = self.server_for(file).await else {
            return;
        };
        let Status::Running { .. } = server.status().await else {
            return;
        };

        // Hot swapping needs the URL the file is served at, which only exists
        // for files inside the document root.
        let Some(url) = server.url_for(Some(file), &config).await else {
            return;
        };
        let path = url
            .split_once("://")
            .and_then(|(_, rest)| rest.find('/').map(|index| rest[index..].to_string()))
            .unwrap_or_else(|| "/".to_string());

        let instruction = classify(&path, config.full_reload);

        // Claim a number, then fire only if still the most recent claim once
        // the quiet window has passed.
        let ticket = self.keystroke.fetch_add(1, Ordering::SeqCst) + 1;
        let keystroke = self.keystroke.clone();
        let wait = Duration::from_millis(config.wait.max(30));

        tokio::spawn(async move {
            tokio::time::sleep(wait).await;
            if keystroke.load(Ordering::SeqCst) == ticket {
                server.notify(instruction).await;
            }
        });
    }

    /// Rebuilds every running server after the configuration changed.
    async fn reconfigure(&self, value: Option<Value>) {
        let (config, error) = Config::parse(value);
        if let Some(error) = error {
            self.warn(format!("Live Reload config ignored: {error}"))
                .await;
            return;
        }

        // Clients push the current configuration once at startup, and may
        // resend it whenever anything in settings changes. Restarting on every
        // such notification would tear down a server the user is already using
        // and, because the browser was sent to the original port, strand the
        // page they are looking at.
        if config == **self.config.read().await {
            return;
        }

        *self.config.write().await = Arc::new(config);
        let config = self.config().await;

        let servers: Vec<Arc<LiveServer>> = self.servers.read().await.values().cloned().collect();
        for server in servers {
            // Only restart what was already running, so changing a setting does
            // not start servers the user had deliberately stopped.
            if server.status().await == Status::Stopped {
                continue;
            }
            if let Err(err) = server.restart(config.clone()).await {
                self.warn(format!("Live Reload could not restart: {err}"))
                    .await;
            }
        }
        self.refresh_status().await;
    }
}

fn file_path(uri: &Url) -> Option<PathBuf> {
    uri.to_file_path().ok()
}

/// Builds a code action that runs one of our commands.
fn action(title: &str, command: &str, arguments: Vec<Value>) -> CodeActionOrCommand {
    CodeActionOrCommand::CodeAction(CodeAction {
        title: title.to_string(),
        kind: Some(CodeActionKind::EMPTY),
        command: Some(Command {
            title: title.to_string(),
            command: command.to_string(),
            arguments: Some(arguments),
        }),
        ..Default::default()
    })
}

#[tower_lsp::async_trait]
impl LanguageServer for Backend {
    async fn initialize(&self, params: InitializeParams) -> RpcResult<InitializeResult> {
        let (config, error) = Config::parse(params.initialization_options);
        *self.config.write().await = Arc::new(config);

        // Reported after initialize completes, since notifications sent during
        // initialization are not guaranteed to be displayed.
        if let Some(error) = error {
            let client = self.client.clone();
            tokio::spawn(async move {
                client
                    .show_message(
                        MessageType::WARNING,
                        format!("Live Reload config ignored, using defaults: {error}"),
                    )
                    .await;
            });
        }

        let mut roots = Vec::new();
        if let Some(folders) = params.workspace_folders {
            roots.extend(
                folders
                    .into_iter()
                    .filter_map(|folder| file_path(&folder.uri)),
            );
        }
        #[allow(deprecated)]
        if roots.is_empty() {
            if let Some(root) = params.root_uri.as_ref().and_then(file_path) {
                roots.push(root);
            }
        }

        let mut servers = self.servers.write().await;
        for root in roots {
            servers.insert(
                root.clone(),
                Arc::new(LiveServer::new(root, self.overlay.clone())),
            );
        }
        drop(servers);

        let live_changes = self.config().await.live_changes;

        Ok(InitializeResult {
            server_info: Some(ServerInfo {
                name: "Live Reload".to_string(),
                version: Some(env!("CARGO_PKG_VERSION").to_string()),
            }),
            capabilities: ServerCapabilities {
                code_action_provider: Some(CodeActionProviderCapability::Simple(true)),
                execute_command_provider: Some(ExecuteCommandOptions {
                    commands: vec![
                        CMD_START.to_string(),
                        CMD_STOP.to_string(),
                        CMD_RESTART.to_string(),
                        CMD_OPEN.to_string(),
                    ],
                    ..Default::default()
                }),
                // Streaming every keystroke is only worth the traffic when the
                // unsaved-buffer feature is actually on.
                text_document_sync: Some(TextDocumentSyncCapability::Options(
                    TextDocumentSyncOptions {
                        open_close: Some(true),
                        change: Some(if live_changes {
                            TextDocumentSyncKind::INCREMENTAL
                        } else {
                            TextDocumentSyncKind::NONE
                        }),
                        save: Some(TextDocumentSyncSaveOptions::SaveOptions(SaveOptions {
                            include_text: Some(false),
                        })),
                        ..Default::default()
                    },
                )),
                workspace: Some(WorkspaceServerCapabilities {
                    workspace_folders: Some(WorkspaceFoldersServerCapabilities {
                        supported: Some(true),
                        change_notifications: Some(OneOf::Left(true)),
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            },
        })
    }

    async fn initialized(&self, _: InitializedParams) {
        let config = self.config().await;
        self.log(format!("Live Reload {} ready", env!("CARGO_PKG_VERSION")))
            .await;

        self.start_control_channel().await;

        if !config.auto_start {
            return;
        }

        let servers: Vec<Arc<LiveServer>> = self.servers.read().await.values().cloned().collect();
        for server in servers {
            self.start(&server).await;
        }
    }

    async fn shutdown(&self) -> RpcResult<()> {
        let servers: Vec<Arc<LiveServer>> = self.servers.read().await.values().cloned().collect();
        for server in servers {
            server.stop().await;
        }
        // Leaving these behind would make the CLI report a running server that
        // is not there, until it tried to connect and cleaned up after itself.
        for path in self.control_files.read().await.iter() {
            control::remove_state(path);
        }
        self.clear_status().await;
        Ok(())
    }

    async fn did_change_configuration(&self, params: DidChangeConfigurationParams) {
        // Clients wrap the settings under the server's section name, or send
        // them bare. Unwrap the section if it is there.
        let settings = params
            .settings
            .get("live-reload")
            .cloned()
            .or(Some(params.settings));
        self.reconfigure(settings).await;
    }

    async fn did_change_workspace_folders(&self, params: DidChangeWorkspaceFoldersParams) {
        let config = self.config().await;

        for folder in params.event.removed {
            if let Some(root) = file_path(&folder.uri) {
                if let Some(server) = self.servers.write().await.remove(&root) {
                    server.stop().await;
                }
            }
        }

        for folder in params.event.added {
            let Some(root) = file_path(&folder.uri) else {
                continue;
            };
            let server = Arc::new(LiveServer::new(root.clone(), self.overlay.clone()));
            self.servers.write().await.insert(root, server.clone());
            if config.auto_start {
                self.start(&server).await;
            }
        }

        self.refresh_status().await;
    }

    /* ---------------------------------------------------------- documents */

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        if !self.config().await.live_changes {
            return;
        }
        let Some(path) = file_path(&params.text_document.uri) else {
            return;
        };
        self.overlay.set(path, params.text_document.text).await;
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        if !self.config().await.live_changes {
            return;
        }
        let Some(path) = file_path(&params.text_document.uri) else {
            return;
        };

        for change in params.content_changes {
            self.overlay.apply(&path, change.range, &change.text).await;
        }
        self.buffer_changed(&path).await;
    }

    async fn did_save(&self, params: DidSaveTextDocumentParams) {
        let Some(path) = file_path(&params.text_document.uri) else {
            return;
        };
        // Once the buffer is on disk the overlay is redundant, and keeping it
        // would mask any change made to the file by another program.
        self.overlay.clear(&path).await;
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        let Some(path) = file_path(&params.text_document.uri) else {
            return;
        };
        self.overlay.clear(&path).await;
    }

    /* ------------------------------------------------------------ actions */

    async fn code_action(&self, params: CodeActionParams) -> RpcResult<Option<CodeActionResponse>> {
        let Some(file) = file_path(&params.text_document.uri) else {
            return Ok(None);
        };
        let Some(server) = self.server_for(&file).await else {
            return Ok(None);
        };

        let root = Value::from(server.workspace().to_string_lossy().to_string());
        let file_argument = Value::from(file.to_string_lossy().to_string());

        // The offered actions describe the server's actual state, so the list
        // never contains something that would be a no-op.
        let actions = match server.status().await {
            Status::Running { port, .. } => vec![
                action(
                    &format!("Live Reload: open this file in the browser (:{port})"),
                    CMD_OPEN,
                    vec![root.clone(), file_argument],
                ),
                action(
                    &format!("Live Reload: restart server (:{port})"),
                    CMD_RESTART,
                    vec![root.clone()],
                ),
                action(
                    &format!("Live Reload: stop server (:{port})"),
                    CMD_STOP,
                    vec![root],
                ),
            ],
            Status::Stopped => vec![action("Live Reload: start server", CMD_START, vec![root])],
        };

        Ok(Some(actions))
    }

    async fn execute_command(&self, params: ExecuteCommandParams) -> RpcResult<Option<Value>> {
        let mut arguments = params.arguments.into_iter();
        let root = arguments
            .next()
            .and_then(|argument| argument.as_str().map(PathBuf::from));

        // A command with no workspace argument applies to every workspace,
        // which is what a keybinding or task invocation will send.
        let servers: Vec<Arc<LiveServer>> = match &root {
            Some(root) => self
                .server_at(root)
                .await
                .map(|server| vec![server])
                .unwrap_or_default(),
            None => self.servers.read().await.values().cloned().collect(),
        };

        if servers.is_empty() {
            return Err(RpcError::invalid_params("no matching workspace"));
        }

        match params.command.as_str() {
            CMD_START => {
                for server in &servers {
                    self.start(server).await;
                }
            }
            CMD_STOP => {
                for server in &servers {
                    self.stop(server).await;
                }
            }
            CMD_RESTART => {
                let config = self.config().await;
                for server in &servers {
                    match server.restart(config.clone()).await {
                        Ok((port, _)) => self.log(format!("restarted on :{port}")).await,
                        Err(err) => {
                            self.warn(format!("Live Reload could not restart: {err}"))
                                .await
                        }
                    }
                }
                self.refresh_status().await;
            }
            CMD_OPEN => {
                let config = self.config().await;
                let file = arguments
                    .next()
                    .and_then(|argument| argument.as_str().map(PathBuf::from));

                for server in &servers {
                    // Opening implies wanting a server, so start one rather
                    // than reporting that there is nothing to open.
                    if server.status().await == Status::Stopped {
                        self.start(server).await;
                    }

                    let Some(url) = server.url_for(file.as_deref(), &config).await else {
                        continue;
                    };
                    if let Err(err) = open_browser(&url, config.browser.as_deref()) {
                        self.warn(err).await;
                    }
                }
            }
            other => {
                return Err(RpcError::invalid_params(format!(
                    "unknown command: {other}"
                )))
            }
        }

        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_an_action_carrying_its_command() {
        let CodeActionOrCommand::CodeAction(built) =
            action("Start", CMD_START, vec![Value::from("/srv")])
        else {
            panic!("expected a code action");
        };
        let command = built.command.unwrap();
        assert_eq!(command.command, CMD_START);
        assert_eq!(command.arguments.unwrap(), vec![Value::from("/srv")]);
    }

    #[tokio::test]
    async fn picks_the_most_deeply_nested_workspace_for_a_file() {
        let outer = PathBuf::from("/srv/project");
        let inner = PathBuf::from("/srv/project/site");

        let servers: HashMap<PathBuf, Arc<LiveServer>> = [
            (
                outer.clone(),
                Arc::new(LiveServer::new(outer.clone(), Overlay::default())),
            ),
            (
                inner.clone(),
                Arc::new(LiveServer::new(inner.clone(), Overlay::default())),
            ),
        ]
        .into_iter()
        .collect();

        let file = Path::new("/srv/project/site/index.html");
        let chosen = servers
            .iter()
            .filter(|(root, _)| file.starts_with(root))
            .max_by_key(|(root, _)| root.as_os_str().len())
            .map(|(root, _)| root.clone())
            .unwrap();

        assert_eq!(chosen, inner);
    }
}
