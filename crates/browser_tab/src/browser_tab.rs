//! OTerminal: a web browser as a workspace item (center pane tab).
//!
//! Each [`BrowserTab`] shows one web page under an address bar and a
//! bookmarks bar. The page itself is a native web view (see `web_view`),
//! currently on Windows only. Tabs are serialized by URL and reopen the same
//! page on restart.

mod bookmarks;
#[cfg(target_os = "windows")]
mod web_view;

use anyhow::Result;
use db::kvp::KeyValueStore;
use editor::{Editor, EditorEvent, actions::SelectAll};
use gpui::{
    AnyWindowHandle, App, Context, Entity, EventEmitter, FocusHandle, Focusable, IntoElement,
    ParentElement, Render, SharedString, Styled, Subscription, Task, WeakEntity, Window, actions,
    div,
};
use project::Project;
use ui::{ContextMenu, Icon, IconButton, IconName, Tooltip, prelude::*, right_click_menu};
use util::ResultExt as _;
use workspace::{Item, ItemId, SerializableItem, Workspace, WorkspaceId, item::ItemEvent};

pub use bookmarks::{Bookmark, BookmarkStore};

#[cfg(not(target_os = "windows"))]
use unsupported::Page;
#[cfg(target_os = "windows")]
use web_view::Page;

actions!(
    browser,
    [
        /// Opens a new browser tab in the active pane.
        NewTab,
        /// Reloads the page in the active browser tab.
        Reload,
        /// Goes back one page in the active browser tab's history.
        GoBack,
        /// Goes forward one page in the active browser tab's history.
        GoForward,
        /// Focuses the address bar of the active browser tab.
        FocusAddressBar,
        /// Adds the active browser tab's page to the bookmarks bar, or
        /// removes it when it is already bookmarked.
        ToggleBookmark
    ]
);

const BROWSER_TABS_NAMESPACE: &str = "browser_tabs";
const DEFAULT_URL: &str = "https://duckduckgo.com/";
const SEARCH_URL_PREFIX: &str = "https://duckduckgo.com/?q=";
const FALLBACK_TAB_TITLE: &str = "Browser";
const MAX_TAB_TITLE_CHARS: usize = 40;
const MAX_BOOKMARK_LABEL_CHARS: usize = 24;
/// How many frames after a keystroke or a workspace change the page keeps
/// checking whether something was opened over it.
const VISIBILITY_CHECKS_AFTER_CHANGE: u8 = 3;

pub fn init(cx: &mut App) {
    bookmarks::init(cx);
    workspace::register_serializable_item::<BrowserTab>(cx);

    cx.observe_new(|workspace: &mut Workspace, _window, _cx| {
        workspace
            .register_action(|workspace, _: &NewTab, window, cx| {
                BrowserTab::open_new(workspace, window, cx);
            })
            .register_action(|workspace, _: &Reload, _window, cx| {
                update_active_tab(workspace, cx, |tab, _| tab.page.reload());
            })
            .register_action(|workspace, _: &GoBack, _window, cx| {
                update_active_tab(workspace, cx, |tab, _| tab.page.go_back());
            })
            .register_action(|workspace, _: &GoForward, _window, cx| {
                update_active_tab(workspace, cx, |tab, _| tab.page.go_forward());
            })
            .register_action(|workspace, _: &FocusAddressBar, window, cx| {
                update_active_tab(workspace, cx, |tab, cx| tab.focus_address_bar(window, cx));
            })
            .register_action(|workspace, _: &ToggleBookmark, _window, cx| {
                update_active_tab(workspace, cx, |tab, cx| tab.toggle_bookmark(cx));
            });
    })
    .detach();
}

fn update_active_tab(
    workspace: &Workspace,
    cx: &mut Context<Workspace>,
    update: impl FnOnce(&mut BrowserTab, &mut Context<BrowserTab>),
) {
    if let Some(tab) = workspace.active_item_as::<BrowserTab>(cx) {
        tab.update(cx, update);
    }
}

