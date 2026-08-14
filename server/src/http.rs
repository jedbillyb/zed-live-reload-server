//! The static file server, the WebSocket endpoint and everything in between.

use std::io::SeekFrom;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use percent_encoding::percent_decode_str;
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::broadcast;

use crate::config::{Config, Mount};
use crate::inject::{self, CLIENT_JS, CLIENT_PATH};
use crate::overlay::Overlay;
use crate::reload::Reload;
use crate::ws;

/// Prefix reserved for the reload machinery. A project with a real directory of
/// this name would shadow it, which is why the name is deliberately unlikely.
pub const RESERVED_PREFIX: &str = "/__live_reload/";
const WS_PATH: &str = "/__live_reload/ws";

/// How long an idle keep-alive connection is held open before being dropped.
const KEEP_ALIVE_TIMEOUT: Duration = Duration::from_secs(30);
/// Ceiling on request head size, to bound what a single connection can allocate.
const MAX_HEAD: usize = 32 * 1024;
/// Chunk size used when streaming file bodies.
const CHUNK: usize = 64 * 1024;

/// Everything a connection needs to serve a request.
pub struct Context {
    pub config: Arc<Config>,
    /// Absolute document root, already resolved from the workspace and
    /// `config.root`.
    pub root: PathBuf,
    /// Workspace root, used to resolve relative mount paths.
    pub workspace: PathBuf,
    pub overlay: Overlay,
    pub reload: broadcast::Sender<Reload>,
    /// Signalled when the server is stopping. Connections watch this so that a
    /// browser holding an idle keep-alive socket cannot keep being served after
    /// the user has stopped the server.
    pub shutdown: broadcast::Sender<()>,
}

/// A parsed request head.
struct Request {
    method: String,
    target: String,
    headers: Vec<(String, String)>,
}

impl Request {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    /// Whether this is a WebSocket upgrade request.
    ///
    /// `Connection` is a comma-separated list and may carry other tokens such
    /// as `keep-alive`, so it is checked by token rather than by equality.
    fn is_websocket_upgrade(&self) -> bool {
        let upgrading = self
            .header("connection")
            .map(|value| {
                value
                    .split(',')
                    .any(|token| token.trim().eq_ignore_ascii_case("upgrade"))
            })
            .unwrap_or(false);
        let to_websocket = self
            .header("upgrade")
            .map(|value| value.eq_ignore_ascii_case("websocket"))
            .unwrap_or(false);
        upgrading && to_websocket
    }
}

/// Serves one connection until the peer goes away or an error ends it.
pub async fn serve_connection(mut stream: TcpStream, context: Arc<Context>) {
    // Interactive asset loading is latency sensitive and the payloads are
    // small, so Nagle's algorithm costs more than it saves here.
    let _ = stream.set_nodelay(true);
    let mut shutdown = context.shutdown.subscribe();

    loop {
        let request = tokio::select! {
            // `recv` also resolves, with an error, once the sender is dropped,
            // so a torn-down server closes its connections either way.
            _ = shutdown.recv() => return,
            request = read_request(&mut stream) => match request {
                Some(request) => request,
                None => return,
            },
        };

        if request.target == WS_PATH && request.is_websocket_upgrade() {
            handle_websocket(stream, request, context).await;
            return;
        }

        let keep_alive = request
            .header("connection")
            .map(|value| !value.eq_ignore_ascii_case("close"))
            .unwrap_or(true);

        if handle_request(&mut stream, &request, &context)
            .await
            .is_err()
            || !keep_alive
        {
            return;
        }
    }
}

/* ------------------------------------------------------------------ parsing */

