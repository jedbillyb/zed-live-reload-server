//! Server lifecycle: starting, stopping and addressing one workspace's server.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;

use notify::Watcher;
use tokio::net::TcpListener;
use tokio::sync::{broadcast, Mutex};
use tokio::task::JoinHandle;

use crate::config::{Config, PORT_SCAN_LIMIT};
use crate::http::{self, Context};
use crate::overlay::Overlay;
use crate::reload::Reload;
use crate::watch;

/// Capacity of the reload broadcast. Generous enough that a bulk change does
/// not lag a browser that is merely slow to be scheduled.
const RELOAD_CAPACITY: usize = 64;

/// A running server.
struct Instance {
    host: String,
    port: u16,
    /// Sending here reaches every connected browser. Dropping it closes them,
    /// which is how [`LiveServer::stop`] tears down live WebSocket connections.
    reload: broadcast::Sender<Reload>,
    /// Signalled on stop to close connections that are open but idle.
    shutdown: broadcast::Sender<()>,
    accept: JoinHandle<()>,
    /// Kept alive only so the watch stays registered; dropping it stops it.
    _watcher: Box<dyn Watcher + Send>,
}

/// What the editor should show about a workspace's server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    Stopped,
    Running { host: String, port: u16 },
}

/// Owns the server for a single workspace folder.
pub struct LiveServer {
    workspace: PathBuf,
    overlay: Overlay,
    instance: Mutex<Option<Instance>>,
}

impl LiveServer {
    pub fn new(workspace: PathBuf, overlay: Overlay) -> Self {
        Self {
            workspace,
            overlay,
            instance: Mutex::new(None),
        }
    }

    pub fn workspace(&self) -> &Path {
        &self.workspace
    }

    pub async fn status(&self) -> Status {
        match &*self.instance.lock().await {
            Some(instance) => Status::Running {
                host: instance.host.clone(),
                port: instance.port,
            },
            None => Status::Stopped,
        }
    }

    /// Starts the server, or returns the existing address if already running.
    ///
    /// Warnings about the configuration are returned alongside the port rather
    /// than raised as errors, so a bad ignore glob does not stop a server that
    /// is otherwise fine from coming up.
    pub async fn start(&self, config: Arc<Config>) -> Result<(u16, Vec<String>), String> {
        let mut guard = self.instance.lock().await;
        if let Some(instance) = &*guard {
            return Ok((instance.port, Vec::new()));
        }

        let root = document_root(&self.workspace, &config.root);
        if !root.is_dir() {
            return Err(format!("document root {} does not exist", root.display()));
        }

        let (listener, port) = bind(&config).await?;
        let (reload, _) = broadcast::channel(RELOAD_CAPACITY);
        let (shutdown, _) = broadcast::channel(1);
        let (ignore, mut warnings) = config.ignore_set();

        let watcher = match watch::watch(root.clone(), config.clone(), ignore, reload.clone()) {
            Ok(watcher) => Box::new(watcher) as Box<dyn Watcher + Send>,
            Err(err) => {
                // Losing the watcher costs automatic reloads but the server is
                // still useful, so this is reported rather than fatal.
                warnings.push(format!("file watching unavailable: {err}"));
                Box::new(NullWatcher) as Box<dyn Watcher + Send>
            }
        };

        let context = Arc::new(Context {
            config: config.clone(),
            root,
            workspace: self.workspace.clone(),
            overlay: self.overlay.clone(),
            reload: reload.clone(),
            shutdown: shutdown.clone(),
        });

        let accept = tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((stream, _)) => {
                        let context = context.clone();
                        tokio::spawn(http::serve_connection(stream, context));
                    }
                    // A per-connection failure (a dropped SYN, an fd limit) is
                    // not a reason to take the whole server down.
                    Err(_) => tokio::time::sleep(std::time::Duration::from_millis(50)).await,
                }
            }
        });

        *guard = Some(Instance {
            host: config.host.clone(),
            port,
            reload,
            shutdown,
            accept,
            _watcher: watcher,
        });

        Ok((port, warnings))
    }

    /// Stops the server. Returns whether one was running.
    pub async fn stop(&self) -> bool {
        let Some(instance) = self.instance.lock().await.take() else {
            return false;
        };
        instance.accept.abort();
        // Aborting only schedules the task to be dropped, and the listening
        // socket lives until that happens. Without waiting here, an immediate
        // restart races the old listener and the rebind lands on the next port
        // up, stranding any browser already pointed at the original one.
        let _ = instance.accept.await;
        // Closes connections that are open but idle between requests, so a
        // pooled browser socket cannot be served after the server has stopped.
        let _ = instance.shutdown.send(());
        // Dropping the sender closes every browser's WebSocket, so a page left
        // open stops waiting for reloads that are never coming and falls back
        // to retrying quietly.
        drop(instance.reload);
        self.overlay.clear_all().await;
        true
    }

    pub async fn restart(&self, config: Arc<Config>) -> Result<(u16, Vec<String>), String> {
        self.stop().await;
        self.start(config).await
    }

    /// Broadcasts a reload to connected browsers, if any are listening.
    pub async fn notify(&self, instruction: Reload) {
        if let Some(instance) = &*self.instance.lock().await {
            let _ = instance.reload.send(instruction);
        }
    }

    /// Builds the browsable URL for a file inside the workspace.
    pub async fn url_for(&self, file: Option<&Path>, config: &Config) -> Option<String> {
        let Status::Running { host, port } = self.status().await else {
            return None;
        };

        // `0.0.0.0` is an address to bind, not one to browse to.
        let host = match host.as_str() {
            "0.0.0.0" => "127.0.0.1".to_string(),
            "::" | "[::]" => "[::1]".to_string(),
            other => other.to_string(),
        };

        let root = document_root(&self.workspace, &config.root);
        let relative = file
            .and_then(|file| file.strip_prefix(&root).ok())
            .map(encode_url_path)
            .unwrap_or_default();

        Some(format!("http://{host}:{port}/{relative}"))
    }
}

