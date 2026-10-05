//! Browser bookmarks, shared by every browser tab and persisted as JSON in
//! the global key-value store.

use db::kvp::KeyValueStore;
use gpui::{App, AppContext as _, Context, Entity, Global, TaskExt as _};
use serde::{Deserialize, Serialize};
use util::ResultExt as _;

const BOOKMARKS_KEY: &str = "browser_tab_bookmarks";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Bookmark {
    pub title: String,
    pub url: String,
}

struct GlobalBookmarkStore(Entity<BookmarkStore>);

impl Global for GlobalBookmarkStore {}

pub struct BookmarkStore {
    bookmarks: Vec<Bookmark>,
}

pub(crate) fn init(cx: &mut App) {
    let stored = KeyValueStore::global(cx)
        .read_kvp(BOOKMARKS_KEY)
        .log_err()
        .flatten();
    let bookmarks = bookmarks_from_stored(stored.as_deref(), || {
        default_bookmarks(&othcloud_client::absolute_url("/dashboard"))
    });
    let store = cx.new(|_| BookmarkStore { bookmarks });
    cx.set_global(GlobalBookmarkStore(store));
}

impl BookmarkStore {
    pub fn global(cx: &App) -> Option<Entity<Self>> {
        cx.try_global::<GlobalBookmarkStore>()
            .map(|store| store.0.clone())
    }

    pub fn bookmarks(&self) -> &[Bookmark] {
        &self.bookmarks
    }

    pub fn is_bookmarked(&self, url: &str) -> bool {
        is_bookmarked(&self.bookmarks, url)
    }

    pub fn toggle(&mut self, title: &str, url: &str, cx: &mut Context<Self>) {
        toggle_bookmark(&mut self.bookmarks, title, url);
        self.changed(cx);
    }

    pub fn remove(&mut self, url: &str, cx: &mut Context<Self>) {
        if remove_bookmark(&mut self.bookmarks, url) {
            self.changed(cx);
        }
    }

    fn changed(&mut self, cx: &mut Context<Self>) {
        cx.notify();
        let Some(json) = serde_json::to_string(&self.bookmarks).log_err() else {
            return;
        };
        let kvp = KeyValueStore::global(cx);
        cx.background_spawn(async move { kvp.write_kvp(BOOKMARKS_KEY.to_string(), json).await })
            .detach_and_log_err(cx);
    }
}

/// The bookmarks to start with: what was stored (an empty list stays empty),
/// or the defaults when nothing was ever stored or it can't be read.
fn bookmarks_from_stored(
    stored: Option<&str>,
    defaults: impl FnOnce() -> Vec<Bookmark>,
) -> Vec<Bookmark> {
    match stored {
        Some(json) => serde_json::from_str(json)
            .log_err()
            .unwrap_or_else(defaults),
        None => defaults(),
    }
}

fn default_bookmarks(othcloud_console_url: &str) -> Vec<Bookmark> {
    vec![
        Bookmark {
            title: "Claude Code".to_string(),
            url: "https://claude.ai/code".to_string(),
        },
        Bookmark {
            title: "GitHub".to_string(),
            url: "https://github.com".to_string(),
        },
        Bookmark {
            title: "OTHCloud Console".to_string(),
            url: othcloud_console_url.to_string(),
        },
    ]
}

pub(crate) fn is_bookmarked(bookmarks: &[Bookmark], url: &str) -> bool {
    bookmarks.iter().any(|bookmark| bookmark.url == url)
}

/// Removes the bookmark for `url`, or adds one when there is none. Returns
/// whether `url` is bookmarked afterwards.
pub(crate) fn toggle_bookmark(bookmarks: &mut Vec<Bookmark>, title: &str, url: &str) -> bool {
    if remove_bookmark(bookmarks, url) {
        return false;
    }
    bookmarks.push(Bookmark {
        title: title.to_string(),
        url: url.to_string(),
    });
    true
}

/// Returns whether a bookmark was removed.
pub(crate) fn remove_bookmark(bookmarks: &mut Vec<Bookmark>, url: &str) -> bool {
    let count = bookmarks.len();
    bookmarks.retain(|bookmark| bookmark.url != url);
    bookmarks.len() != count
}

#[cfg(test)]
mod tests {
    use super::*;

    fn defaults() -> Vec<Bookmark> {
        default_bookmarks("https://othcloud.xyz/dashboard")
    }

    #[test]
    fn first_run_seeds_the_default_bookmarks() {
        let bookmarks = bookmarks_from_stored(None, defaults);
        let urls: Vec<&str> = bookmarks
            .iter()
            .map(|bookmark| bookmark.url.as_str())
            .collect();
        assert_eq!(
            urls,
            [
                "https://claude.ai/code",
                "https://github.com",
                "https://othcloud.xyz/dashboard"
            ]
        );
    }

    #[test]
    fn stored_empty_list_is_not_reseeded() {
        assert_eq!(bookmarks_from_stored(Some("[]"), defaults), Vec::new());
    }

    #[test]
    fn stored_bookmarks_round_trip() {
        let mut bookmarks = Vec::new();
        toggle_bookmark(&mut bookmarks, "Example", "https://example.com/");
        let json = serde_json::to_string(&bookmarks).expect("bookmarks serialize");
        assert_eq!(bookmarks_from_stored(Some(&json), defaults), bookmarks);
    }

    #[test]
    fn unreadable_stored_bookmarks_fall_back_to_defaults() {
        assert_eq!(
            bookmarks_from_stored(Some("not json"), defaults),
            defaults()
        );
    }

    #[test]
    fn toggle_adds_then_removes() {
        let mut bookmarks = Vec::new();
        assert!(toggle_bookmark(
            &mut bookmarks,
            "Example",
            "https://example.com/"
        ));
        assert!(is_bookmarked(&bookmarks, "https://example.com/"));
        assert_eq!(bookmarks.len(), 1);

        assert!(!toggle_bookmark(
            &mut bookmarks,
            "Other title",
            "https://example.com/"
        ));
        assert!(!is_bookmarked(&bookmarks, "https://example.com/"));
        assert!(bookmarks.is_empty());
    }

    #[test]
    fn remove_only_touches_the_matching_url() {
        let mut bookmarks = defaults();
        assert!(remove_bookmark(&mut bookmarks, "https://github.com"));
        assert!(!remove_bookmark(&mut bookmarks, "https://github.com"));
        assert_eq!(bookmarks.len(), 2);
        assert!(is_bookmarked(&bookmarks, "https://claude.ai/code"));
    }
}
