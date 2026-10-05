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
    Action, AnyWindowHandle, App, Bounds, ClipboardItem, Context, DismissEvent, Entity,
    EventEmitter, FocusHandle, Focusable, IntoElement, ParentElement, Pixels, Point, Render,
    SharedString, Styled, Subscription, Task, WeakEntity, Window, actions, anchored, deferred, div,
    point, px,
};
use project::Project;
use ui::{
    ContextMenu, ContextMenuEntry, Icon, IconButton, IconName, Tooltip, prelude::*,
    right_click_menu,
};
use util::ResultExt as _;
use workspace::{
    Item, ItemId, NewCenterTerminal, NewTerminal, Pane, SerializableItem, Workspace, WorkspaceId,
    item::ItemEvent,
};

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

/// Only `http` and `https` links are opened from a page's context menu: the
/// URL comes from the page, which could otherwise point a new tab at a local
/// file or a script.
fn is_web_url(url: &str) -> bool {
    url::Url::parse(url).is_ok_and(|url| matches!(url.scheme(), "http" | "https"))
}

/// Where in the window a point of the page is. `x` and `y` are CSS pixels
/// from the top left of the page's viewport, which are GPUI's logical pixels
/// as long as the page is not zoomed. They come from the page, so they are
/// kept inside `content_bounds`.
fn page_point_in_window(content_bounds: Bounds<Pixels>, x: f32, y: f32) -> Point<Pixels> {
    let inside = |offset: f32, extent: Pixels| {
        let extent = f32::from(extent).max(0.);
        if offset.is_finite() {
            px(offset.clamp(0., extent))
        } else {
            px(0.)
        }
    };
    point(
        content_bounds.origin.x + inside(x, content_bounds.size.width),
        content_bounds.origin.y + inside(y, content_bounds.size.height),
    )
}

fn insert_text_script(text: &str) -> Result<String> {
    // Encoded as a JSON string, which is also a JavaScript string literal,
    // so nothing in the clipboard can end the literal and run as script.
    let literal = serde_json::to_string(text)?;
    Ok(format!(
        "document.execCommand('insertText', false, {literal});"
    ))
}

/// A right-click in the page, as reported by the page itself. Every field
/// is untrusted and only ever used as data.
// Only the Windows page reports right-clicks, so elsewhere nothing builds
// one of these.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct PageContextMenuRequest {
    /// CSS pixels from the top left of the page's viewport.
    x: f32,
    y: f32,
    link_url: Option<String>,
    image_url: Option<String>,
    selected_text: Option<String>,
    editable: bool,
}