/// Resolves the document root from the workspace and the `root` setting.
fn document_root(workspace: &Path, root: &str) -> PathBuf {
    let trimmed = root.trim_matches('/');
    if trimmed.is_empty() {
        workspace.to_path_buf()
    } else {
        workspace.join(trimmed)
    }
}

/// Binds the configured port, walking upward when it is taken.
///
/// Port 0 is honoured as "any free port" rather than scanned, since that is
/// what the operating system already means by it.
async fn bind(config: &Config) -> Result<(TcpListener, u16), String> {
    if config.port == 0 {
        let address = format!("{}:0", config.host);
        let listener = TcpListener::bind(&address)
            .await
            .map_err(|err| format!("could not bind {address}: {err}"))?;
        let port = listener.local_addr().map_err(|err| err.to_string())?.port();
        return Ok((listener, port));
    }

    let mut last_error = String::new();
    for offset in 0..PORT_SCAN_LIMIT {
        let Some(port) = config.port.checked_add(offset) else {
            break;
        };
        let address: SocketAddr = match format!("{}:{port}", config.host).parse() {
            Ok(address) => address,
            Err(_) => {
                // A hostname rather than a literal address, so let the resolver
                // handle it and give up on scanning.
                let address = format!("{}:{port}", config.host);
                return TcpListener::bind(&address)
                    .await
                    .map(|listener| (listener, port))
                    .map_err(|err| format!("could not bind {address}: {err}"));
            }
        };

        match TcpListener::bind(address).await {
            Ok(listener) => return Ok((listener, port)),
            Err(err) if err.kind() == std::io::ErrorKind::AddrInUse => {
                last_error = err.to_string();
                continue;
            }
            Err(err) => return Err(format!("could not bind {address}: {err}")),
        }
    }

    Err(format!(
        "no free port in {}..{} on {} ({last_error})",
        config.port,
        config.port.saturating_add(PORT_SCAN_LIMIT),
        config.host
    ))
}