/// Turns what was typed into the address bar into a URL: a URL is used as
/// is, something that looks like a host gets `https://` (`http://` for this
/// machine, where dev servers rarely have certificates), and anything else
/// becomes a web search. Returns `None` for blank input.
fn url_from_address_input(input: &str) -> Option<String> {
    let input = input.trim();
    if input.is_empty() {
        None
    } else if has_scheme(input) {
        Some(input.to_string())
    } else if looks_like_host(input) {
        let scheme = if is_local_host(input) {
            "http"
        } else {
            "https"
        };
        Some(format!("{scheme}://{input}"))
    } else {
        Some(format!("{SEARCH_URL_PREFIX}{}", urlencoding::encode(input)))
    }
}

fn is_local_host(input: &str) -> bool {
    ["localhost", "127.0.0.1", "[::1]"].into_iter().any(|host| {
        input
            .strip_prefix(host)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with([':', '/']))
    })
}

fn has_scheme(input: &str) -> bool {
    if input.starts_with("about:") {
        return true;
    }
    let Some((scheme, rest)) = input.split_once("://") else {
        return false;
    };
    let mut characters = scheme.chars();
    !rest.is_empty()
        && characters
            .next()
            .is_some_and(|first| first.is_ascii_alphabetic())
        && characters.all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '+' | '-' | '.')
        })
}

fn looks_like_host(input: &str) -> bool {
    if input.chars().any(char::is_whitespace) {
        return false;
    }
    let authority = input.split(['/', '?', '#']).next().unwrap_or(input);
    if authority.starts_with('[') {
        return authority.contains(']');
    }
    let host = match authority.rsplit_once(':') {
        Some((host, port))
            if !port.is_empty() && port.chars().all(|digit| digit.is_ascii_digit()) =>
        {
            host
        }
        Some(_) => return false,
        None => authority,
    };
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    host.contains('.')
        && !host.starts_with('.')
        && !host.ends_with('.')
        && host
            .chars()
            .all(|character| character.is_alphanumeric() || matches!(character, '.' | '-'))
}

fn host_of(url: &str) -> Option<String> {
    url::Url::parse(url)
        .ok()?
        .host_str()
        .map(|host| host.to_string())
}

/// The tab's label: the page title, else the host being loaded, else
/// "Browser".
fn tab_title(title: Option<&str>, url: &str) -> String {
    match title.map(str::trim).filter(|title| !title.is_empty()) {
        Some(title) => util::truncate_and_trailoff(title, MAX_TAB_TITLE_CHARS),
        None => host_of(url).unwrap_or_else(|| FALLBACK_TAB_TITLE.to_string()),
    }
}

/// The title a new bookmark for this page gets.
fn bookmark_title(title: Option<&str>, url: &str) -> String {
    match title.map(str::trim).filter(|title| !title.is_empty()) {
        Some(title) => title.to_string(),
        None => host_of(url).unwrap_or_else(|| url.to_string()),
    }
}

#[cfg(target_os = "windows")]
pub(crate) enum PageEvent {
    Navigated(String),
    TitleChanged(String),
    /// The page asked for a new window (`target="_blank"`, `window.open`).
    NewWindowRequested(String),
}

pub enum BrowserTabEvent {
    UpdateTab,
    UrlChanged,
}

pub struct BrowserTab {
    address_bar: Entity<Editor>,
    current_url: String,
    title: Option<String>,
    can_go_back: bool,
    can_go_forward: bool,
    bookmark_store: Option<Entity<BookmarkStore>>,
    page: Page,
    window: AnyWindowHandle,
    _subscriptions: Vec<Subscription>,
    _workspace_subscriptions: Vec<Subscription>,
}