#[cfg(target_os = "windows")]
pub(crate) enum PageEvent {
    Navigated(String),
    TitleChanged(String),
    /// The page asked for a new window (`target="_blank"`, `window.open`).
    NewWindowRequested(String),
    ContextMenuRequested(PageContextMenuRequest),
    /// A mouse button went down in the page, which GPUI does not see.
    PointerDown,
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
    page_context_menu: Option<(Entity<ContextMenu>, Point<Pixels>, Subscription)>,
    window: AnyWindowHandle,
    workspace: Option<WeakEntity<Workspace>>,
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
            page_context_menu: None,
            window: window.window_handle(),
            workspace: None,
            _subscriptions: subscriptions,
            _workspace_subscriptions: Vec::new(),
        }
    }

    /// Opens a new browser tab in the active pane.
    pub fn open_new(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
        let tab = cx.new(|cx| Self::new(None, window, cx));
        workspace.add_item_to_active_pane(Box::new(tab), None, true, window, cx);
    }

    /// Opens `url` in a new browser tab in `pane`.
    pub fn open_url(
        url: String,
        pane: Entity<Pane>,
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let tab = cx.new(|cx| Self::new(Some(url), window, cx));
        workspace.add_item(pane, Box::new(tab), None, true, true, window, cx);
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

    // Only the Windows page reports right-clicks; elsewhere this is never
    // called.
    #[cfg_attr(not(target_os = "windows"), allow(dead_code))]
    fn deploy_page_context_menu(
        &mut self,
        request: PageContextMenuRequest,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(content_bounds) = self.page.content_bounds() else {
            return;
        };
        let position = page_point_in_window(content_bounds, request.x, request.y);
        let menu = self.build_page_context_menu(request, window, cx);
        // The right-click gave the page the keyboard, and the menu is
        // driven by keys that only reach GPUI's own window.
        self.page.release_keyboard_focus();
        window.focus(&menu.focus_handle(cx), cx);
        let subscription =
            cx.subscribe_in(&menu, window, |this, menu, _: &DismissEvent, window, cx| {
                this.page_context_menu_dismissed(menu, window, cx);
            });
        self.page_context_menu = Some((menu, position, subscription));
        self.request_visibility_checks(VISIBILITY_CHECKS_AFTER_CHANGE, window, cx);
        cx.notify();
    }

    fn page_context_menu_dismissed(
        &mut self,
        menu: &Entity<ContextMenu>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let is_current = self
            .page_context_menu
            .as_ref()
            .is_some_and(|(current, _, _)| current == menu);
        if !is_current {
            return;
        }
        // The menu took the keyboard from the page. Unless the chosen entry
        // or a click moved the focus somewhere else, the page gets it back.
        if window.is_window_active() && menu.focus_handle(cx).contains_focused(window, cx) {
            self.page.focus();
        }
        self.close_page_context_menu(window, cx);
    }

    fn close_page_context_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.page_context_menu.take().is_some() {
            self.request_visibility_checks(VISIBILITY_CHECKS_AFTER_CHANGE, window, cx);
            cx.notify();
        }
    }

    fn build_page_context_menu(
        &self,
        request: PageContextMenuRequest,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<ContextMenu> {
        let PageContextMenuRequest {
            link_url,
            image_url,
            selected_text,
            editable,
            ..
        } = request;
        let tab = cx.weak_entity();
        let focus_handle = self.focus_handle(cx);
        let can_go_back = self.can_go_back;
        let can_go_forward = self.can_go_forward;
        let page_url = self.current_url.clone();
        let bookmarked = self
            .bookmark_store
            .as_ref()
            .is_some_and(|store| store.read(cx).is_bookmarked(&self.current_url));
        let web_link_url = link_url.clone().filter(|url| is_web_url(url));
        let clipboard_has_text = editable
            && cx
                .read_from_clipboard()
                .and_then(|item| item.text())
                .is_some();
        let has_target_entries =
            link_url.is_some() || image_url.is_some() || selected_text.is_some() || editable;
        // Claude Code's action is looked up by name since this crate can't
        // depend on agent_ui.
        let new_claude_tab = cx.build_action("agent::NewClaudeTab", None).ok();

        ContextMenu::build(window, cx, move |menu, _, _| {
            // The new-tab entries are workspace actions, which have to be
            // dispatched from inside the workspace rather than from the menu.
            menu.context(focus_handle)
                .item(
                    ContextMenuEntry::new("Back")
                        .disabled(!can_go_back)
                        .handler(tab_entry_handler(&tab, |tab, _| tab.page.go_back())),
                )
                .item(
                    ContextMenuEntry::new("Forward")
                        .disabled(!can_go_forward)
                        .handler(tab_entry_handler(&tab, |tab, _| tab.page.go_forward())),
                )
                .entry(
                    "Reload",
                    None,
                    tab_entry_handler(&tab, |tab, _| tab.page.reload()),
                )
                .when(has_target_entries, |menu| menu.separator())
                .when_some(web_link_url, |menu, url| {
                    menu.entry("Open Link in New Browser Tab", None, {
                        let tab = tab.clone();
                        move |window, cx| open_link_in_new_tab(&tab, url.clone(), window, cx)
                    })
                })
                .when_some(link_url, |menu, url| {
                    menu.entry("Copy Link Address", None, copy_handler(url))
                })
                .when_some(image_url, |menu, url| {
                    menu.entry("Copy Image Address", None, copy_handler(url))
                })
                .when_some(selected_text.clone(), |menu, text| {
                    menu.entry("Copy", None, copy_handler(text))
                })
                .when(editable, |menu| {
                    menu.when_some(selected_text, |menu, text| {
                        menu.entry(
                            "Cut",
                            None,
                            tab_entry_handler(&tab, move |tab, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(text.clone()));
                                tab.run_editing_script("document.execCommand('delete');");
                            }),
                        )
                    })
                    .item(
                        ContextMenuEntry::new("Paste")
                            .disabled(!clipboard_has_text)
                            .handler(tab_entry_handler(&tab, |tab, cx| {
                                let Some(text) =
                                    cx.read_from_clipboard().and_then(|item| item.text())
                                else {
                                    return;
                                };
                                if let Some(script) = insert_text_script(&text).log_err() {
                                    tab.run_editing_script(&script);
                                }
                            })),
                    )
                    .entry(
                        "Select All",
                        None,
                        tab_entry_handler(&tab, |tab, _| {
                            tab.run_editing_script("document.execCommand('selectAll');");
                        }),
                    )
                })
                .separator()
                .entry(
                    if bookmarked {
                        "Remove Bookmark"
                    } else {
                        "Bookmark This Page"
                    },
                    None,
                    tab_entry_handler(&tab, |tab, cx| tab.toggle_bookmark(cx)),
                )
                .entry("Copy Page Address", None, copy_handler(page_url))
                .separator()
                .when_some(new_claude_tab, |menu, action| {
                    menu.action("New Claude Code Tab", action)
                })
                .action("New Browser Tab", NewTab.boxed_clone())
                .action("New Terminal", NewTerminal::default().boxed_clone())
                .action(
                    "New Center Terminal",
                    NewCenterTerminal::default().boxed_clone(),
                )
        })
    }

    fn run_editing_script(&self, script: &str) {
        // Editing commands act on the focused document, and the menu took
        // the focus away from the page.
        self.page.focus();
        self.page.run_script(script);
    }

    #[cfg(target_os = "windows")]
    fn handle_page_event(&mut self, event: PageEvent, window: &mut Window, cx: &mut Context<Self>) {
        self.can_go_back = self.page.can_go_back();
        self.can_go_forward = self.page.can_go_forward();
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
            PageEvent::ContextMenuRequested(request) => {
                self.deploy_page_context_menu(request, window, cx)
            }
            PageEvent::PointerDown => self.close_page_context_menu(window, cx),
        }
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

