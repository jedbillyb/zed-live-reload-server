//! Out-of-band control channel.
//!
//! Zed extensions cannot register commands or status bar buttons, so the only
//! in-editor trigger available is a code action, which costs a menu. A Zed task
//! can be bound to a single key, but a task is a shell command and cannot reach
//! into the language server.
//!
//! This bridges that gap. While the language server is running it also listens
//! on a loopback port and records how to reach it in a small state file keyed by
//! workspace. `live-reload-lsp toggle <dir>` then finds that file and drives the
//! real server, so a one-key task gets the same behaviour as the code action,
//! including the user's settings and the unsaved-buffer mode.

use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, oneshot};

/// A command sent from the CLI to a running language server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    Start,
    Stop,
    /// Stop if running, start if not.
    Toggle,
    /// Open the served site in a browser, starting the server if needed.
    Open,
    Status,
}

impl Command {
    pub fn parse(text: &str) -> Option<Self> {
        match text.trim().to_ascii_lowercase().as_str() {
            "start" => Some(Command::Start),
            "stop" => Some(Command::Stop),
            "toggle" => Some(Command::Toggle),
            "open" => Some(Command::Open),
            "status" => Some(Command::Status),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Command::Start => "start",
            Command::Stop => "stop",
            Command::Toggle => "toggle",
            Command::Open => "open",
            Command::Status => "status",
        }
    }
}

/// A command plus somewhere to send the human-readable reply.
pub struct Request {
    pub workspace: PathBuf,
    pub command: Command,
    pub reply: oneshot::Sender<String>,
}

/* -------------------------------------------------------------- state file */

/// Every directory a state file might live in, most preferred first.
///
/// `XDG_RUNTIME_DIR` is preferred because it is user-private and cleared on
/// logout, which is exactly the lifetime these files want. The temp directory
/// is the fallback for platforms that do not set it.
///
/// Several candidates are listed rather than only the preferred one, because
/// the server and the CLI are separate processes that need not share an
/// environment. An editor launched from a desktop session has
/// `XDG_RUNTIME_DIR`; a shell started elsewhere may not. Writing to one place
/// and looking in another fails with a "no server" message that points nowhere
/// near the real cause.
fn state_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();

    if let Some(runtime) = std::env::var_os("XDG_RUNTIME_DIR") {
        dirs.push(PathBuf::from(runtime).join("live-reload"));
    } else if let Some(conventional) = conventional_runtime_dir() {
        // The variable being absent here does not mean the server lacked it.
        // A desktop session sets it, a bare shell often does not, and looking
        // only in the temp directory would miss a server that is running
        // perfectly well.
        dirs.push(conventional.join("live-reload"));
    }

    let temp = std::env::temp_dir().join("live-reload");
    if !dirs.contains(&temp) {
        dirs.push(temp);
    }
    dirs
}

/// `/run/user/<uid>`, if it exists.
///
/// The uid is taken from the ownership of `/proc/self` rather than through
/// libc, to avoid a dependency for one number.
#[cfg(target_os = "linux")]
fn conventional_runtime_dir() -> Option<PathBuf> {
    use std::os::unix::fs::MetadataExt;

    let uid = std::fs::metadata("/proc/self").ok()?.uid();
    let path = PathBuf::from(format!("/run/user/{uid}"));
    path.is_dir().then_some(path)
}

#[cfg(not(target_os = "linux"))]
fn conventional_runtime_dir() -> Option<PathBuf> {
    None
}

/// Filename a workspace's state is stored under.
///
/// The path is hashed rather than escaped so the name is a predictable length
/// and cannot collide with directory separators.
fn state_file_name(workspace: &Path) -> String {
    format!(
        "{:016x}.json",
        fnv1a(workspace.to_string_lossy().as_bytes())
    )
}

/// Where the server writes this workspace's state file.
pub fn state_path(workspace: &Path) -> PathBuf {
    let name = state_file_name(workspace);
    state_dirs()
        .into_iter()
        .next()
        .unwrap_or_else(std::env::temp_dir)
        .join(name)
}

/// Everywhere the CLI should look for a workspace's state file.
fn state_paths(workspace: &Path) -> Vec<PathBuf> {
    let name = state_file_name(workspace);
    state_dirs()
        .into_iter()
        .map(|dir| dir.join(&name))
        .collect()
}

