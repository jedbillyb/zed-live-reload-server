//! Zed extension shim for Live Reload.
//!
//! Zed extensions run as sandboxed WASM and cannot open sockets, watch the
//! filesystem or spawn long-lived processes. Registering a language server is
//! the one supported way to get a native process with the lifetime of the
//! project, so the actual development server lives in `live-reload-server-lsp` and
//! this crate exists only to locate that binary and hand it the user's config.

use std::fs;

use zed_extension_api::{
    self as zed, serde_json, settings::LspSettings, LanguageServerId, Result, Worktree,
};

/// Name of the language server as declared in `extension.toml`.
const SERVER_ID: &str = "live-reload-server";
/// Binary published by this repository's release workflow.
const BINARY_NAME: &str = "live-reload-server-lsp";
const REPO: &str = "jedbillyb/zed-live-reload-server";

struct LiveReloadExtension {
    cached_binary_path: Option<String>,
}

impl LiveReloadExtension {
    /// Resolves the server binary, preferring anything the user has pointed us
    /// at over a downloaded copy so that working on the server itself does not
    /// require cutting a release.
    fn binary_path(
        &mut self,
        language_server_id: &LanguageServerId,
        worktree: &Worktree,
    ) -> Result<String> {
        if let Some(path) = LspSettings::for_worktree(SERVER_ID, worktree)
            .ok()
            .and_then(|settings| settings.binary)
            .and_then(|binary| binary.path)
        {
            return Ok(path);
        }

        if let Some(path) = worktree.which(BINARY_NAME) {
            return Ok(path);
        }

        if let Some(path) = &self.cached_binary_path {
            if fs::metadata(path).map_or(false, |stat| stat.is_file()) {
                return Ok(path.clone());
            }
        }

        let path = self.download_binary(language_server_id)?;
        self.cached_binary_path = Some(path.clone());
        Ok(path)
    }

    fn download_binary(&self, language_server_id: &LanguageServerId) -> Result<String> {
        zed::set_language_server_installation_status(
            language_server_id,
            &zed::LanguageServerInstallationStatus::CheckingForUpdate,
        );

        let release = zed::latest_github_release(
            REPO,
            zed::GithubReleaseOptions {
                require_assets: true,
                pre_release: false,
            },
        )?;

        let (platform, arch) = zed::current_platform();
        let arch = match arch {
            zed::Architecture::Aarch64 => "aarch64",
            zed::Architecture::X86 => "x86",
            zed::Architecture::X8664 => "x86_64",
        };
        let os = match platform {
            zed::Os::Mac => "apple-darwin",
            zed::Os::Linux => "unknown-linux-musl",
            zed::Os::Windows => "pc-windows-msvc",
        };

        // Assets are gzipped tarballs containing a single executable, which
        // keeps the release roughly a quarter of the size of a raw binary.
        let asset_name = format!("{BINARY_NAME}-{arch}-{os}.tar.gz");
        let asset = release
            .assets
            .iter()
            .find(|asset| asset.name == asset_name)
            .ok_or_else(|| format!("no release asset found matching {asset_name:?}"))?;

        let version_dir = format!("{BINARY_NAME}-{}", release.version);
        let binary_path = match platform {
            zed::Os::Windows => format!("{version_dir}/{BINARY_NAME}.exe"),
            zed::Os::Mac | zed::Os::Linux => format!("{version_dir}/{BINARY_NAME}"),
        };

        if !fs::metadata(&binary_path).map_or(false, |stat| stat.is_file()) {
            zed::set_language_server_installation_status(
                language_server_id,
                &zed::LanguageServerInstallationStatus::Downloading,
            );

            fs::create_dir_all(&version_dir)
                .map_err(|err| format!("failed to create directory '{version_dir}': {err}"))?;

            zed::download_file(
                &asset.download_url,
                &version_dir,
                zed::DownloadedFileType::GzipTar,
            )
            .map_err(|err| format!("failed to download {asset_name}: {err}"))?;

            zed::make_file_executable(&binary_path)?;
            remove_stale_versions(&version_dir);
        }

        Ok(binary_path)
    }
}

/// Deletes previously downloaded versions, keeping only `keep`.
///
/// Failures here are not fatal: a stale directory wastes disk but does not stop
/// the server from starting, so a warning would be more disruptive than useful.
fn remove_stale_versions(keep: &str) {
    let Ok(entries) = fs::read_dir(".") else {
        return;
    };
    for entry in entries.flatten() {
        if entry.file_name().to_str() != Some(keep) {
            fs::remove_dir_all(entry.path()).ok();
        }
    }
}

/// Reads the user's configuration for this server.
///
/// Zed exposes two slots for a language server, `initialization_options` and
/// `settings`. We accept the config in either one so users do not have to care
/// about the distinction, with `settings` winning when both are present since
/// that is the slot Zed can push updates through without a restart.
fn user_config(worktree: &Worktree) -> Option<serde_json::Value> {
    let settings = LspSettings::for_worktree(SERVER_ID, worktree).ok()?;
    settings.settings.or(settings.initialization_options)
}

impl zed::Extension for LiveReloadExtension {
    fn new() -> Self {
        Self {
            cached_binary_path: None,
        }
    }

    fn language_server_command(
        &mut self,
        language_server_id: &LanguageServerId,
        worktree: &Worktree,
    ) -> Result<zed::Command> {
        // Configuration travels over LSP rather than argv, so that changing a
        // setting does not require a different process invocation.
        let args = LspSettings::for_worktree(SERVER_ID, worktree)
            .ok()
            .and_then(|settings| settings.binary)
            .and_then(|binary| binary.arguments)
            .unwrap_or_default();

        Ok(zed::Command {
            command: self.binary_path(language_server_id, worktree)?,
            args,
            env: worktree.shell_env(),
        })
    }

    fn language_server_initialization_options(
        &mut self,
        _language_server_id: &LanguageServerId,
        worktree: &Worktree,
    ) -> Result<Option<serde_json::Value>> {
        Ok(user_config(worktree))
    }

    fn language_server_workspace_configuration(
        &mut self,
        _language_server_id: &LanguageServerId,
        worktree: &Worktree,
    ) -> Result<Option<serde_json::Value>> {
        Ok(user_config(worktree))
    }
}

zed::register_extension!(LiveReloadExtension);
