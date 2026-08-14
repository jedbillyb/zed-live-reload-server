# Live Reload for Zed

A local development server with live reload, in the spirit of the
[VS Code Live Server extension][vscode-live-server].

Edit a file, and the page in your browser updates. Stylesheets and images are
swapped in place without losing scroll position or page state; everything else
does a full reload that restores your scroll position afterwards.

- **Registry id:** `live-reload`
- **Repository:** `jedbillyb/zed-live-server`

---

## Why this exists

There is already a [Live Server extension for Zed][frederik]. This one was
written because that one has some significant gaps:

| | That one | This one |
|---|---|---|
| `/live-server-start` and `/live-server-stop` | Print a hardcoded string and do nothing | No fake commands. Start and stop are real code actions |
| Reported port | Hardcoded `57391`, regardless of the real port | Always the actual bound port |
| Starting and stopping | Not possible. The server runs from the moment you open an HTML file | Start, stop and restart on demand, plus `auto_start` if you want the old behaviour |
| Documented `lazy` / `public` / `start_port` settings | Never wired up. The extension passes a fixed `--eager` and no init options | Full settings, applied live |
| Port already in use | Retries in an unbounded loop with no backoff, and never tells you where it landed | Scans upward, then reports the port it got |
| External file changes | Not noticed. Only editor events trigger reloads | Watches the filesystem, so Sass, bundlers and `git checkout` reload too |
| Reload strategy | CSS updates a style tag, everything else full reloads | CSS and images hot swap, full reloads restore scroll position |
| Ranged requests | Not supported, so `<video>` will not seek | `Range` supported |

## Install

Not yet in the Zed extension registry. To install as a dev extension:

```
git clone https://github.com/jedbillyb/zed-live-server
```

Then in Zed: **zed: extensions** → **Install Dev Extension** → pick the cloned
directory.