/// FNV-1a. Hand-rolled because `DefaultHasher` is explicitly not guaranteed to
/// be stable between Rust releases, and the CLI has to derive the same name as
/// the server did, possibly from a differently built binary.
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// Serialises the state file. Hand-written to keep the format obvious; both
/// fields are machine generated and cannot contain characters needing escapes.
fn state_json(port: u16, token: &str, workspace: &Path) -> String {
    format!(
        "{{\"control_port\":{port},\"token\":\"{token}\",\"workspace\":{}}}\n",
        json_string(&workspace.to_string_lossy())
    )
}

fn json_string(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for character in value.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Pulls a value out of the state file without a JSON parser.
///
/// The file is written by [`state_json`] above and never by anything else, so
/// its shape is known. This avoids making the CLI path depend on serde_json
/// behaviour for a two-field file.
fn field<'a>(json: &'a str, key: &str) -> Option<&'a str> {
    let start = json.find(&format!("\"{key}\":"))? + key.len() + 3;
    let rest = &json[start..];
    if let Some(stripped) = rest.strip_prefix('"') {
        let end = stripped.find('"')?;
        Some(&stripped[..end])
    } else {
        let end = rest.find([',', '}'])?;
        Some(rest[..end].trim())
    }
}

/// Generates the shared secret guarding the control port.
///
/// The port is loopback-only, but any local process can reach loopback, so the
/// token is what actually gates it. It lives in a file only the user can read.
fn token() -> String {
    let mut bytes = [0u8; 16];
    if std::fs::File::open("/dev/urandom")
        .and_then(|mut file| std::io::Read::read_exact(&mut file, &mut bytes))
        .is_ok()
    {
        return bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    }

    // Fallback for platforms without /dev/urandom. Weaker, but combined with a
    // loopback-only socket and a private file it is adequate for a dev tool.
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    format!("{:032x}", nanos ^ ((std::process::id() as u128) << 96))
}

#[cfg(unix)]
fn restrict(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn restrict(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

/* ----------------------------------------------------------------- server */

/// Starts the control listener for one workspace.
///
/// Returns the state file path so the caller can remove it on shutdown. A
/// failure here is reported to the caller rather than being fatal: losing the
/// control channel costs the toggle command, but the code actions still work.
pub async fn listen(
    workspace: PathBuf,
    requests: mpsc::Sender<Request>,
) -> Result<PathBuf, String> {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|err| format!("control channel: {err}"))?;
    let port = listener
        .local_addr()
        .map_err(|err| format!("control channel: {err}"))?
        .port();

    let secret = token();
    let path = state_path(&workspace);
    let directory = path.parent().unwrap_or_else(|| Path::new("."));

    std::fs::create_dir_all(directory)
        .map_err(|err| format!("control channel: {}: {err}", directory.display()))?;
    std::fs::write(&path, state_json(port, &secret, &workspace))
        .map_err(|err| format!("control channel: {}: {err}", path.display()))?;
    restrict(&path).ok();

    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                continue;
            };
            let secret = secret.clone();
            let workspace = workspace.clone();
            let requests = requests.clone();
            tokio::spawn(async move {
                handle(stream, secret, workspace, requests).await;
            });
        }
    });

    Ok(path)
}

async fn handle(
    stream: TcpStream,
    secret: String,
    workspace: PathBuf,
    requests: mpsc::Sender<Request>,
) {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    if reader.read_line(&mut line).await.is_err() {
        return;
    }

    // Built as a plain value rather than written from several branches, so
    // there is exactly one write and no borrow juggling across await points.
    let response = respond(&line, &secret, workspace, requests).await;

    let stream = reader.get_mut();
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.write_all(b"\n").await;
}

/// Authenticates and runs one request line, returning the reply to send.
async fn respond(
    line: &str,
    secret: &str,
    workspace: PathBuf,
    requests: mpsc::Sender<Request>,
) -> String {
    let Some((presented, command)) = line.trim().split_once(char::is_whitespace) else {
        return "error: malformed request".to_string();
    };

    if presented != secret {
        return "error: bad token".to_string();
    }

    let Some(command) = Command::parse(command) else {
        return format!("error: unknown command {command:?}");
    };

    let (tx, rx) = oneshot::channel();
    if requests
        .send(Request {
            workspace,
            command,
            reply: tx,
        })
        .await
        .is_err()
    {
        return "error: server is shutting down".to_string();
    }

    rx.await
        .unwrap_or_else(|_| "error: no response".to_string())
}