fn tab_entry_handler(
    tab: &WeakEntity<BrowserTab>,
    handler: impl Fn(&mut BrowserTab, &mut Context<BrowserTab>) + 'static,
) -> impl Fn(&mut Window, &mut App) + 'static {
    let tab = tab.clone();
    move |_, cx| {
        tab.update(cx, |tab, cx| handler(tab, cx)).log_err();
    }
}

fn copy_handler(text: String) -> impl Fn(&mut Window, &mut App) + 'static {
    move |_, cx| cx.write_to_clipboard(ClipboardItem::new_string(text.clone()))
}

fn open_link_in_new_tab(
    tab: &WeakEntity<BrowserTab>,
    url: String,
    window: &mut Window,
    cx: &mut App,
) {
    let Some(tab) = tab.upgrade() else {
        return;
    };
    let Some(workspace) = tab.read(cx).workspace.clone() else {
        return;
    };
    // Not done from inside an update of `tab`: adding an item to its pane
    // deactivates it, which updates it.
    workspace
        .update(cx, |workspace, cx| {
            let pane = workspace
                .pane_for(&tab)
                .unwrap_or_else(|| workspace.active_pane().clone());
            BrowserTab::open_url(url, pane, workspace, window, cx);
        })
        .log_err();
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
            .children(self.page_context_menu.as_ref().map(|(menu, position, _)| {
                deferred(anchored().position(*position).child(menu.clone())).with_priority(1)
            }))
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
        self.workspace = Some(workspace.weak_handle());
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
    use gpui::{
        AnyElement, Bounds, Context, IntoElement, ParentElement, Pixels, Styled, WeakEntity,
        Window, div,
    };
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

        pub(crate) fn run_script(&self, _script: &str) {}

        pub(crate) fn content_bounds(&self) -> Option<Bounds<Pixels>> {
            None
        }

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
    fn only_web_urls_open_from_the_context_menu() {
        assert!(is_web_url("https://example.com/a?b=c"));
        assert!(is_web_url("http://localhost:3000"));
        for url in [
            "file:///C:/Windows/system.ini",
            "javascript:alert(1)",
            "data:text/html,<script>alert(1)</script>",
            "about:blank",
            "mailto:someone@example.com",
            "example.com",
            "",
        ] {
            assert!(!is_web_url(url), "{url}");
        }
    }

    #[test]
    fn page_points_are_offset_by_the_content_origin_and_kept_inside() {
        let content = Bounds::new(point(px(100.), px(40.)), gpui::size(px(800.), px(600.)));
        assert_eq!(
            page_point_in_window(content, 10., 20.),
            point(px(110.), px(60.))
        );
        assert_eq!(
            page_point_in_window(content, -5., 9000.),
            point(px(100.), px(640.))
        );
        assert_eq!(
            page_point_in_window(content, f32::NAN, f32::INFINITY),
            point(px(100.), px(40.))
        );
    }

    #[test]
    fn pasted_text_is_a_string_literal_in_the_script() -> Result<()> {
        assert_eq!(
            insert_text_script("hello")?,
            r#"document.execCommand('insertText', false, "hello");"#
        );
        assert_eq!(
            insert_text_script("\"); alert(1); (\"\n")?,
            r#"document.execCommand('insertText', false, "\"); alert(1); (\"\n");"#
        );
        Ok(())
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