impl BrowserTab {
    /// Creates a tab showing `url`, or the start page when `url` is `None`.
    pub fn new(url: Option<String>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let current_url = url.unwrap_or_else(|| DEFAULT_URL.to_string());
        let address_bar = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("Search or enter address", window, cx);
            editor.set_text(current_url.clone(), window, cx);
            editor
        });
        let bookmark_store = BookmarkStore::global(cx);

        let mut subscriptions = vec![
            cx.subscribe_in(
                &address_bar,
                window,
                |this, _, event: &EditorEvent, window, cx| {
                    if matches!(event, EditorEvent::Blurred) {
                        this.sync_address_bar(window, cx);
                    }
                },
            ),
            // Keystrokes open and dismiss menus and modals without any other
            // signal reaching this tab.
            cx.observe_keystrokes(|this, _, window, cx| {
                if window.window_handle() == this.window {
                    this.request_visibility_checks(VISIBILITY_CHECKS_AFTER_CHANGE, window, cx);
                }
            }),
        ];
        if let Some(bookmark_store) = &bookmark_store {
            subscriptions.push(cx.observe(bookmark_store, |_, _, cx| cx.notify()));
        }

        Self {
            address_bar,
            current_url,
            title: None,
            can_go_back: false,
            can_go_forward: false,
            bookmark_store,
            page: Page::new(cx),
            window: window.window_handle(),
            _subscriptions: subscriptions,
            _workspace_subscriptions: Vec::new(),
        }
    }

    /// Opens a new browser tab in the active pane.
    pub fn open_new(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
        let tab = cx.new(|cx| Self::new(None, window, cx));
        workspace.add_item_to_active_pane(Box::new(tab), None, true, window, cx);
    }

    pub fn current_url(&self) -> &str {
        &self.current_url
    }

    /// Loads `url` in this tab.
    pub fn navigate(&mut self, url: String, window: &mut Window, cx: &mut Context<Self>) {
        self.page.load_url(&url);
        self.title = None;
        self.current_url = url;
        self.address_bar.update(cx, |editor, cx| {
            editor.set_text(self.current_url.clone(), window, cx);
        });
        cx.emit(BrowserTabEvent::UrlChanged);
        cx.notify();
    }

    fn navigate_from_address_bar(
        &mut self,
        _: &menu::Confirm,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let input = self.address_bar.read(cx).text(cx);
        if let Some(url) = url_from_address_input(&input) {
            self.navigate(url, window, cx);
            self.page.focus();
        }
    }

    fn focus_address_bar(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.page.release_keyboard_focus();
        let focus_handle = self.address_bar.focus_handle(cx);
        window.focus(&focus_handle, cx);
        self.address_bar.update(cx, |editor, cx| {
            editor.select_all(&SelectAll, window, cx);
        });
    }

    fn toggle_bookmark(&mut self, cx: &mut Context<Self>) {
        let Some(bookmark_store) = &self.bookmark_store else {
            return;
        };
        let title = bookmark_title(self.title.as_deref(), &self.current_url);
        bookmark_store.update(cx, |store, cx| {
            store.toggle(&title, &self.current_url, cx);
        });
    }

    /// The address bar shows the page's URL unless the user is typing in it.
    fn sync_address_bar(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let url = self.current_url.clone();
        self.address_bar.update(cx, |editor, cx| {
            if editor.text(cx) != url {
                editor.set_text(url, window, cx);
            }
        });
    }

    #[cfg(target_os = "windows")]
    fn handle_page_event(&mut self, event: PageEvent, window: &mut Window, cx: &mut Context<Self>) {
        match event {
            PageEvent::Navigated(url) => self.page_url_changed(url, window, cx),
            PageEvent::TitleChanged(title) => {
                self.title = Some(title).filter(|title| !title.trim().is_empty());
                if let Some(url) = self.page.url() {
                    self.page_url_changed(url, window, cx);
                }
                cx.emit(BrowserTabEvent::UpdateTab);
            }
            PageEvent::NewWindowRequested(url) => self.navigate(url, window, cx),
        }
        self.can_go_back = self.page.can_go_back();
        self.can_go_forward = self.page.can_go_forward();
        cx.notify();
    }

    #[cfg(target_os = "windows")]
    fn page_url_changed(&mut self, url: String, window: &mut Window, cx: &mut Context<Self>) {
        if url == self.current_url {
            return;
        }
        self.current_url = url;
        self.title = None;
        // GPUI is not told when the page takes the keyboard, so the address
        // bar can still look focused while the user is in the page.
        let typing_in_address_bar =
            self.address_bar.focus_handle(cx).is_focused(window) && !self.page.has_keyboard_focus();
        if !typing_in_address_bar {
            self.sync_address_bar(window, cx);
        }
        cx.emit(BrowserTabEvent::UrlChanged);
    }

    fn serialization_key(workspace_id: WorkspaceId, item_id: ItemId) -> String {
        format!("{}:{item_id}", i64::from(workspace_id))
    }

    fn render_toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let bookmarked = self
            .bookmark_store
            .as_ref()
            .is_some_and(|store| store.read(cx).is_bookmarked(&self.current_url));
        h_flex()
            .w_full()
            .gap_1()
            .px_2()
            .py_1()
            .border_b_1()
            .border_color(cx.theme().colors().border)
            .child(
                IconButton::new("browser-go-back", IconName::ArrowLeft)
                    .icon_size(IconSize::Small)
                    .disabled(!self.can_go_back)
                    .tooltip(Tooltip::text("Go Back"))
                    .on_click(cx.listener(|this, _, _, _| this.page.go_back())),
            )
            .child(
                IconButton::new("browser-go-forward", IconName::ArrowRight)
                    .icon_size(IconSize::Small)
                    .disabled(!self.can_go_forward)
                    .tooltip(Tooltip::text("Go Forward"))
                    .on_click(cx.listener(|this, _, _, _| this.page.go_forward())),
            )
            .child(
                IconButton::new("browser-reload", IconName::RotateCw)
                    .icon_size(IconSize::Small)
                    .tooltip(Tooltip::text("Reload"))
                    .on_click(cx.listener(|this, _, _, _| this.page.reload())),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .px_2()
                    .py_0p5()
                    .rounded_md()
                    .border_1()
                    .border_color(cx.theme().colors().border_variant)
                    .bg(cx.theme().colors().editor_background)
                    .on_action(cx.listener(Self::navigate_from_address_bar))
                    .child(self.address_bar.clone()),
            )
            .child(
                IconButton::new(
                    "browser-toggle-bookmark",
                    if bookmarked {
                        IconName::StarFilled
                    } else {
                        IconName::Star
                    },
                )
                .icon_size(IconSize::Small)
                .icon_color(if bookmarked {
                    Color::Accent
                } else {
                    Color::Default
                })
                .tooltip(Tooltip::text(if bookmarked {
                    "Remove Bookmark"
                } else {
                    "Bookmark This Page"
                }))
                .on_click(cx.listener(|this, _, _, cx| this.toggle_bookmark(cx))),
            )
    }

    fn render_bookmarks_bar(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let bookmark_store = self.bookmark_store.clone()?;
        let bookmarks = bookmark_store.read(cx).bookmarks().to_vec();
        if bookmarks.is_empty() {
            return None;
        }
        Some(
            h_flex()
                .w_full()
                .gap_1()
                .px_2()
                .py_0p5()
                .overflow_hidden()
                .border_b_1()
                .border_color(cx.theme().colors().border)
                .children(bookmarks.into_iter().enumerate().map(|(index, bookmark)| {
                    let open_bookmark = cx.listener({
                        let url = bookmark.url.clone();
                        move |this, _, window, cx| this.navigate(url.clone(), window, cx)
                    });
                    let bookmark_store = bookmark_store.clone();
                    let url = bookmark.url.clone();
                    right_click_menu(("browser-bookmark-menu", index))
                        .trigger(move |_, _, _| {
                            Button::new(
                                ("browser-bookmark", index),
                                util::truncate_and_trailoff(
                                    &bookmark.title,
                                    MAX_BOOKMARK_LABEL_CHARS,
                                ),
                            )
                            .label_size(LabelSize::Small)
                            .tooltip(Tooltip::text(bookmark.url))
                            .on_click(open_bookmark)
                        })
                        .menu(move |window, cx| {
                            let bookmark_store = bookmark_store.clone();
                            let url = url.clone();
                            ContextMenu::build(window, cx, move |menu, _, _| {
                                menu.entry("Remove Bookmark", None, move |_, cx| {
                                    bookmark_store.update(cx, |store, cx| store.remove(&url, cx));
                                })
                            })
                        })
                })),
        )
    }
}