/// Removes a workspace's state file.
pub fn remove_state(path: &Path) {
    std::fs::remove_file(path).ok();
}

/* ----------------------------------------------------------------- client */

/// Sends a command to the language server owning `workspace`.
///
/// The workspace path is canonicalised first so that a task passing a relative
/// or symlinked path still resolves to the same state file the server wrote.
pub fn send(workspace: &Path, command: Command) -> Result<String, String> {
    use std::io::{BufRead, Write};

    let workspace = workspace
        .canonicalize()
        .map_err(|err| format!("{}: {err}", workspace.display()))?;

    // Try each candidate directory, so an environment mismatch between the
    // editor and this process does not read as "no server".
    let candidates = state_paths(&workspace);
    let mut found = None;
    for candidate in &candidates {
        match std::fs::read_to_string(candidate) {
            Ok(state) => {
                found = Some((candidate.clone(), state));
                break;
            }
            Err(err) if err.kind() == ErrorKind::NotFound => continue,
            Err(err) => return Err(format!("{}: {err}", candidate.display())),
        }
    }

    let (path, state) = found.ok_or_else(|| {
        format!(
            "no Live Reload server for {}.\n\
             The editor has to be open on that project with a file open in it.",
            workspace.display()
        )
    })?;

    let port: u16 = field(&state, "control_port")
        .and_then(|value| value.parse().ok())
        .ok_or("state file is malformed")?;
    let secret = field(&state, "token").ok_or("state file is malformed")?;

    let mut stream = std::net::TcpStream::connect(("127.0.0.1", port)).map_err(|err| {
        // A leftover file from a server that has since exited is the common
        // case here, and saying so is more useful than the raw connect error.
        remove_state(&path);
        format!(
            "Live Reload is not running for {} ({err})",
            workspace.display()
        )
    })?;

    writeln!(stream, "{secret} {}", command.as_str()).map_err(|err| err.to_string())?;
    stream.flush().map_err(|err| err.to_string())?;

    let mut response = String::new();
    std::io::BufReader::new(stream)
        .read_line(&mut response)
        .map_err(|err| err.to_string())?;

    let response = response.trim().to_string();
    if let Some(message) = response.strip_prefix("error: ") {
        return Err(message.to_string());
    }
    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_every_command() {
        assert_eq!(Command::parse("start"), Some(Command::Start));
        assert_eq!(Command::parse(" STOP\n"), Some(Command::Stop));
        assert_eq!(Command::parse("Toggle"), Some(Command::Toggle));
        assert_eq!(Command::parse("open"), Some(Command::Open));
        assert_eq!(Command::parse("status"), Some(Command::Status));
        assert_eq!(Command::parse("delete-everything"), None);
    }

    #[test]
    fn command_names_round_trip() {
        for command in [
            Command::Start,
            Command::Stop,
            Command::Toggle,
            Command::Open,
            Command::Status,
        ] {
            assert_eq!(Command::parse(command.as_str()), Some(command));
        }
    }

    #[test]
    fn the_same_workspace_always_maps_to_the_same_state_file() {
        let a = state_path(Path::new("/srv/project"));
        let b = state_path(Path::new("/srv/project"));
        assert_eq!(a, b);
    }

    #[test]
    fn different_workspaces_map_to_different_state_files() {
        let a = state_path(Path::new("/srv/project"));
        let b = state_path(Path::new("/srv/other"));
        assert_ne!(a, b);
    }

    #[test]
    fn the_hash_is_stable_and_not_the_std_default_hasher() {
        // Pinned so a future toolchain cannot silently change where the CLI
        // looks for the state file.
        assert_eq!(fnv1a(b"/srv/project"), 0x52d4_6b4a_6ecd_f2c1);
    }

    #[test]
    fn reads_fields_back_out_of_the_state_file() {
        let json = state_json(45123, "deadbeef", Path::new("/srv/my project"));
        assert_eq!(field(&json, "control_port"), Some("45123"));
        assert_eq!(field(&json, "token"), Some("deadbeef"));
        assert_eq!(field(&json, "workspace"), Some("/srv/my project"));
    }

    #[test]
    fn escapes_awkward_workspace_paths() {
        let json = state_json(1, "t", Path::new("/srv/we\"ird\\path"));
        assert!(json.contains(r#"\"ird\\path"#), "{json}");
    }

    #[test]
    fn tokens_are_not_predictable() {
        assert_ne!(token(), token());
        assert!(token().len() >= 32);
    }

    #[tokio::test]
    async fn drives_a_command_end_to_end_over_the_socket() {
        let workspace = std::env::temp_dir().join("live-reload-control-test");
        std::fs::create_dir_all(&workspace).unwrap();

        let (tx, mut rx) = mpsc::channel::<Request>(4);
        let path = listen(workspace.clone(), tx).await.unwrap();

        // Stand in for the language server.
        tokio::spawn(async move {
            while let Some(request) = rx.recv().await {
                let _ = request
                    .reply
                    .send(format!("ok: {}", request.command.as_str()));
            }
        });

        let response = tokio::task::spawn_blocking(move || send(&workspace, Command::Toggle))
            .await
            .unwrap();
        assert_eq!(response.unwrap(), "ok: toggle");

        remove_state(&path);
    }

    #[tokio::test]
    async fn rejects_a_wrong_token() {
        let workspace = std::env::temp_dir().join("live-reload-control-auth-test");
        std::fs::create_dir_all(&workspace).unwrap();

        let (tx, mut rx) = mpsc::channel::<Request>(4);
        let path = listen(workspace.clone(), tx).await.unwrap();
        tokio::spawn(async move {
            while let Some(request) = rx.recv().await {
                let _ = request.reply.send("should not be reached".to_string());
            }
        });

        let state = std::fs::read_to_string(&path).unwrap();
        let port: u16 = field(&state, "control_port").unwrap().parse().unwrap();

        let response = tokio::task::spawn_blocking(move || {
            use std::io::{BufRead, Write};
            let mut stream = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
            writeln!(stream, "not-the-token toggle").unwrap();
            let mut line = String::new();
            std::io::BufReader::new(stream)
                .read_line(&mut line)
                .unwrap();
            line
        })
        .await
        .unwrap();

        assert_eq!(response.trim(), "error: bad token");
        remove_state(&path);
    }

    #[test]
    fn looks_in_both_the_runtime_and_temp_directories() {
        // The server and the CLI are separate processes and need not share an
        // environment, so a file written under either must still be found.
        let paths = state_paths(Path::new("/srv/project"));
        assert!(
            paths.len() >= 2 || std::env::var_os("XDG_RUNTIME_DIR").is_none(),
            "expected both candidates, got {paths:?}"
        );
        // Whatever the server would write is always among them.
        assert!(paths.contains(&state_path(Path::new("/srv/project"))));
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn falls_back_to_the_conventional_runtime_dir_when_the_variable_is_absent() {
        // A shell started outside a desktop session has no XDG_RUNTIME_DIR,
        // but the editor that started the server almost certainly did. Without
        // this the toggle reports "no server" for one that is running fine.
        let path = conventional_runtime_dir();
        if path.is_none() {
            return; // no /run/user on this machine, nothing to assert
        }
        let expected = path.unwrap().join("live-reload");

        let saved = std::env::var_os("XDG_RUNTIME_DIR");
        std::env::remove_var("XDG_RUNTIME_DIR");
        let dirs = state_dirs();
        if let Some(saved) = saved {
            std::env::set_var("XDG_RUNTIME_DIR", saved);
        }

        assert!(dirs.contains(&expected), "{dirs:?} is missing {expected:?}");
    }

    #[test]
    fn every_candidate_uses_the_same_file_name() {
        let paths = state_paths(Path::new("/srv/project"));
        let names: Vec<_> = paths.iter().filter_map(|p| p.file_name()).collect();
        assert!(names.windows(2).all(|w| w[0] == w[1]), "{paths:?}");
    }

    #[test]
    fn reports_a_missing_server_clearly() {
        let workspace = std::env::temp_dir().join("live-reload-absent-test");
        std::fs::create_dir_all(&workspace).unwrap();
        remove_state(&state_path(&workspace.canonicalize().unwrap()));

        let error = send(&workspace, Command::Status).unwrap_err();
        assert!(error.contains("no Live Reload server"), "{error}");
    }
}
