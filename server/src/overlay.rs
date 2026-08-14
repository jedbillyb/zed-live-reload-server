//! Unsaved editor buffers, kept in memory so they can be served in place of
//! the file on disk. This backs the `live_changes` beta option.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::sync::RwLock;
use tower_lsp::lsp_types::{Position, Range};

/// Buffer contents the editor has told us about but which are not on disk yet.
#[derive(Clone, Default)]
pub struct Overlay {
    documents: Arc<RwLock<HashMap<PathBuf, String>>>,
}

impl Overlay {
    /// Replaces the whole buffer for a path.
    pub async fn set(&self, path: PathBuf, text: String) {
        self.documents.write().await.insert(path, text);
    }

    /// Forgets a buffer, so later requests fall back to disk.
    pub async fn clear(&self, path: &Path) {
        self.documents.write().await.remove(path);
    }

    /// Drops every buffer.
    pub async fn clear_all(&self) {
        self.documents.write().await.clear();
    }

    /// Returns the buffer for a path, if we are holding one.
    pub async fn get(&self, path: &Path) -> Option<String> {
        self.documents.read().await.get(path).cloned()
    }

    /// Applies an incremental LSP edit. A `None` range means a full replace.
    ///
    /// Unknown paths are inserted rather than ignored, so an edit that arrives
    /// before we recorded the open document still leaves us with usable content.
    pub async fn apply(&self, path: &Path, range: Option<Range>, text: &str) {
        let mut documents = self.documents.write().await;
        let Some(document) = documents.get_mut(path) else {
            if range.is_none() {
                documents.insert(path.to_path_buf(), text.to_string());
            }
            return;
        };

        match range {
            None => *document = text.to_string(),
            Some(range) => {
                let start = byte_offset(document, range.start);
                let end = byte_offset(document, range.end);
                // Guard against a range the client and our copy disagree about,
                // which would otherwise panic on a bad slice.
                if start <= end && end <= document.len() {
                    document.replace_range(start..end, text);
                }
            }
        }
    }
}

/// Converts an LSP [`Position`] to a byte offset.
///
/// LSP counts lines in `\n` terminated rows and columns in UTF-16 code units,
/// not bytes and not characters. Conflating those silently corrupts any buffer
/// containing non-ASCII text, so the conversion is done explicitly here.
///
/// Positions past the end of a line clamp to the line end, and positions past
/// the end of the document clamp to its length, matching how editors treat a
/// stale position rather than panicking.
fn byte_offset(text: &str, position: Position) -> usize {
    let mut offset = 0;
    let mut line = 0;

    // Walk to the start of the target line.
    while line < position.line {
        match text[offset..].find('\n') {
            Some(index) => {
                offset += index + 1;
                line += 1;
            }
            None => return text.len(),
        }
    }

    let line_end = text[offset..]
        .find('\n')
        .map(|index| offset + index)
        .unwrap_or(text.len());

    let mut utf16 = 0;
    for (index, character) in text[offset..line_end].char_indices() {
        if utf16 >= position.character {
            return offset + index;
        }
        utf16 += character.len_utf16() as u32;
    }

    line_end
}

#[cfg(test)]
mod tests {
    use super::*;

    fn position(line: u32, character: u32) -> Position {
        Position { line, character }
    }

    #[test]
    fn resolves_offsets_in_ascii_text() {
        let text = "abc\ndef\nghi";
        assert_eq!(byte_offset(text, position(0, 0)), 0);
        assert_eq!(byte_offset(text, position(0, 3)), 3);
        assert_eq!(byte_offset(text, position(1, 0)), 4);
        assert_eq!(byte_offset(text, position(2, 2)), 10);
    }

    #[test]
    fn counts_columns_in_utf16_units_not_bytes() {
        // "é" is two bytes but one UTF-16 unit.
        let text = "éé!";
        assert_eq!(byte_offset(text, position(0, 1)), 2);
        assert_eq!(byte_offset(text, position(0, 2)), 4);
    }

    #[test]
    fn treats_astral_characters_as_two_utf16_units() {
        // An emoji outside the BMP is a surrogate pair to the client.
        let text = "a🎉b";
        assert_eq!(byte_offset(text, position(0, 1)), 1);
        assert_eq!(byte_offset(text, position(0, 3)), 5);
    }

    #[test]
    fn clamps_positions_past_the_end_of_a_line() {
        let text = "ab\ncd";
        assert_eq!(byte_offset(text, position(0, 99)), 2);
    }

    #[test]
    fn clamps_positions_past_the_end_of_the_document() {
        let text = "ab\ncd";
        assert_eq!(byte_offset(text, position(99, 0)), text.len());
    }

    #[tokio::test]
    async fn applies_a_ranged_edit() {
        let overlay = Overlay::default();
        let path = PathBuf::from("/tmp/index.html");
        overlay.set(path.clone(), "<h1>old</h1>".to_string()).await;

        overlay
            .apply(
                &path,
                Some(Range {
                    start: position(0, 4),
                    end: position(0, 7),
                }),
                "new",
            )
            .await;

        assert_eq!(overlay.get(&path).await.unwrap(), "<h1>new</h1>");
    }

    #[tokio::test]
    async fn applies_a_full_replace() {
        let overlay = Overlay::default();
        let path = PathBuf::from("/tmp/a.css");
        overlay.set(path.clone(), "a{}".to_string()).await;
        overlay.apply(&path, None, "b{}").await;
        assert_eq!(overlay.get(&path).await.unwrap(), "b{}");
    }

    #[tokio::test]
    async fn edits_multibyte_buffers_without_corrupting_them() {
        let overlay = Overlay::default();
        let path = PathBuf::from("/tmp/i18n.html");
        overlay.set(path.clone(), "<p>日本語</p>".to_string()).await;

        // Replace the three Japanese characters, columns 3..6 in UTF-16 units.
        overlay
            .apply(
                &path,
                Some(Range {
                    start: position(0, 3),
                    end: position(0, 6),
                }),
                "hello",
            )
            .await;

        assert_eq!(overlay.get(&path).await.unwrap(), "<p>hello</p>");
    }

    #[tokio::test]
    async fn clearing_falls_back_to_disk() {
        let overlay = Overlay::default();
        let path = PathBuf::from("/tmp/gone.html");
        overlay.set(path.clone(), "x".to_string()).await;
        overlay.clear(&path).await;
        assert!(overlay.get(&path).await.is_none());
    }
}