/// Percent-encodes a relative path for use in a URL.
fn encode_url_path(path: &Path) -> String {
    path.components()
        .filter_map(|component| match component {
            std::path::Component::Normal(part) => Some(encode_segment(&part.to_string_lossy())),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/")
}

fn encode_segment(segment: &str) -> String {
    let mut out = String::with_capacity(segment.len());
    for byte in segment.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// Opens a URL in the browser.
///
/// Spawned detached with its output discarded: some launchers stay in the
/// foreground and would otherwise block the server, and any chatter they print
/// would land in the middle of the LSP stdio stream and corrupt it.
pub fn open_browser(url: &str, browser: Option<&str>) -> Result<(), String> {
    let mut command = match browser {
        Some(browser) => {
            // Accept a command with arguments, so a user can pass browser flags.
            let mut parts = browser.split_whitespace();
            let program = parts.next().ok_or("browser setting is empty")?;
            let mut command = std::process::Command::new(program);
            command.args(parts);
            command.arg(url);
            command
        }
        None => {
            #[cfg(target_os = "macos")]
            let (program, leading): (&str, &[&str]) = ("open", &[]);
            // The empty string is the window title, which `start` would
            // otherwise mistake the URL for.
            #[cfg(target_os = "windows")]
            let (program, leading): (&str, &[&str]) = ("cmd", &["/C", "start", ""]);
            #[cfg(not(any(target_os = "macos", target_os = "windows")))]
            let (program, leading): (&str, &[&str]) = ("xdg-open", &[]);

            let mut command = std::process::Command::new(program);
            command.args(leading);
            command.arg(url);
            command
        }
    };

    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(|_| ())
        .map_err(|err| format!("could not open browser: {err}"))
}

/// Stand-in used when the real watcher could not be created.
struct NullWatcher;

impl Watcher for NullWatcher {
    fn new<F: notify::EventHandler>(_: F, _: notify::Config) -> notify::Result<Self> {
        Ok(NullWatcher)
    }
    fn watch(&mut self, _: &Path, _: notify::RecursiveMode) -> notify::Result<()> {
        Ok(())
    }
    fn unwatch(&mut self, _: &Path) -> notify::Result<()> {
        Ok(())
    }
    fn kind() -> notify::WatcherKind {
        notify::WatcherKind::NullWatcher
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_the_document_root_from_the_root_setting() {
        let workspace = Path::new("/srv/project");
        assert_eq!(document_root(workspace, "/"), workspace);
        assert_eq!(document_root(workspace, ""), workspace);
        assert_eq!(
            document_root(workspace, "/public"),
            Path::new("/srv/project/public")
        );
        assert_eq!(
            document_root(workspace, "dist/"),
            Path::new("/srv/project/dist")
        );
    }

    #[test]
    fn encodes_url_paths() {
        assert_eq!(encode_url_path(Path::new("index.html")), "index.html");
        assert_eq!(
            encode_url_path(Path::new("a b/c&d.html")),
            "a%20b/c%26d.html"
        );
    }

    #[tokio::test]
    async fn scans_upward_when_the_port_is_taken() {
        let config = Config {
            port: 0,
            ..Config::default()
        };
        let (listener, port) = bind(&config).await.unwrap();
        assert!(port > 0);

        // Now ask for the port we just took, and expect to be moved off it.
        let config = Config {
            port,
            ..Config::default()
        };
        let (_next, next_port) = bind(&config).await.unwrap();

        // Deliberately not asserting exactly `port + 1`. Tests run in parallel
        // and the machine has other things on it, so the next port up may
        // legitimately be busy and the scan continues past it. Requiring
        // adjacency made this fail on a loaded CI runner.
        assert!(
            next_port > port && next_port <= port + PORT_SCAN_LIMIT,
            "expected a port in {}..={}, got {next_port}",
            port + 1,
            port + PORT_SCAN_LIMIT
        );
        drop(listener);
    }

    #[tokio::test]
    async fn reports_a_stopped_server_as_stopped() {
        let server = LiveServer::new(PathBuf::from("/srv/project"), Overlay::default());
        assert_eq!(server.status().await, Status::Stopped);
        assert!(!server.stop().await);
        assert!(server.url_for(None, &Config::default()).await.is_none());
    }

    #[tokio::test]
    async fn a_restart_keeps_the_same_port() {
        // Regression: `stop` used to only `abort` the accept task, which left
        // the listening socket alive until that task was actually dropped. An
        // immediate restart then found the port still taken and scanned to the
        // next one, stranding any browser already pointed at the original.
        let directory = std::env::temp_dir().join("live-reload-restart-test");
        std::fs::create_dir_all(&directory).unwrap();

        let server = LiveServer::new(directory, Overlay::default());
        let config = Arc::new(Config {
            port: 0,
            open_browser: false,
            ..Config::default()
        });

        let (first, _) = server.start(config.clone()).await.unwrap();

        // Ask for the port it just had, which is what a real restart does.
        let config = Arc::new(Config {
            port: first,
            open_browser: false,
            ..Config::default()
        });
        let (second, _) = server.restart(config).await.unwrap();

        assert_eq!(
            second, first,
            "restart moved the server from {first} to {second}"
        );
        server.stop().await;
    }

    #[tokio::test]
    async fn refuses_to_start_with_a_missing_document_root() {
        let server = LiveServer::new(
            PathBuf::from("/srv/definitely-not-here"),
            Overlay::default(),
        );
        let error = server.start(Arc::new(Config::default())).await.unwrap_err();
        assert!(error.contains("does not exist"), "{error}");
    }
}
