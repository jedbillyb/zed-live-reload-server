//! Deciding what a changed file should do to the open page.

use std::path::Path;

/// Instruction sent to connected browsers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reload {
    /// Reload the whole document.
    Full,
    /// Swap the stylesheet at this URL path in place.
    Css(String),
    /// Re-fetch the image at this URL path in place.
    Image(String),
}

impl Reload {
    /// Serialises the instruction for the browser client.
    ///
    /// Hand-rolled rather than via serde because the shapes are fixed and tiny,
    /// and the only value needing care is the path, which is already a URL path
    /// and so cannot contain a quote or backslash.
    pub fn to_message(&self) -> String {
        match self {
            Reload::Full => "{\"type\":\"reload\"}".to_string(),
            Reload::Css(path) => format!("{{\"type\":\"css\",\"path\":\"{path}\"}}"),
            Reload::Image(path) => format!("{{\"type\":\"image\",\"path\":\"{path}\"}}"),
        }
    }
}

const IMAGE_EXTENSIONS: &[&str] = &[
    "png", "jpg", "jpeg", "gif", "webp", "avif", "bmp", "ico", "svg",
];

/// Chooses the cheapest update that can pick up a change to `url_path`.
///
/// Hot swapping a stylesheet or an image keeps scroll position, form state and
/// any JavaScript state on the page, which is the whole reason to bother. When
/// `full_reload` is set the user has asked us not to be clever.
pub fn classify(url_path: &str, full_reload: bool) -> Reload {
    if full_reload {
        return Reload::Full;
    }

    let extension = Path::new(url_path)
        .extension()
        .and_then(|extension| extension.to_str())
        .map(|extension| extension.to_ascii_lowercase());

    match extension.as_deref() {
        Some("css") => Reload::Css(url_path.to_string()),
        // An SVG can be a stylesheet-adjacent asset or a document in its own
        // right, but treating it as an image is right for the common case of it
        // being referenced from an `<img>`, and the client falls back to a full
        // reload when it cannot find a matching element.
        Some(extension) if IMAGE_EXTENSIONS.contains(&extension) => {
            Reload::Image(url_path.to_string())
        }
        _ => Reload::Full,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hot_swaps_stylesheets() {
        assert_eq!(
            classify("/css/site.css", false),
            Reload::Css("/css/site.css".to_string())
        );
    }

    #[test]
    fn hot_swaps_images() {
        assert_eq!(
            classify("/img/logo.PNG", false),
            Reload::Image("/img/logo.PNG".to_string())
        );
    }

    #[test]
    fn reloads_fully_for_markup_and_scripts() {
        assert_eq!(classify("/index.html", false), Reload::Full);
        assert_eq!(classify("/app.js", false), Reload::Full);
        assert_eq!(classify("/data", false), Reload::Full);
    }

    #[test]
    fn honours_the_full_reload_override() {
        assert_eq!(classify("/site.css", true), Reload::Full);
    }

    #[test]
    fn serialises_each_instruction() {
        assert_eq!(Reload::Full.to_message(), "{\"type\":\"reload\"}");
        assert_eq!(
            Reload::Css("/a.css".into()).to_message(),
            "{\"type\":\"css\",\"path\":\"/a.css\"}"
        );
        assert_eq!(
            Reload::Image("/a.png".into()).to_message(),
            "{\"type\":\"image\",\"path\":\"/a.png\"}"
        );
    }
}