The extension downloads the matching `live-reload-lsp` binary from this
repository's releases on first use. To use your own build instead, see
[Building from source](#building-from-source).

## Using it

The server starts automatically when you open a project (set `auto_start` to
`false` to change that) and prints its address in a notification.

Everything else is a **code action** on the file you are looking at:

- *Live Reload: open this file in the browser (:5500)*
- *Live Reload: restart server (:5500)*
- *Live Reload: stop server (:5500)*
- *Live Reload: start server*, when it is not running

Right-click in the editor and choose **Code Actions**, or bind a key:

```json
// ~/.config/zed/keymap.json
[
  {
    "context": "Editor",
    "bindings": { "cmd-alt-l": "editor::ToggleCodeActions" }
  }
]
```

The list only ever offers actions that apply, so you never get a *stop* on a
server that is not running.

### About the status bar

Zed extensions **cannot add status bar buttons**. The extension API
(`zed_extension_api` 0.7) exposes language servers, slash commands, themes,
context servers, debug adapters, snippets and docs, and no UI extension point at
all. There is no way for any extension to put a "Go Live" button down there, so
this one does the two things that are possible:

1. While a server is running it reports LSP progress titled `Live Reload :5500`,
   which Zed shows in its language server status area. That is the closest thing
   to a live status indicator available.
2. The code actions above, which are one keystroke away once bound.

## Settings

Configure under `lsp.live-reload` in Zed's `settings.json`. Names follow the VS
Code Live Server options where an equivalent exists, in snake_case.

```json
{
  "lsp": {
    "live-reload": {
      "initialization_options": {
        "port": 5500,
        "host": "127.0.0.1",
        "root": "/",
        "auto_start": true,
        "open_browser": true,
        "browser": null,
        "index": "index.html",
        "spa": false,
        "cors": false,
        "directory_listing": true,
        "wait": 100,
        "full_reload": false,
        "ignore_files": ["**/node_modules/**", "**/*.log"],
        "mount": [{ "route": "/lib", "path": "node_modules" }],
        "live_changes": false
      }
    }
  }
}
```

| Setting | Default | What it does |
|---|---|---|
| `port` | `5500` | Preferred port. If taken, scans upward up to 50 ports. `0` picks any free port. |
| `host` | `"127.0.0.1"` | Interface to bind. Use `"0.0.0.0"` to reach the server from a phone on the same network. |
| `root` | `"/"` | Document root, relative to the workspace. For example `"/dist"`. |
| `auto_start` | `true` | Start serving as soon as the project opens. |
| `open_browser` | `true` | Open a browser when the server starts. |
| `browser` | `null` | Browser command, with arguments if you like. `null` uses the system default. |
| `index` | `"index.html"` | File served for directory requests. |
| `spa` | `false` | Serve `index` for unknown paths instead of a 404, for client-side routers. |
| `cors` | `false` | Send `Access-Control-Allow-Origin: *`. |
| `directory_listing` | `true` | Browsable listing when a directory has no index file. |
| `wait` | `100` | Milliseconds to coalesce rapid changes into one reload. |
| `full_reload` | `false` | Always reload the whole page, never hot swap. |
| `ignore_files` | see below | Glob patterns whose changes never trigger a reload. |
| `mount` | `[]` | Extra directories served outside the document root. |
| `live_changes` | `false` | **Beta.** Serve unsaved editor buffers. See below. |

Settings can go in `initialization_options` or `settings`; both are read, and
`settings` wins if you use both. Changing them restarts any running server.

Default `ignore_files`: `.git`, `node_modules`, `target`, `.DS_Store`, `*.log`,
`*.swp`, `*~`, `.#*`. Ignoring a directory ignores everything under it, whether
you write `**/dist` or `**/dist/**`.

## Live changes without saving (beta)

Off by default. Turn it on with `"live_changes": true`.

With it on, the page updates **as you type**, without saving. The editor sends
each keystroke over LSP, the server keeps that text in memory, and serves it in
place of the file on disk.

Worth knowing before you turn it on:

- **This is not how the VS Code Live Server works.** That extension only ever
  watches the filesystem, so it reloads on save and nothing else. Microsoft's
  separate *Live Preview* extension does update as you type, but only inside a
  webview it controls. Zed extensions have no webview, so this takes a different
  route: the buffer is served over HTTP to a real browser.
- Only files **open in the editor** are served from memory. Everything else
  comes from disk as usual.
- Once you save, the overlay is dropped and the file on disk takes over again.
- Changes are coalesced over `wait` milliseconds, so a burst of typing produces
  one reload rather than one per character.
- Editing HTML means a full page reload on every pause in typing. If you are
  working on CSS this feels great; on a heavy page with lots of JavaScript state,
  it may not. `full_reload` and `wait` are the knobs.

## Running it without Zed

The same binary is a standalone server:

```
live-reload-lsp serve ./public --port 3000
live-reload-lsp serve --host 0.0.0.0 --no-browser
live-reload-lsp serve --spa            # for client-side routers
live-reload-lsp --help
```

With no arguments it speaks LSP on stdio, which is how Zed starts it.

## Building from source

Requires a Rust toolchain.

```bash
# The server binary
cd server && cargo build --release && cargo test

# The extension (WASM)
rustup target add wasm32-wasip2
cargo build --target wasm32-wasip2 --release
```

To point the extension at a local server build instead of a released one:

```json
{
  "lsp": {
    "live-reload": {
      "binary": { "path": "/path/to/zed-live-server/server/target/release/live-reload-lsp" }
    }
  }
}
```

`live-reload-lsp` on your `PATH` is also picked up automatically, ahead of any
downloaded copy.

## How it works

Zed extensions run as sandboxed WASM. They cannot open sockets, watch the
filesystem or spawn long-lived processes, and there is no API for registering a
command or a button. Declaring a **language server** is the one supported way to
get a native process with the lifetime of the project, so that is what this is:
the extension is a thin shim that locates `live-reload-lsp` and passes it your
settings, and the binary does the real work.

The server injects a small script before `</body>` in every HTML response, which
opens a WebSocket back to it. When a file changes, the server decides the
cheapest update that will pick it up:

- a stylesheet is swapped in place, loading the new one alongside the old so
  there is no flash of unstyled content
- an image is re-fetched in place
- anything else is a full reload, with scroll position saved and restored

A batch of changes that includes any full reload collapses to a single reload,
so a build that rewrites markup and CSS together loads the page once.

The filesystem is the trigger, not editor save events. That means changes from
Sass, bundlers, `git checkout` and other programs all reload the page, and a
single save cannot fire twice.

## Credits and licence

MIT. See [LICENSE](LICENSE).

- [ritwickdey/vscode-live-server][vscode-live-server] (MIT) for the original
  design and the option vocabulary. No code was taken from it; this is an
  independent implementation in Rust.
- [frederik-uni/zed-live-server][frederik] (MIT) for establishing that a
  language server is the way to run a development server from a Zed extension.

This is an independent extension and is not affiliated with either project.

[vscode-live-server]: https://github.com/ritwickdey/vscode-live-server
[frederik]: https://github.com/frederik-uni/zed-live-server