/// Reads and parses a request head, returning `None` when the connection ends.
async fn read_request(stream: &mut TcpStream) -> Option<Request> {
    let mut buffer = Vec::with_capacity(1024);
    let mut chunk = [0u8; 2048];

    loop {
        // The first read waits on an idle keep-alive connection; a browser that
        // has finished with us simply never sends again, and we should not hold
        // the socket forever.
        let read = tokio::time::timeout(KEEP_ALIVE_TIMEOUT, stream.read(&mut chunk))
            .await
            .ok()?
            .ok()?;
        if read == 0 {
            return None;
        }
        buffer.extend_from_slice(&chunk[..read]);

        if buffer.len() > MAX_HEAD {
            return None;
        }

        let mut headers = [httparse::EMPTY_HEADER; 64];
        let mut parsed = httparse::Request::new(&mut headers);
        match parsed.parse(&buffer) {
            Ok(httparse::Status::Complete(_)) => {
                return Some(Request {
                    method: parsed.method?.to_string(),
                    target: parsed.path?.to_string(),
                    headers: parsed
                        .headers
                        .iter()
                        .map(|header| {
                            (
                                header.name.to_string(),
                                String::from_utf8_lossy(header.value).into_owned(),
                            )
                        })
                        .collect(),
                })
            }
            Ok(httparse::Status::Partial) => continue,
            Err(_) => return None,
        }
    }
}

/* --------------------------------------------------------------- websockets */

async fn handle_websocket(mut stream: TcpStream, request: Request, context: Arc<Context>) {
    let Some(key) = request.header("sec-websocket-key") else {
        let _ = stream
            .write_all(b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\n\r\n")
            .await;
        return;
    };

    let response = format!(
        "HTTP/1.1 101 Switching Protocols\r\n\
         Upgrade: websocket\r\n\
         Connection: Upgrade\r\n\
         Sec-WebSocket-Accept: {}\r\n\r\n",
        ws::accept_key(key)
    );
    if stream.write_all(response.as_bytes()).await.is_err() {
        return;
    }

    let mut receiver = context.reload.subscribe();
    let mut shutdown = context.shutdown.subscribe();
    let (mut reader, mut writer) = stream.into_split();

    let hello = ws::text_frame("{\"type\":\"connected\"}");
    if writer.write_all(&hello).await.is_err() {
        return;
    }

    loop {
        tokio::select! {
            _ = shutdown.recv() => return,
            // Reading is not optional even though the client has nothing to
            // say: it is how we notice a closed tab, and how pings get answered.
            incoming = ws::read_frame(&mut reader) => match incoming {
                ws::Incoming::Close => return,
                ws::Incoming::Pong(payload) => {
                    if ws::send_pong(&mut writer, &payload).await.is_err() {
                        return;
                    }
                }
                ws::Incoming::Ignore => {}
            },
            event = receiver.recv() => match event {
                Ok(reload) => {
                    if writer.write_all(&ws::text_frame(&reload.to_message())).await.is_err() {
                        return;
                    }
                }
                // A slow tab that fell behind still wants to be current, and a
                // full reload gets it there in one step.
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    let message = ws::text_frame(&Reload::Full.to_message());
                    if writer.write_all(&message).await.is_err() {
                        return;
                    }
                }
                Err(broadcast::error::RecvError::Closed) => return,
            },
        }
    }
}

/* ------------------------------------------------------------------ routing */

