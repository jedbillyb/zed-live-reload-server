# Live Reload for Zed

A local development server with live reload, in the spirit of the
[VS Code Live Server extension][vscode-live-server].

Edit a file, and the page in your browser updates.

- **Registry id:** `live-reload`
- **Binary:** `live-reload-lsp`

## Features

- Start and stop on demand, or serve automatically when a project opens
- CSS and images hot swap in place; everything else full reloads and restores
  scroll position
- Watches the filesystem, so Sass, bundlers and `git checkout` reload too
- Reports the port it actually bound, scanning upward if the preferred one is taken
- `Range` requests, so `<video>` seeks
- SPA fallback, CORS, extra mounts, directory listings
- Serve to a phone on the same network with `"host": "0.0.0.0"`
- Optional beta: update the page as you type, without saving

## Install

Not yet in the Zed extension registry. To install as a dev extension:

```
git clone https://github.com/jedbillyb/zed-live-server
```

In Zed: **zed: extensions** → **Install Dev Extension** → pick the cloned
directory. The matching `live-reload-lsp` binary is downloaded from this
repository's releases on first use.

## Usage

Nothing serves until you ask it to. While the server is up, its address shows in
Zed's status bar as `Live Reload :5500`. Set `"auto_start": true` to have it
running from the moment a project opens instead.

### One keystroke

Copy [`.zed/tasks.json`](.zed/tasks.json) into your project, or into
`~/.config/zed/tasks.json` for all projects, then bind it:

```json
// ~/.config/zed/keymap.json
[
  {
    "context": "Workspace",
    "bindings": {
      "alt-s": ["task::Spawn", { "task_name": "Live Reload: go" }]
    }
  }
]
```

Press it to start the server and open the page in your default browser. Press it
again to stop.

Any key works, but check that the one you pick is genuinely free. Zed's default
keymap binds most `alt-` chords already, and **a default binding in a deeper
context beats yours in a shallower one**, silently. `alt-1` is the trap: it looks
unused, but it is `pane::ActivateItem` in the `Pane` context, which sits inside
`Workspace`, so it switches tabs and your task never runs. As of Zed 1.15 these
are unbound on Linux:

```
alt-            a e g h i m n o p q s u v x z
alt-shift-      b c d e f g h j k m n p s v w 0-9
ctrl-alt-       m q u v w 1-9
```

Other tasks, if you want them on separate keys:

| Task | Does |
|---|---|
| `Live Reload: go` | Start and open a browser, or stop if running |
| `Live Reload: toggle` | Same, without the browser |
| `Live Reload: open browser` | Open the site, starting the server if needed |
| `Live Reload: status` | Print the current state |
| `Live Reload: stop` | Stop the server |

These drive the server the extension is already running, so they use your
settings. They do not start a second server.

### Code actions

Available with no setup. Right-click in the editor and choose **Code Actions**:

- *Live Reload: open this file in the browser (:5500)*
- *Live Reload: restart server (:5500)*
- *Live Reload: stop server (:5500)*

When the server is stopped the only offer is *start server*.

### From a terminal

```sh
live-reload-lsp go        # in the project directory: start and open, or stop
live-reload-lsp toggle    # same, without the browser
live-reload-lsp status
live-reload-lsp stop
```

## Settings

Configure under `lsp.live-reload` in Zed's `settings.json`. Names follow the VS
Code Live Server options where an equivalent exists, in snake_case.

```json
{
  "lsp": {
    "live-reload": {
      "initialization_options": {
        "port": 5500,
        "open_browser": false,
        "root": "/dist"
      }
    }
  }
}
```

| Setting | Default | What it does |
|---|---|---|
| `port` | `5500` | Preferred port. If taken, scans upward up to 50 ports. `0` picks any free port. |
| `host` | `"127.0.0.1"` | Interface to bind. `"0.0.0.0"` to reach the server from another device. |
| `root` | `"/"` | Document root, relative to the workspace. For example `"/dist"`. |
| `auto_start` | `false` | Start serving as soon as the project opens, rather than waiting to be asked. |
| `open_browser` | `false` | Open a browser when the server starts. |
| `browser` | `null` | Browser command, with arguments if you like. `null` uses the system default. |
| `status_bar` | `true` | Show the address in Zed's status bar. Turn off if the spinner beside it bothers you. |
| `index` | `"index.html"` | File served for directory requests. |
| `spa` | `false` | Serve `index` for unknown paths instead of a 404, for client-side routers. |
| `cors` | `false` | Send `Access-Control-Allow-Origin: *`. |
| `directory_listing` | `true` | Browsable listing when a directory has no index file. |
| `wait` | `100` | Milliseconds to coalesce rapid changes into one reload. |
| `full_reload` | `false` | Always reload the whole page, never hot swap. |
| `ignore_files` | see below | Glob patterns whose changes never trigger a reload. |
| `mount` | `[]` | Extra directories served outside the document root, as `[{ "route": "/lib", "path": "node_modules" }]`. |
| `live_changes` | `false` | Beta. Serve unsaved editor buffers. See below. |

Settings can go in `initialization_options` or `settings`; both are read, and
`settings` wins if you use both. Changing them restarts any running server.

