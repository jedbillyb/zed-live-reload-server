//! `live-reload-lsp`, the native half of the Zed Live Reload extension.
//!
//! Normally started by the editor and spoken to over LSP on stdio. It can also
//! be run directly as a plain development server, which is useful for testing
//! and for anyone who wants the server without the editor.

mod config;
mod control;
mod http;
mod inject;
mod lsp;
mod overlay;
mod reload;
mod server;
mod watch;
mod ws;

use std::path::PathBuf;
use std::sync::Arc;

use config::Config;
use control::Command as ControlCommand;
use overlay::Overlay;
use server::{open_browser, LiveServer};
use tower_lsp::{LspService, Server};

const USAGE: &str = "\
live-reload-lsp - development server with live reload

USAGE:
    live-reload-lsp [--stdio]        Speak LSP on stdio (the default; used by Zed)
    live-reload-lsp serve [DIR]      Serve a directory directly, without an editor
    live-reload-lsp --version
    live-reload-lsp --help

CONTROLLING THE EDITOR'S SERVER:
    live-reload-lsp go [DIR]         Start and open a browser, or stop if running
    live-reload-lsp toggle [DIR]     Start it if stopped, stop it if running
    live-reload-lsp start [DIR]
    live-reload-lsp stop [DIR]
    live-reload-lsp open [DIR]       Open the site, starting the server if needed
    live-reload-lsp status [DIR]

    These drive the server the editor is already running, so they respect your
    editor settings. DIR defaults to the current directory and must be the root
    of a project open in the editor. Bind one to a key with a Zed task; see the
    README.

SERVE OPTIONS:
    --port <PORT>     Preferred port, scanning upward if taken (default 5500)
    --host <HOST>     Interface to bind (default 127.0.0.1, use 0.0.0.0 for LAN)
    --no-browser      Do not open a browser on start
    --spa             Serve index.html for unknown paths
    --cors            Send permissive CORS headers

Configuration under the editor is read from Zed's settings, not from these
flags. See the README for the full list of options.";

#[tokio::main]
async fn main() {
    let arguments: Vec<String> = std::env::args().skip(1).collect();

    match arguments.first().map(String::as_str) {
        Some("--help" | "-h") => println!("{USAGE}"),
        Some("--version" | "-V") => println!("live-reload-lsp {}", env!("CARGO_PKG_VERSION")),
        Some("serve") => serve(&arguments[1..]).await,
        Some(word) if ControlCommand::parse(word).is_some() => {
            // Unwrap is sound: the guard above already parsed it.
            control_command(ControlCommand::parse(word).unwrap(), arguments.get(1))
        }
        _ => run_lsp().await,
    }
}

async fn run_lsp() {
    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();
    let (service, socket) = LspService::build(lsp::Backend::new).finish();
    Server::new(stdin, stdout, socket).serve(service).await;
}

/// Standalone mode. Errors exit non-zero with a message on stderr, since here
/// there is no editor to show a notification in.
async fn serve(arguments: &[String]) {
    let mut config = Config::default();
    let mut directory = PathBuf::from(".");
    let mut index = 0;

    while index < arguments.len() {
        match arguments[index].as_str() {
            "--port" => {
                index += 1;
                match arguments.get(index).and_then(|value| value.parse().ok()) {
                    Some(port) => config.port = port,
                    None => fail("--port needs a number"),
                }
            }
            "--host" => {
                index += 1;
                match arguments.get(index) {
                    Some(host) => config.host = host.clone(),
                    None => fail("--host needs a value"),
                }
            }
            "--no-browser" => config.open_browser = false,
            "--spa" => config.spa = true,
            "--cors" => config.cors = true,
            value if value.starts_with('-') => fail(&format!("unknown option: {value}")),
            value => directory = PathBuf::from(value),
        }
        index += 1;
    }

    let directory = match directory.canonicalize() {
        Ok(directory) => directory,
        Err(err) => return fail(&format!("{}: {err}", directory.display())),
    };

    let config = Arc::new(config);
    let server = LiveServer::new(directory.clone(), Overlay::default());

    match server.start(config.clone()).await {
        Ok((port, warnings)) => {
            for warning in warnings {
                eprintln!("warning: {warning}");
            }
            let url = format!("http://{}:{port}/", displayable_host(&config.host));
            println!("Serving {} at {url}", directory.display());
            println!("Press Ctrl+C to stop.");

            if config.open_browser {
                if let Err(err) = open_browser(&url, config.browser.as_deref()) {
                    eprintln!("warning: {err}");
                }
            }
        }
        Err(err) => return fail(&err),
    }

    // Nothing else to do on the main task; the accept loop owns the work.
    std::future::pending::<()>().await;
}

/// Sends a command to the language server the editor is running.
fn control_command(command: ControlCommand, directory: Option<&String>) {
    let directory = directory
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));

    match control::send(&directory, command) {
        Ok(message) => println!("{message}"),
        Err(message) => fail(&message),
    }
}

/// A bind address is not always something you can point a browser at.
fn displayable_host(host: &str) -> &str {
    match host {
        "0.0.0.0" => "127.0.0.1",
        "::" | "[::]" => "[::1]",
        other => other,
    }
}

fn fail(message: &str) {
    eprintln!("live-reload-lsp: {message}");
    std::process::exit(1);
}