async fn handle_request(
    stream: &mut TcpStream,
    request: &Request,
    context: &Context,
) -> std::io::Result<()> {
    if request.method != "GET" && request.method != "HEAD" {
        return respond(
            stream,
            405,
            "Method Not Allowed",
            "text/plain; charset=utf-8",
            b"Method Not Allowed",
            true,
            context,
            &[("Allow", "GET, HEAD")],
        )
        .await;
    }

    let head_only = request.method == "HEAD";
    let path = request_path(&request.target);

    let Some(path) = path else {
        return respond(
            stream,
            400,
            "Bad Request",
            "text/plain; charset=utf-8",
            b"Bad Request",
            head_only,
            context,
            &[],
        )
        .await;
    };

    if path == CLIENT_PATH {
        return respond(
            stream,
            200,
            "OK",
            "text/javascript; charset=utf-8",
            CLIENT_JS.as_bytes(),
            head_only,
            context,
            &[],
        )
        .await;
    }

    if path.starts_with(RESERVED_PREFIX) {
        return respond(
            stream,
            404,
            "Not Found",
            "text/plain; charset=utf-8",
            b"Not Found",
            head_only,
            context,
            &[],
        )
        .await;
    }

    let Some(target) = resolve(&path, context) else {
        return respond(
            stream,
            403,
            "Forbidden",
            "text/plain; charset=utf-8",
            b"Forbidden",
            head_only,
            context,
            &[],
        )
        .await;
    };

    let metadata = tokio::fs::metadata(&target).await.ok();

    if let Some(metadata) = &metadata {
        if metadata.is_dir() {
            // Without the trailing slash the browser resolves relative links in
            // the index against the parent directory, which quietly breaks the
            // page. A redirect is the only correct fix.
            if !path.ends_with('/') {
                let location = format!("{path}/");
                return respond(
                    stream,
                    301,
                    "Moved Permanently",
                    "text/plain; charset=utf-8",
                    b"",
                    head_only,
                    context,
                    &[("Location", &location)],
                )
                .await;
            }

            let index = target.join(&context.config.index);
            if tokio::fs::metadata(&index).await.is_ok()
                && send_file(stream, &index, request, head_only, context).await?
            {
                return Ok(());
            }

            if context.config.directory_listing {
                let body = directory_listing(&target, &path).await;
                return respond(
                    stream,
                    200,
                    "OK",
                    "text/html; charset=utf-8",
                    body.as_bytes(),
                    head_only,
                    context,
                    &[],
                )
                .await;
            }

            return not_found(stream, request, head_only, context).await;
        }

        if send_file(stream, &target, request, head_only, context).await? {
            return Ok(());
        }
    }

    not_found(stream, request, head_only, context).await
}

/// Serves the SPA fallback if enabled, otherwise a 404.
async fn not_found(
    stream: &mut TcpStream,
    request: &Request,
    head_only: bool,
    context: &Context,
) -> std::io::Result<()> {
    if context.config.spa {
        let index = context.root.join(&context.config.index);
        if tokio::fs::metadata(&index).await.is_ok()
            && send_file(stream, &index, request, head_only, context).await?
        {
            return Ok(());
        }
    }

    let body = b"<!doctype html><meta charset=utf-8><title>404</title><h1>404 Not Found</h1>";
    respond(
        stream,
        404,
        "Not Found",
        "text/html; charset=utf-8",
        body,
        head_only,
        context,
        &[],
    )
    .await
}

/* ----------------------------------------------------------- path resolution */

/// Extracts and decodes the path from a request target.
///
/// Returns `None` for a target that escapes the document root. Normalisation
/// happens on the decoded form so that an encoded `..` is caught too, which is
/// the classic way past a naive check.
pub fn request_path(target: &str) -> Option<String> {
    let without_query = target.split(['?', '#']).next().unwrap_or("");
    let decoded = percent_decode_str(without_query).decode_utf8().ok()?;

    if !decoded.starts_with('/') {
        return None;
    }

    let trailing_slash = decoded.len() > 1 && decoded.ends_with('/');
    let mut segments: Vec<&str> = Vec::new();

    for segment in decoded.split('/') {
        match segment {
            "" | "." => continue,
            ".." => {
                // Popping past the root is an escape attempt, not a path.
                segments.pop()?;
            }
            segment => {
                // A NUL or a backslash in a path segment is never legitimate
                // and both have a history of confusing filesystem layers.
                if segment.contains('\0') || segment.contains('\\') {
                    return None;
                }
                segments.push(segment);
            }
        }
    }

    let mut path = format!("/{}", segments.join("/"));
    if trailing_slash && !path.ends_with('/') {
        path.push('/');
    }
    Some(path)
}

/// Maps a URL path onto a filesystem path, honouring mounts.
fn resolve(path: &str, context: &Context) -> Option<PathBuf> {
    let relative = path.trim_start_matches('/');

    if let Some((mount, remainder)) = longest_mount(&context.config.mount, path) {
        let base = PathBuf::from(&mount.path);
        let base = if base.is_absolute() {
            base
        } else {
            context.workspace.join(base)
        };
        return Some(join_checked(&base, remainder));
    }

    Some(join_checked(&context.root, relative))
}

