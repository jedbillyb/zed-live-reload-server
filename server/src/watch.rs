//! Filesystem watching.
//!
//! Editor save events are deliberately not the trigger for reloads. Watching
//! the filesystem instead means a Sass build, a bundler, a `git checkout` or an
//! edit made in another program all reload the page too, and it keeps us from
//! firing twice for a single save.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use globset::GlobSet;
use notify::{Event, EventKind, RecursiveMode, Watcher};
use tokio::sync::{broadcast, mpsc};

use crate::config::Config;
use crate::reload::{classify, Reload};

/// Starts watching `root`, emitting reload instructions on `reload`.
///
/// The returned watcher must be kept alive: dropping it stops the watch.
pub fn watch(
    root: PathBuf,
    config: Arc<Config>,
    ignore: GlobSet,
    reload: broadcast::Sender<Reload>,
) -> notify::Result<impl Watcher> {
    let (tx, rx) = mpsc::unbounded_channel();

    let mut watcher = notify::recommended_watcher(move |event: notify::Result<Event>| {
        // The callback runs on notify's own thread, so it must not block. An
        // unbounded send is the cheapest way off that thread.
        if let Ok(event) = event {
            let _ = tx.send(event);
        }
    })?;

    watcher.watch(&root, RecursiveMode::Recursive)?;
    tokio::spawn(debounce(root, config, ignore, reload, rx));
    Ok(watcher)
}

/// Collects changes into quiet-period batches and turns each batch into the
/// smallest set of reload instructions that covers it.
async fn debounce(
    root: PathBuf,
    config: Arc<Config>,
    ignore: GlobSet,
    reload: broadcast::Sender<Reload>,
    mut rx: mpsc::UnboundedReceiver<Event>,
) {
    // A zero wait would reload on each of the several writes a tool makes while
    // saving, so the window is never allowed to fully close.
    let window = Duration::from_millis(config.wait.max(20));
    let mut pending: BTreeSet<PathBuf> = BTreeSet::new();

    loop {
        // Block until something happens, then keep draining until the
        // filesystem goes quiet for a full window.
        let Some(event) = rx.recv().await else {
            return;
        };
        collect(event, &root, &ignore, &mut pending);

        loop {
            match tokio::time::timeout(window, rx.recv()).await {
                Ok(Some(event)) => collect(event, &root, &ignore, &mut pending),
                Ok(None) => return,
                Err(_) => break,
            }
        }

        if pending.is_empty() {
            continue;
        }

        for instruction in plan(&pending, &root, &config) {
            // An error here only means no browser is connected.
            let _ = reload.send(instruction);
        }
        pending.clear();
    }
}

/// Records the paths from one event, dropping anything ignored.
fn collect(event: Event, root: &Path, ignore: &GlobSet, pending: &mut BTreeSet<PathBuf>) {
    // Access events fire on plain reads and would reload the page whenever
    // anything so much as opened a file, including this server.
    if matches!(event.kind, EventKind::Access(_) | EventKind::Other) {
        return;
    }

    for path in event.paths {
        if is_ignored(&path, root, ignore) {
            continue;
        }
        pending.insert(path);
    }
}

/// Matches ignore globs against both the workspace-relative and absolute paths.
///
/// Users write patterns like `**/*.log` expecting them to match relative to the
/// project, but also occasionally write absolute paths, so both are tried.
///
/// Ancestors are tested as well, so that ignoring a directory ignores its whole
/// subtree. Without this, a pattern like `**/dist` would filter out the
/// directory's own creation event but nothing inside it, and the reverse
/// pattern `**/dist/**` would filter the contents but not the directory. People
/// write one or the other and reasonably expect both to mean the same thing.
fn is_ignored(path: &Path, root: &Path, ignore: &GlobSet) -> bool {
    if matches_with_ancestors(path, ignore) {
        return true;
    }
    path.strip_prefix(root)
        .map(|relative| matches_with_ancestors(relative, ignore))
        .unwrap_or(false)
}

fn matches_with_ancestors(path: &Path, ignore: &GlobSet) -> bool {
    path.ancestors()
        .filter(|ancestor| !ancestor.as_os_str().is_empty())
        .any(|ancestor| ignore.is_match(ancestor))
}

/// Reduces a batch of changed paths to the instructions to send.
///
/// A single full reload supersedes every hot swap in the same batch, so a build
/// that rewrites markup and stylesheets together produces one page load rather
/// than a reload plus a flurry of redundant stylesheet swaps.
fn plan(pending: &BTreeSet<PathBuf>, root: &Path, config: &Config) -> Vec<Reload> {
    let mut instructions = Vec::new();

    for path in pending {
        let Some(url_path) = url_path_for(path, root) else {
            // A change outside the document root, most likely in a mounted
            // directory. We cannot name it in a hot swap, so reload.
            return vec![Reload::Full];
        };

        let instruction = classify(&url_path, config.full_reload);
        if instruction == Reload::Full {
            return vec![Reload::Full];
        }
        if !instructions.contains(&instruction) {
            instructions.push(instruction);
        }
    }

    instructions
}

/// Converts an absolute filesystem path into the URL path it is served at.
fn url_path_for(path: &Path, root: &Path) -> Option<String> {
    let relative = path.strip_prefix(root).ok()?;
    let mut url = String::from("/");
    for (index, component) in relative.components().enumerate() {
        if index > 0 {
            url.push('/');
        }
        url.push_str(&component.as_os_str().to_string_lossy());
    }
    Some(url)
}