impl EventEmitter<BrowserTabEvent> for BrowserTab {}

impl Focusable for BrowserTab {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.address_bar.focus_handle(cx)
    }
}

impl Render for BrowserTab {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .size_full()
            .bg(cx.theme().colors().editor_background)
            .child(self.render_toolbar(cx))
            .children(self.render_bookmarks_bar(cx))
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .child(self.page.render(cx.weak_entity())),
            )
    }
}

impl Item for BrowserTab {
    type Event = BrowserTabEvent;

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        tab_title(self.title.as_deref(), &self.current_url).into()
    }

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::Public))
    }

    fn tab_tooltip_text(&self, _cx: &App) -> Option<SharedString> {
        Some(self.current_url.clone().into())
    }

    fn to_item_events(event: &Self::Event, f: &mut dyn FnMut(ItemEvent)) {
        match event {
            BrowserTabEvent::UpdateTab | BrowserTabEvent::UrlChanged => f(ItemEvent::UpdateTab),
        }
    }

    fn include_in_nav_history() -> bool {
        false
    }

    fn deactivated(&mut self, _window: &mut Window, _cx: &mut Context<Self>) {
        self.page.hide();
    }

    fn added_to_workspace(
        &mut self,
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.window = window.window_handle();
        // Modals, notifications and zoomed panels are drawn over the page by
        // the workspace without this tab being repainted.
        let mut subscriptions =
            vec![
                cx.observe_in(workspace.modal_layer(), window, |this, _, window, cx| {
                    this.request_visibility_checks(VISIBILITY_CHECKS_AFTER_CHANGE, window, cx);
                }),
            ];
        if let Some(workspace) = workspace.weak_handle().upgrade() {
            subscriptions.push(cx.observe_in(&workspace, window, |this, _, window, cx| {
                this.request_visibility_checks(VISIBILITY_CHECKS_AFTER_CHANGE, window, cx);
            }));
        }
        self._workspace_subscriptions = subscriptions;
    }
}