/// Finds the most specific mount matching `path`, with the remainder after it.
fn longest_mount<'m, 'p>(mounts: &'m [Mount], path: &'p str) -> Option<(&'m Mount, &'p str)> {
    let mut best: Option<(&Mount, usize)> = None;

    for mount in mounts {
        let route = mount.route.trim_end_matches('/');
        if route.is_empty() {
            continue;
        }
        let matches = path == route
            || path
                .strip_prefix(route)
                .is_some_and(|rest| rest.starts_with('/'));
        if matches && best.map(|(_, length)| route.len() > length).unwrap_or(true) {
            best = Some((mount, route.len()));
        }
    }

    best.map(|(mount, length)| (mount, &path[length..]))
}

/// Joins a relative path onto a base, keeping only ordinary components.
///
/// [`request_path`] has already removed traversal, but joining is where a
/// mistake becomes a filesystem read, so the invariant is enforced again here
/// rather than assumed.
fn join_checked(base: &Path, relative: &str) -> PathBuf {
    let mut out = base.to_path_buf();
    for component in Path::new(relative.trim_start_matches('/')).components() {
        if let Component::Normal(part) = component {
            out.push(part);
        }
    }
    out
}

/* ------------------------------------------------------------------ sending */

/// Writes a file as the response.
///
/// Returns `false` without writing anything when the file could not be opened,
/// leaving the caller to decide between the SPA fallback and a 404. Handling
/// that here instead would make this function and `not_found` mutually
/// recursive for no benefit.
async fn send_file(
    stream: &mut TcpStream,
    file: &Path,
    request: &Request,
    head_only: bool,
    context: &Context,
) -> std::io::Result<bool> {
    let mime = mime_guess::from_path(file).first_or_octet_stream();
    let is_html =
        mime.type_() == mime_guess::mime::TEXT && mime.subtype() == mime_guess::mime::HTML;
    let content_type = content_type_for(&mime);

    // The overlay only holds text the editor gave us, so it is consulted for
    // text responses only. This is what makes `live_changes` work.
    if context.config.live_changes {
        if let Some(text) = context.overlay.get(file).await {
            let body = if is_html { inject::inject(&text) } else { text };
            respond(
                stream,
                200,
                "OK",
                &content_type,
                body.as_bytes(),
                head_only,
                context,
                &[],
            )
            .await?;
            return Ok(true);
        }
    }

    if is_html {
        // HTML is rewritten, so it is read whole rather than streamed. Range
        // requests do not apply to a body we generate.
        let Ok(text) = tokio::fs::read(file).await else {
            return Ok(false);
        };
        let body = inject::inject(&String::from_utf8_lossy(&text));
        respond(
            stream,
            200,
            "OK",
            &content_type,
            body.as_bytes(),
            head_only,
            context,
            &[],
        )
        .await?;
        return Ok(true);
    }

    let Ok(mut handle) = tokio::fs::File::open(file).await else {
        return Ok(false);
    };
    let Ok(metadata) = handle.metadata().await else {
        return Ok(false);
    };
    let total = metadata.len();

    // Range support exists mainly for `<video>` and `<audio>`: without it
    // browsers refuse to seek, and some will not play the file at all.
    let range = request
        .header("range")
        .and_then(|value| parse_range(value, total));

    let (status, reason, start, length, extra) = match range {
        Some((start, end)) => {
            let content_range = format!("bytes {start}-{end}/{total}");
            (
                206,
                "Partial Content",
                start,
                end - start + 1,
                Some(content_range),
            )
        }
        None if request.header("range").is_some() => {
            let content_range = format!("bytes */{total}");
            respond(
                stream,
                416,
                "Range Not Satisfiable",
                "text/plain; charset=utf-8",
                b"",
                head_only,
                context,
                &[("Content-Range", &content_range)],
            )
            .await?;
            return Ok(true);
        }
        None => (200, "OK", 0, total, None),
    };

    let mut headers = vec![("Accept-Ranges".to_string(), "bytes".to_string())];
    if let Some(content_range) = extra {
        headers.push(("Content-Range".to_string(), content_range));
    }
    let header_refs: Vec<(&str, &str)> = headers
        .iter()
        .map(|(key, value)| (key.as_str(), value.as_str()))
        .collect();

    write_head(
        stream,
        status,
        reason,
        &content_type,
        length,
        context,
        &header_refs,
    )
    .await?;

    if head_only {
        return Ok(true);
    }

    handle.seek(SeekFrom::Start(start)).await?;
    let mut remaining = length;
    let mut buffer = vec![0u8; CHUNK];
    while remaining > 0 {
        let want = remaining.min(CHUNK as u64) as usize;
        let read = handle.read(&mut buffer[..want]).await?;
        if read == 0 {
            break;
        }
        stream.write_all(&buffer[..read]).await?;
        remaining -= read as u64;
    }

    Ok(true)
}