#[cfg(test)]
mod tests {
    use super::*;
    use globset::{Glob, GlobSetBuilder};

    fn globs(patterns: &[&str]) -> GlobSet {
        let mut builder = GlobSetBuilder::new();
        for pattern in patterns {
            builder.add(Glob::new(pattern).unwrap());
        }
        builder.build().unwrap()
    }

    #[test]
    fn builds_url_paths_from_the_document_root() {
        let root = Path::new("/srv/site");
        assert_eq!(
            url_path_for(Path::new("/srv/site/css/app.css"), root).unwrap(),
            "/css/app.css"
        );
        assert_eq!(
            url_path_for(Path::new("/srv/site/index.html"), root).unwrap(),
            "/index.html"
        );
    }

    #[test]
    fn returns_nothing_for_paths_outside_the_root() {
        assert!(url_path_for(Path::new("/elsewhere/a.css"), Path::new("/srv/site")).is_none());
    }

    #[test]
    fn ignores_by_relative_pattern() {
        let root = Path::new("/srv/site");
        let ignore = globs(&["**/node_modules/**", "**/*.log"]);
        assert!(is_ignored(
            Path::new("/srv/site/node_modules/x/index.js"),
            root,
            &ignore
        ));
        assert!(is_ignored(Path::new("/srv/site/debug.log"), root, &ignore));
        assert!(!is_ignored(Path::new("/srv/site/app.js"), root, &ignore));
    }

    #[test]
    fn ignoring_a_directory_ignores_everything_under_it() {
        let root = Path::new("/srv/site");
        // Only the bare directory form, with no trailing `/**`.
        let ignore = globs(&["**/dist"]);
        assert!(is_ignored(Path::new("/srv/site/dist"), root, &ignore));
        assert!(is_ignored(
            Path::new("/srv/site/dist/a/b.css"),
            root,
            &ignore
        ));
        assert!(!is_ignored(Path::new("/srv/site/src/a.css"), root, &ignore));
    }

    #[test]
    fn the_contents_form_also_covers_the_directory_itself() {
        let root = Path::new("/srv/site");
        // The reverse spelling, which is what the defaults used to use alone.
        let ignore = globs(&["**/node_modules/**"]);
        assert!(is_ignored(
            Path::new("/srv/site/node_modules/x/i.js"),
            root,
            &ignore
        ));
    }

    #[test]
    fn the_defaults_cover_a_freshly_created_dependency_directory() {
        let root = Path::new("/srv/site");
        let (ignore, errors) = Config::default().ignore_set();
        assert!(errors.is_empty(), "{errors:?}");

        // Creating the directory emits an event for the directory itself, which
        // is what leaked a spurious reload before.
        assert!(is_ignored(
            Path::new("/srv/site/node_modules"),
            root,
            &ignore
        ));
        assert!(is_ignored(
            Path::new("/srv/site/node_modules/x/index.js"),
            root,
            &ignore
        ));
        assert!(is_ignored(Path::new("/srv/site/.git/HEAD"), root, &ignore));
        assert!(!is_ignored(
            Path::new("/srv/site/index.html"),
            root,
            &ignore
        ));
    }

    #[test]
    fn a_single_full_reload_supersedes_hot_swaps_in_the_same_batch() {
        let root = Path::new("/srv/site");
        let config = Config::default();
        let pending: BTreeSet<PathBuf> = [
            PathBuf::from("/srv/site/app.css"),
            PathBuf::from("/srv/site/index.html"),
        ]
        .into_iter()
        .collect();

        assert_eq!(plan(&pending, root, &config), vec![Reload::Full]);
    }

    #[test]
    fn batches_multiple_stylesheets_into_separate_swaps() {
        let root = Path::new("/srv/site");
        let config = Config::default();
        let pending: BTreeSet<PathBuf> = [
            PathBuf::from("/srv/site/a.css"),
            PathBuf::from("/srv/site/b.css"),
        ]
        .into_iter()
        .collect();

        assert_eq!(
            plan(&pending, root, &config),
            vec![
                Reload::Css("/a.css".to_string()),
                Reload::Css("/b.css".to_string())
            ]
        );
    }

    #[test]
    fn falls_back_to_a_full_reload_for_changes_outside_the_root() {
        let root = Path::new("/srv/site");
        let config = Config::default();
        let pending: BTreeSet<PathBuf> = [PathBuf::from("/srv/vendor/a.css")].into_iter().collect();
        assert_eq!(plan(&pending, root, &config), vec![Reload::Full]);
    }

    #[test]
    fn respects_the_full_reload_override() {
        let root = Path::new("/srv/site");
        let config = Config {
            full_reload: true,
            ..Config::default()
        };
        let pending: BTreeSet<PathBuf> = [PathBuf::from("/srv/site/a.css")].into_iter().collect();
        assert_eq!(plan(&pending, root, &config), vec![Reload::Full]);
    }

    #[test]
    fn skips_access_events() {
        let root = Path::new("/srv/site");
        let ignore = globs(&[]);
        let mut pending = BTreeSet::new();
        let event = Event {
            kind: EventKind::Access(notify::event::AccessKind::Read),
            paths: vec![PathBuf::from("/srv/site/index.html")],
            attrs: Default::default(),
        };
        collect(event, root, &ignore, &mut pending);
        assert!(pending.is_empty());
    }
}