Default `ignore_files`: `.git`, `node_modules`, `target`, `.DS_Store`, `*.log`,
`*.swp`, `*~`, `.#*`. Ignoring a directory ignores everything under it, whether
you write `**/dist` or `**/dist/**`.

## Live changes without saving (beta)

Off by default. Turn it on with `"live_changes": true`, and the page updates as
you type. The editor sends each change over LSP, the server holds that text in
memory and serves it in place of the file on disk.

- Only files open in the editor are served from memory. Everything else comes
  from disk.
- Saving drops the overlay, and the file on disk takes over again.
- Changes are coalesced over `wait` ms, so a burst of typing is one reload.
- Editing HTML means a full page reload on every pause. Fine for CSS work, less
  so on a page with heavy JavaScript state. `full_reload` and `wait` are the knobs.

The VS Code Live Server does not do this; it only watches the filesystem.
Microsoft's separate Live Preview does, but inside a webview it controls. Zed
extensions have no webview, so this serves the buffer over HTTP to a real browser.

## Without Zed

The same binary is a standalone server:

```
live-reload-lsp serve ./public --port 3000
live-reload-lsp serve --host 0.0.0.0 --no-browser
live-reload-lsp serve --spa
live-reload-lsp --help
```

With no arguments it speaks LSP on stdio, which is how Zed starts it.

## FAQ

### Why is there no clickable status bar button?

Zed extensions cannot add one. The extension API (`zed_extension_api` 0.7)
offers language servers, slash commands, themes, context servers, debug
adapters, snippets and docs, and no UI extension point at all. The other items
in Zed's status bar are Zed's own code, not extensions.

This is not something an extension can ship for itself. An extension is a WASM
guest whose entire host surface is fixed when Zed is compiled: settings, file
download, HTTP, processes, and the language server, slash command, theme,
context server, debug adapter, snippet and docs hooks. Nothing draws.

[Zed discussion #53403][rfc] proposes a Visual Extension API with a status bar
API as its first phase. When that ships, wiring a button to the existing
start/stop commands is a small change.

Until then the status bar narrates the same states the VS Code button does,
driven by a keybinding instead of a click:

| VS Code button | Here |
|---|---|
| `Go Live` | (nothing shown) |
| `Starting...` | `Live Reload: starting…` |
| `Port: 5500` | `Live Reload :5500` |
| `Disposing...` | `Live Reload: disposing…` |

### Why does the status bar show a spinner?

LSP progress is the only way an extension can put text in Zed's status bar, and
Zed draws every progress item with a loading spinner:

```rust
// crates/activity_indicator/src/activity_indicator.rs
return Some(Content {
    icon: ActivityIcon::LoadingSpinner,
    message,
    on_click: None,
    ...
```

There is no field in the protocol that can ask for a different icon, and ending
the progress removes the text along with the spinner. So the two come together
or not at all. Nothing is stuck, and the spinner is not reporting that the
server is busy.

Set `"status_bar": false` to have neither.

### Why is this a language server?

Zed extensions run as sandboxed WASM. They cannot open sockets, watch the
filesystem or spawn long-lived processes. Declaring a language server is the one
supported way to get a native process with the lifetime of the project. The
extension is a thin shim that locates `live-reload-lsp` and passes it your
settings; the binary does the work.

### How does the reload work?

The server injects a script before `</body>` in every HTML response, which opens
a WebSocket back to it. On a change it picks the cheapest update: swap a
stylesheet in place (loading the new one alongside the old, so there is no flash
of unstyled content), re-fetch an image, or full reload with scroll position
saved and restored. A batch containing any full reload collapses to one reload.

The trigger is the filesystem, not editor save events, so changes from other
programs reload the page and a single save cannot fire twice.

### Why not use the existing Zed Live Server extension?

[frederik-uni/zed-live-server][frederik] has some significant gaps:

| | That one | This one |
|---|---|---|
| `/live-server-start` and `/live-server-stop` | Print a hardcoded string and do nothing | Real code actions |
| Reported port | Hardcoded `57391`, regardless of the real port | The actual bound port |
| Starting and stopping | Not possible; runs from the moment you open an HTML file | Start, stop and restart on demand |
| Documented `lazy` / `public` / `start_port` | Never wired up | Full settings, applied live |
| Port already in use | Unbounded retry loop, never tells you where it landed | Scans upward, reports the port |
| External file changes | Not noticed; only editor events trigger reloads | Watches the filesystem |
| Reload strategy | CSS updates a style tag, everything else full reloads | CSS and images hot swap, full reloads restore scroll |
| Ranged requests | Not supported | Supported |

## Building from source

Requires a Rust toolchain.

```bash
cd server && cargo build --release && cargo test

rustup target add wasm32-wasip2
cargo build --target wasm32-wasip2 --release
```

To use a local server build:

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

## Licence

MIT. See [LICENSE](LICENSE).

Credit to [ritwickdey/vscode-live-server][vscode-live-server] (MIT) for the
original design and the option vocabulary, and to
[frederik-uni/zed-live-server][frederik] (MIT) for establishing that a language
server is the way to run a development server from a Zed extension. No code was
taken from either; this is an independent implementation in Rust and is not
affiliated with either project.

[vscode-live-server]: https://github.com/ritwickdey/vscode-live-server
[frederik]: https://github.com/frederik-uni/zed-live-server
[rfc]: https://github.com/zed-industries/zed/discussions/53403