/// Parses a single-range `Range` header, returning an inclusive byte range.
///
/// Multi-range requests are declined by returning `None`, which makes the
/// caller serve the whole body. That is a legal response and browsers handle
/// it, whereas a wrong multipart body would not be.
fn parse_range(value: &str, total: u64) -> Option<(u64, u64)> {
    let spec = value.strip_prefix("bytes=")?.trim();
    if spec.contains(',') || total == 0 {
        return None;
    }

    let (start, end) = spec.split_once('-')?;

    let (start, end) = match (start.trim(), end.trim()) {
        // `-N`: the final N bytes.
        ("", suffix) => {
            let length: u64 = suffix.parse().ok()?;
            if length == 0 {
                return None;
            }
            (total.saturating_sub(length), total - 1)
        }
        // `N-`: from N to the end.
        (start, "") => (start.parse().ok()?, total - 1),
        (start, end) => (start.parse().ok()?, end.parse::<u64>().ok()?.min(total - 1)),
    };

    if start > end || start >= total {
        return None;
    }
    Some((start, end))
}

fn content_type_for(mime: &mime_guess::Mime) -> String {
    // Browsers guess the encoding of a text response without a charset, and
    // guess wrong on non-ASCII content often enough to be worth being explicit.
    let textual = mime.type_() == mime_guess::mime::TEXT
        || matches!(
            (mime.type_().as_str(), mime.subtype().as_str()),
            ("application", "javascript") | ("application", "json") | ("image", "svg+xml")
        );

    if textual && mime.get_param("charset").is_none() {
        format!("{mime}; charset=utf-8")
    } else {
        mime.to_string()
    }
}

async fn write_head(
    stream: &mut TcpStream,
    status: u16,
    reason: &str,
    content_type: &str,
    length: u64,
    context: &Context,
    extra: &[(&str, &str)],
) -> std::io::Result<()> {
    let mut head = format!(
        "HTTP/1.1 {status} {reason}\r\n\
         Content-Type: {content_type}\r\n\
         Content-Length: {length}\r\n\
         Cache-Control: no-store, no-cache, must-revalidate\r\n"
    );

    if context.config.cors {
        head.push_str("Access-Control-Allow-Origin: *\r\n");
    }
    for (key, value) in extra {
        head.push_str(&format!("{key}: {value}\r\n"));
    }
    head.push_str("\r\n");

    stream.write_all(head.as_bytes()).await
}

#[allow(clippy::too_many_arguments)]
async fn respond(
    stream: &mut TcpStream,
    status: u16,
    reason: &str,
    content_type: &str,
    body: &[u8],
    head_only: bool,
    context: &Context,
    extra: &[(&str, &str)],
) -> std::io::Result<()> {
    write_head(
        stream,
        status,
        reason,
        content_type,
        body.len() as u64,
        context,
        extra,
    )
    .await?;

    if !head_only {
        stream.write_all(body).await?;
    }
    Ok(())
}

/* --------------------------------------------------------- directory listing */