impl SerializableItem for BrowserTab {
    fn serialized_item_kind() -> &'static str {
        "BrowserTab"
    }

    fn cleanup(
        workspace_id: WorkspaceId,
        alive_items: Vec<ItemId>,
        _window: &mut Window,
        cx: &mut App,
    ) -> Task<Result<()>> {
        let kvp = KeyValueStore::global(cx);
        cx.background_spawn(async move {
            let prefix = format!("{}:", i64::from(workspace_id));
            let keys = kvp.select_bound::<&str, String>(
                "SELECT key FROM scoped_kv_store WHERE namespace = (?)",
            )?(BROWSER_TABS_NAMESPACE)?;
            let alive: Vec<String> = alive_items
                .into_iter()
                .map(|item_id| BrowserTab::serialization_key(workspace_id, item_id))
                .collect();
            let scope = kvp.scoped(BROWSER_TABS_NAMESPACE);
            for key in keys {
                if key.starts_with(&prefix) && !alive.contains(&key) {
                    scope.delete(key).await.log_err();
                }
            }
            Ok(())
        })
    }

    fn deserialize(
        _project: Entity<Project>,
        _workspace: WeakEntity<Workspace>,
        workspace_id: WorkspaceId,
        item_id: ItemId,
        window: &mut Window,
        cx: &mut App,
    ) -> Task<Result<Entity<Self>>> {
        let url = KeyValueStore::global(cx)
            .scoped(BROWSER_TABS_NAMESPACE)
            .read(&Self::serialization_key(workspace_id, item_id))
            .log_err()
            .flatten()
            .filter(|url| !url.trim().is_empty());
        Task::ready(Ok(cx.new(|cx| Self::new(url, window, cx))))
    }

    fn serialize(
        &mut self,
        workspace: &mut Workspace,
        item_id: ItemId,
        _closing: bool,
        cx: &mut Context<Self>,
    ) -> Option<Task<Result<()>>> {
        let workspace_id = workspace.database_id()?;
        let key = Self::serialization_key(workspace_id, item_id);
        let url = self.current_url.clone();
        let kvp = KeyValueStore::global(cx);
        Some(cx.background_spawn(async move {
            kvp.scoped(BROWSER_TABS_NAMESPACE).write(key, url).await
        }))
    }

    fn should_serialize(&self, event: &Self::Event) -> bool {
        matches!(event, BrowserTabEvent::UrlChanged)
    }
}

#[cfg(not(target_os = "windows"))]
mod unsupported {
    use gpui::{AnyElement, Context, IntoElement, ParentElement, Styled, WeakEntity, Window, div};
    use ui::{Color, Label, LabelCommon as _};

    use crate::BrowserTab;

    /// Stands in for the native page on platforms without one.
    pub(crate) struct Page;

    impl Page {
        pub(crate) fn new(_cx: &mut Context<BrowserTab>) -> Self {
            Self
        }

