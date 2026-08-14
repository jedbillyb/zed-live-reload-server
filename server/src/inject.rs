//! Injection of the reload client into HTML responses.

/// The browser-side client, compiled into the binary so there is nothing to
/// install or serve from disk.
pub const CLIENT_JS: &str = include_str!("client.js");

/// Path the client script is served from.
pub const CLIENT_PATH: &str = "/__live_reload/client.js";

/// Inserts the client `<script>` into an HTML document.
///
/// Placement is last closing `</body>`, then last closing `</html>`, then the
/// end of the document. Searching from the end matters because those strings
/// legitimately appear earlier inside `<pre>` blocks, escaped examples and
/// inline scripts, and injecting into one of those would corrupt the page.
///
/// Documents without either tag still get the script appended: browsers parse
/// them fine, and a page that silently does not reload is a worse failure than
/// a script tag outside a body.
pub fn inject(html: &str) -> String {
    let script = format!("<script src=\"{CLIENT_PATH}\" defer></script>");

    if let Some(index) = find_last_tag(html, "</body>") {
        let mut out = String::with_capacity(html.len() + script.len() + 1);
        out.push_str(&html[..index]);
        out.push_str(&script);
        out.push('\n');
        out.push_str(&html[index..]);
        return out;
    }

    if let Some(index) = find_last_tag(html, "</html>") {
        let mut out = String::with_capacity(html.len() + script.len() + 1);
        out.push_str(&html[..index]);
        out.push_str(&script);
        out.push('\n');
        out.push_str(&html[index..]);
        return out;
    }

    format!("{html}\n{script}\n")
}

/// Finds the byte offset of the last case-insensitive occurrence of `tag`.
///
/// Only the tag name is lowercased for comparison, and the tags we look for are
/// pure ASCII, so byte offsets from the lowercased copy stay valid in the
/// original even when the document contains multi-byte characters.
fn find_last_tag(html: &str, tag: &str) -> Option<usize> {
    html.to_ascii_lowercase().rfind(tag)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn injects_before_the_closing_body_tag() {
        let out = inject("<html><body><h1>hi</h1></body></html>");
        assert!(out.contains(&format!("<script src=\"{CLIENT_PATH}\" defer></script>")));
        let script = out.find("<script").unwrap();
        let body = out.find("</body>").unwrap();
        assert!(script < body);
    }

    #[test]
    fn matches_the_closing_tag_regardless_of_case() {
        let out = inject("<HTML><BODY>hi</BODY></HTML>");
        assert!(out.find("<script").unwrap() < out.find("</BODY>").unwrap());
    }

    #[test]
    fn falls_back_to_the_closing_html_tag() {
        let out = inject("<html><p>no body tag</p></html>");
        assert!(out.find("<script").unwrap() < out.find("</html>").unwrap());
    }

    #[test]
    fn appends_when_the_document_has_neither_tag() {
        let out = inject("<h1>fragment</h1>");
        assert!(out.starts_with("<h1>fragment</h1>"));
        assert!(out.contains("<script"));
    }

    #[test]
    fn ignores_an_escaped_body_tag_earlier_in_the_document() {
        // A tutorial page showing `</body>` as text must not be injected into.
        let html = "<html><body><pre>&lt;/body&gt;</pre><p>x</p></body></html>";
        let out = inject(html);
        assert!(out.find("<script").unwrap() > out.find("<pre>").unwrap());
    }

    #[test]
    fn uses_the_final_body_tag_when_one_appears_inside_a_string() {
        let html = "<html><body><script>var a = \"</body>\";</script></body></html>";
        let out = inject(html);
        // Exactly one injected tag, and it sits after the inline script.
        assert_eq!(out.matches(CLIENT_PATH).count(), 1);
        assert!(out.find(CLIENT_PATH).unwrap() > out.find("var a").unwrap());
    }

    #[test]
    fn keeps_multibyte_content_intact() {
        let html = "<html><body><p>日本語のテキスト</p></body></html>";
        let out = inject(html);
        assert!(out.contains("日本語のテキスト"));
        assert!(out.find("<script").unwrap() < out.find("</body>").unwrap());
    }
}