async fn directory_listing(dir: &Path, url_path: &str) -> String {
    let mut directories: Vec<String> = Vec::new();
    let mut files: Vec<(String, u64)> = Vec::new();

    if let Ok(mut entries) = tokio::fs::read_dir(dir).await {
        while let Ok(Some(entry)) = entries.next_entry().await {
            let name = entry.file_name().to_string_lossy().into_owned();
            match entry.metadata().await {
                Ok(metadata) if metadata.is_dir() => directories.push(name),
                Ok(metadata) => files.push((name, metadata.len())),
                Err(_) => continue,
            }
        }
    }

    directories.sort_by_key(|name| name.to_lowercase());
    files.sort_by_key(|(name, _)| name.to_lowercase());

    let mut rows = String::new();
    if url_path != "/" {
        rows.push_str("<li class=\"dir\"><a href=\"..\">../</a></li>");
    }
    for name in &directories {
        rows.push_str(&format!(
            "<li class=\"dir\"><a href=\"{href}/\">{label}/</a></li>",
            href = encode_path_segment(name),
            label = escape_html(name)
        ));
    }
    for (name, size) in &files {
        rows.push_str(&format!(
            "<li><a href=\"{href}\">{label}</a><span>{size}</span></li>",
            href = encode_path_segment(name),
            label = escape_html(name),
            size = human_size(*size)
        ));
    }

    format!(
        "<!doctype html><html><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
         <title>{title}</title><style>\
         :root{{color-scheme:light dark}}\
         body{{font:14px/1.6 ui-sans-serif,system-ui,-apple-system,Segoe UI,sans-serif;\
         margin:0;padding:2rem;max-width:52rem}}\
         h1{{font-size:1rem;font-weight:600;margin:0 0 1rem;font-family:ui-monospace,monospace}}\
         ul{{list-style:none;margin:0;padding:0;border-top:1px solid rgba(128,128,128,.25)}}\
         li{{display:flex;justify-content:space-between;gap:1rem;padding:.35rem .25rem;\
         border-bottom:1px solid rgba(128,128,128,.15)}}\
         a{{text-decoration:none;color:inherit;overflow-wrap:anywhere}}\
         a:hover{{text-decoration:underline}}\
         .dir a{{font-weight:600}}\
         span{{opacity:.55;white-space:nowrap;font-variant-numeric:tabular-nums}}\
         </style></head><body><h1>{title}</h1><ul>{rows}</ul></body></html>",
        title = escape_html(url_path),
        rows = rows
    )
}

fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{size:.1} {}", UNITS[unit])
    }
}