        pub(crate) fn load_url(&self, _url: &str) {}

        pub(crate) fn reload(&self) {}

        pub(crate) fn go_back(&self) {}

        pub(crate) fn go_forward(&self) {}

        pub(crate) fn focus(&self) {}

        pub(crate) fn release_keyboard_focus(&self) {}

        pub(crate) fn hide(&self) {}

        pub(crate) fn render(&self, _tab: WeakEntity<BrowserTab>) -> AnyElement {
            div()
                .size_full()
                .p_4()
                .child(
                    Label::new("Browser tabs are only available on Windows for now")
                        .color(Color::Muted),
                )
                .into_any_element()
        }
    }

    impl BrowserTab {
        pub(crate) fn request_visibility_checks(
            &mut self,
            _frames: u8,
            _window: &mut Window,
            _cx: &mut Context<Self>,
        ) {
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn address_input_with_a_scheme_is_used_as_is() {
        for url in [
            "https://example.com/a?b=c#d",
            "http://localhost:3000",
            "file:///C:/Users/index.html",
            "about:blank",
        ] {
            assert_eq!(url_from_address_input(url).as_deref(), Some(url));
        }
        assert_eq!(
            url_from_address_input("  https://example.com  ").as_deref(),
            Some("https://example.com")
        );
    }

    #[test]
    fn address_input_that_looks_like_a_host_gets_https() {
        for (input, url) in [
            ("example.com", "https://example.com"),
            (
                "github.com/zed-industries/zed",
                "https://github.com/zed-industries/zed",
            ),
            (
                "docs.rs/gpui?search=Window",
                "https://docs.rs/gpui?search=Window",
            ),
            ("localhost", "http://localhost"),
            ("localhost:3000/app", "http://localhost:3000/app"),
            ("127.0.0.1:8080", "http://127.0.0.1:8080"),
            ("[::1]:8080", "http://[::1]:8080"),
        ] {
            assert_eq!(url_from_address_input(input).as_deref(), Some(url));
        }
    }

    #[test]
    fn other_address_input_becomes_a_search() {
        for (input, url) in [
            ("rust", "https://duckduckgo.com/?q=rust"),
            (
                "what is gpui?",
                "https://duckduckgo.com/?q=what%20is%20gpui%3F",
            ),
            (
                "example.com is down",
                "https://duckduckgo.com/?q=example.com%20is%20down",
            ),
            ("a&b=c", "https://duckduckgo.com/?q=a%26b%3Dc"),
            (
                "rust: the book",
                "https://duckduckgo.com/?q=rust%3A%20the%20book",
            ),
            ("std::fmt", "https://duckduckgo.com/?q=std%3A%3Afmt"),
            (".hidden", "https://duckduckgo.com/?q=.hidden"),
        ] {
            assert_eq!(url_from_address_input(input).as_deref(), Some(url));
        }
    }

    #[test]
    fn blank_address_input_is_ignored() {
        assert_eq!(url_from_address_input(""), None);
        assert_eq!(url_from_address_input("   \t"), None);
    }

    #[test]
    fn tab_title_falls_back_to_host_then_browser() {
        assert_eq!(
            tab_title(Some("Example Domain"), "https://example.com/"),
            "Example Domain"
        );
        assert_eq!(tab_title(None, "https://example.com/path"), "example.com");
        assert_eq!(
            tab_title(Some("   "), "https://example.com/path"),
            "example.com"
        );
        assert_eq!(tab_title(None, "about:blank"), "Browser");
        assert_eq!(tab_title(None, "not a url"), "Browser");
    }

    #[test]
    fn tab_title_is_truncated() {
        let title = tab_title(Some(&"x".repeat(100)), "https://example.com/");
        assert!(title.chars().count() <= MAX_TAB_TITLE_CHARS + 1);
        assert!(title.starts_with("xxxx"));
        assert_ne!(title, "x".repeat(100));
    }

    #[test]
    fn bookmark_title_falls_back_to_host_then_url() {
        assert_eq!(
            bookmark_title(Some("Example"), "https://example.com/"),
            "Example"
        );
        assert_eq!(bookmark_title(None, "https://example.com/a"), "example.com");
        assert_eq!(bookmark_title(Some(""), "about:blank"), "about:blank");
    }
}
