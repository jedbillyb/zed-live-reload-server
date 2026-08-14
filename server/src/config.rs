//! User-facing configuration.
//!
//! Option names deliberately mirror the VS Code Live Server extension where an
//! equivalent exists, converted to snake_case to match Zed's settings style, so
//! that people moving over can transfer what they already know.

use globset::{Glob, GlobSet, GlobSetBuilder};
use serde::Deserialize;

/// Default port, chosen to match the VS Code extension.
pub const DEFAULT_PORT: u16 = 5500;

/// Number of consecutive ports to try before giving up when the configured one
/// is already taken.
pub const PORT_SCAN_LIMIT: u16 = 50;

fn default_port() -> u16 {
    DEFAULT_PORT
}
fn default_host() -> String {
    "127.0.0.1".to_string()
}
fn default_root() -> String {
    "/".to_string()
}
fn default_index() -> String {
    "index.html".to_string()
}
fn default_true() -> bool {
    true
}
fn default_wait() -> u64 {
    100
}
fn default_ignore() -> Vec<String> {
    // Directories are listed in both forms so the directory's own creation
    // event is filtered as well as its contents. Ancestor matching in the
    // watcher makes either form sufficient, but being explicit here keeps the
    // defaults readable on their own.
    [
        "**/.git",
        "**/.git/**",
        "**/node_modules",
        "**/node_modules/**",
        "**/target",
        "**/target/**",
        "**/.DS_Store",
        "**/*.log",
        "**/*.swp",
        "**/*~",
        "**/.#*",
    ]
    .iter()
    .map(|pattern| pattern.to_string())
    .collect()
}

/// A path prefix served from somewhere outside the document root.
#[derive(Debug, Clone, Deserialize)]
pub struct Mount {
    /// URL prefix, for example `/lib`.
    pub route: String,
    /// Directory to serve it from, absolute or relative to the workspace root.
    pub path: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Preferred port. If it is taken the server walks upward from here.
    #[serde(alias = "start_port")]
    pub port: u16,
    /// Interface to bind. Set to `0.0.0.0` to reach the server from a phone or
    /// another machine on the same network.
    pub host: String,
    /// Document root, relative to the workspace root.
    pub root: String,
    /// Start serving as soon as the project opens.
    pub auto_start: bool,
    /// Open a browser when the server starts.
    pub open_browser: bool,
    /// Browser command to use. `None` defers to the system default handler.
    pub browser: Option<String>,
    /// File served for directory requests.
    pub index: String,
    /// Serve `index` for unknown paths instead of a 404, for client-side routers.
    pub spa: bool,
    /// Send permissive CORS headers.
    pub cors: bool,
    /// Render a browsable listing when a directory has no index file.
    pub directory_listing: bool,
    /// Milliseconds to coalesce rapid file changes into one reload. Also gives
    /// tools that write a file in several chunks time to finish.
    pub wait: u64,
    /// Always reload the whole page, even for changes that could be hot swapped.
    pub full_reload: bool,
    /// Glob patterns whose changes never trigger a reload.
    pub ignore_files: Vec<String>,
    /// Extra directories mounted outside the document root.
    pub mount: Vec<Mount>,
    /// BETA. Serve the editor's unsaved buffer instead of the file on disk, so
    /// the page updates as you type. See the README for the caveats.
    pub live_changes: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            port: default_port(),
            host: default_host(),
            root: default_root(),
            auto_start: default_true(),
            open_browser: default_true(),
            browser: None,
            index: default_index(),
            spa: false,
            cors: false,
            directory_listing: default_true(),
            wait: default_wait(),
            full_reload: false,
            ignore_files: default_ignore(),
            mount: Vec::new(),
            live_changes: false,
        }
    }
}

impl Config {
    /// Parses config sent by the editor.
    ///
    /// A malformed blob falls back to defaults rather than failing to start,
    /// because a server that runs with the wrong port is far easier to diagnose
    /// than one that silently never came up. The error is returned so the caller
    /// can surface it in the editor.
    pub fn parse(value: Option<serde_json::Value>) -> (Self, Option<String>) {
        let Some(value) = value else {
            return (Self::default(), None);
        };
        if value.is_null() {
            return (Self::default(), None);
        }
        match serde_json::from_value(value) {
            Ok(config) => (config, None),
            Err(err) => (Self::default(), Some(err.to_string())),
        }
    }

    /// Compiles [`Self::ignore_files`] into a matcher.
    ///
    /// Invalid globs are skipped individually so that one typo does not disable
    /// filtering altogether and flood the browser with reloads.
    pub fn ignore_set(&self) -> (GlobSet, Vec<String>) {
        let mut builder = GlobSetBuilder::new();
        let mut errors = Vec::new();
        for pattern in &self.ignore_files {
            match Glob::new(pattern) {
                Ok(glob) => {
                    builder.add(glob);
                }
                Err(err) => errors.push(format!("ignore_files: {pattern:?}: {err}")),
            }
        }
        (builder.build().unwrap_or_else(|_| GlobSet::empty()), errors)
    }

    /// Whether the bind address is reachable from other machines.
    pub fn is_public(&self) -> bool {
        matches!(self.host.as_str(), "0.0.0.0" | "::" | "[::]")
    }
}