fn escape_html(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Percent-encodes a single path segment for use in an `href`.
fn encode_path_segment(segment: &str) -> String {
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

#[cfg(test)]
mod tests {
    use super::*;

    /* ------------------------------------------------------------ paths */

    #[test]
    fn strips_the_query_and_fragment() {
        assert_eq!(request_path("/a/b.html?x=1").unwrap(), "/a/b.html");
        assert_eq!(request_path("/a/b.html#top").unwrap(), "/a/b.html");
    }

    #[test]
    fn decodes_percent_escapes() {
        assert_eq!(request_path("/my%20file.html").unwrap(), "/my file.html");
        assert_eq!(request_path("/%E6%97%A5.html").unwrap(), "/日.html");
    }

    #[test]
    fn normalises_dot_segments() {
        assert_eq!(request_path("/a/./b/../c.html").unwrap(), "/a/c.html");
    }

    #[test]
    fn preserves_a_trailing_slash() {
        assert_eq!(request_path("/assets/").unwrap(), "/assets/");
        assert_eq!(request_path("/").unwrap(), "/");
    }

    #[test]
    fn rejects_traversal_above_the_root() {
        assert!(request_path("/../etc/passwd").is_none());
        assert!(request_path("/a/../../etc/passwd").is_none());
    }

    #[test]
    fn rejects_percent_encoded_traversal() {
        // The classic bypass: decode first, then normalise.
        assert!(request_path("/%2e%2e/%2e%2e/etc/passwd").is_none());
        assert!(request_path("/a/%2E%2E/%2E%2E/etc/passwd").is_none());
    }

    #[test]
    fn rejects_nul_and_backslash_segments() {
        assert!(request_path("/a%00b").is_none());
        assert!(request_path("/..%5c..%5cwindows").is_none());
    }

    #[test]
    fn rejects_targets_that_are_not_absolute_paths() {
        assert!(request_path("http://evil.test/x").is_none());
    }

    #[test]
    fn join_checked_drops_any_surviving_traversal() {
        let joined = join_checked(Path::new("/srv/site"), "../../etc/passwd");
        assert_eq!(joined, PathBuf::from("/srv/site/etc/passwd"));
    }

    /* ----------------------------------------------------------- mounts */

    fn mounts() -> Vec<Mount> {
        vec![
            Mount {
                route: "/lib".into(),
                path: "node_modules".into(),
            },
            Mount {
                route: "/lib/vendor".into(),
                path: "vendor".into(),
            },
        ]
    }

    #[test]
    fn picks_the_most_specific_mount() {
        let mounts = mounts();
        let (mount, rest) = longest_mount(&mounts, "/lib/vendor/x.js").unwrap();
        assert_eq!(mount.path, "vendor");
        assert_eq!(rest, "/x.js");
    }

    #[test]
    fn matches_a_mount_on_segment_boundaries_only() {
        let mounts = mounts();
        // `/library` must not be captured by the `/lib` mount.
        assert!(longest_mount(&mounts, "/library/x.js").is_none());
        assert!(longest_mount(&mounts, "/lib").is_some());
        assert!(longest_mount(&mounts, "/lib/x.js").is_some());
    }

    /* ------------------------------------------------------------ range */

    #[test]
    fn parses_a_closed_range() {
        assert_eq!(parse_range("bytes=0-99", 1000), Some((0, 99)));
    }

    #[test]
    fn parses_an_open_ended_range() {
        assert_eq!(parse_range("bytes=500-", 1000), Some((500, 999)));
    }

    #[test]
    fn parses_a_suffix_range() {
        assert_eq!(parse_range("bytes=-100", 1000), Some((900, 999)));
    }

    #[test]
    fn clamps_an_end_past_the_file_length() {
        assert_eq!(parse_range("bytes=0-9999", 1000), Some((0, 999)));
    }

    #[test]
    fn declines_unsatisfiable_and_multi_ranges() {
        assert_eq!(parse_range("bytes=1000-1001", 1000), None);
        assert_eq!(parse_range("bytes=0-1,5-6", 1000), None);
        assert_eq!(parse_range("chunks=0-1", 1000), None);
        assert_eq!(parse_range("bytes=0-0", 0), None);
    }

    /* ---------------------------------------------------------- headers */

    #[test]
    fn adds_a_charset_to_text_responses() {
        let css = mime_guess::from_path("a.css").first_or_octet_stream();
        assert_eq!(content_type_for(&css), "text/css; charset=utf-8");
    }

    #[test]
    fn leaves_binary_types_alone() {
        let png = mime_guess::from_path("a.png").first_or_octet_stream();
        assert_eq!(content_type_for(&png), "image/png");
    }

    #[test]
    fn detects_a_websocket_upgrade_among_other_connection_tokens() {
        let request = Request {
            method: "GET".into(),
            target: WS_PATH.into(),
            headers: vec![
                ("Connection".into(), "keep-alive, Upgrade".into()),
                ("Upgrade".into(), "websocket".into()),
            ],
        };
        assert!(request.is_websocket_upgrade());
    }

    #[test]
    fn does_not_mistake_a_plain_request_for_an_upgrade() {
        let request = Request {
            method: "GET".into(),
            target: "/".into(),
            headers: vec![("Connection".into(), "keep-alive".into())],
        };
        assert!(!request.is_websocket_upgrade());
    }

    /* ----------------------------------------------------------- markup */

    #[test]
    fn escapes_entries_in_the_directory_listing() {
        assert_eq!(
            escape_html("<script>&\"x\""),
            "&lt;script&gt;&amp;&quot;x&quot;"
        );
    }

    #[test]
    fn encodes_listing_hrefs() {
        assert_eq!(encode_path_segment("my file.html"), "my%20file.html");
        assert_eq!(encode_path_segment("a&b.txt"), "a%26b.txt");
    }

    #[test]
    fn formats_sizes() {
        assert_eq!(human_size(0), "0 B");
        assert_eq!(human_size(999), "999 B");
        assert_eq!(human_size(1536), "1.5 KB");
    }
}
