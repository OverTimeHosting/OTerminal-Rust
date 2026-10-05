//! The native page of a browser tab on Windows: a WebView2 hosted in a child
//! window of the GPUI window, and the logic that keeps that child window in
//! step with what GPUI draws.
//!
//! A child window is composed above everything GPUI paints and stays on
//! screen when GPUI stops painting the element it stands for. So it is shown
//! only while the content element is in the rendered frame, and wherever
//! GPUI draws something in front of that element (a menu, a popover, a
//! modal) a hole is cut into the child window for it to show through.

use std::{
    cell::{Cell, RefCell},
    ffi::c_void,
    num::NonZeroIsize,
    rc::Rc,
};

use anyhow::{Context as _, Result, anyhow};
use futures::{
    StreamExt as _,
    channel::mpsc::{self, UnboundedSender},
};
use gpui::{
    AnyElement, AppContext as _, Bounds, Context, DispatchPhase, ExternalPaths, Hitbox,
    HitboxBehavior, IntoElement, MouseDownEvent, MouseMoveEvent, MouseUpEvent, ParentElement,
    Pixels, SharedString, Styled, Task, WeakEntity, Window, canvas, div, px,
};
use raw_window_handle::{
    HandleError, HasWindowHandle, RawWindowHandle, Win32WindowHandle, WindowHandle,
};
use serde::Deserialize;
use ui::{Color, Label, LabelCommon as _};
use util::ResultExt as _;
use windows::Win32::{
    Foundation::HWND,
    Graphics::Gdi::{CombineRgn, CreateRectRgn, DeleteObject, RGN_DIFF, SetWindowRgn},
    UI::{Input::KeyboardAndMouse::GetFocus, WindowsAndMessaging::IsChild},
};
use wry::{
    NewWindowResponse, Rect, WebContext, WebViewBuilder, WebViewExtWindows as _,
    dpi::{PhysicalPosition, PhysicalSize},
};

use workspace::{DraggedSelection, DraggedTab};

use crate::{BrowserTab, PageContextMenuRequest, PageEvent};

/// Overlays that open a few frames after the input that caused them are
/// still noticed.
const CHECKS_AFTER_INPUT: u8 = 3;
const MIN_OVERLAY_SIZE: Pixels = px(24.);

/// Runs in every document before its own scripts. It replaces WebView2's
/// context menu in the top document with a report of what was right-clicked,
/// and reports mouse presses, which GPUI cannot see inside the page's window.
/// The limits here are the ones `parse_page_message` enforces.
const PAGE_SCRIPT: &str = r#"(function () {
  try {
    var post = function (message) {
      try {
        window.ipc.postMessage(JSON.stringify(message));
      } catch (error) {}
    };
    var bounded = function (value, limit) {
      return typeof value === 'string' && value.length <= limit ? value : '';
    };
    window.addEventListener('pointerdown', function () {
      post({ kind: 'pointer-down' });
    }, true);
    if (window.top !== window) {
      return;
    }
    window.addEventListener('contextmenu', function (event) {
      try {
        event.preventDefault();
        var path = typeof event.composedPath === 'function' ? event.composedPath() : [];
        var target = path[0] || event.target;
        var element = target && target.nodeType === 1 ? target : (target && target.parentElement) || null;
        var tag = element ? String(element.tagName).toUpperCase() : '';
        var type = tag === 'INPUT' ? String(element.type).toLowerCase() : '';
        var field = tag === 'TEXTAREA' || (tag === 'INPUT' &&
          !/^(button|checkbox|color|file|hidden|image|radio|range|reset|submit)$/.test(type));
        var link = element && element.closest ? element.closest('a[href], area[href]') : null;
        var selection = type === 'password' ? '' : String(window.getSelection() || '');
        post({
          kind: 'context-menu',
          x: event.clientX,
          y: event.clientY,
          link: link ? bounded(link.href, 8192) : '',
          image: tag === 'IMG' ? bounded(element.currentSrc || element.src, 8192) : '',
          selection: selection.slice(0, 10000),
          editable: !!element &&
            ((field && !element.disabled && !element.readOnly) || element.isContentEditable === true)
        });
      } catch (error) {}
    }, true);
  } catch (error) {}
})();"#;

/// Any page content can post messages, so anything larger than the script
/// above could send is dropped unread.
const MAX_PAGE_MESSAGE_BYTES: usize = 256 * 1024;
const MAX_PAGE_URL_CHARS: usize = 8192;
const MAX_SELECTION_CHARS: usize = 10_000;
const MAX_PAGE_COORDINATE: f64 = 1_000_000.;

#[derive(Debug, PartialEq)]
enum PageMessage {
    ContextMenu(PageContextMenuRequest),
    PointerDown,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct RawPageMessage {
    kind: String,
    x: f64,
    y: f64,
    link: String,
    image: String,
    selection: String,
    editable: bool,
}

/// Reads a message posted by [`PAGE_SCRIPT`]. Pages can post anything, so
/// whatever is not such a message is an error, and what is kept of one is
/// bounded.
fn parse_page_message(message: &str) -> Result<PageMessage> {
    if message.len() > MAX_PAGE_MESSAGE_BYTES {
        return Err(anyhow!("message of {} bytes is too large", message.len()));
    }
    let raw: RawPageMessage = serde_json::from_str(message).context("not a page message")?;
    match raw.kind.as_str() {
        "pointer-down" => Ok(PageMessage::PointerDown),
        "context-menu" => {
            if !raw.x.is_finite() || !raw.y.is_finite() {
                return Err(anyhow!("context menu position is not a number"));
            }
            let coordinate = |value: f64| value.clamp(0., MAX_PAGE_COORDINATE) as f32;
            let url = |url: String| {
                Some(url).filter(|url| {
                    !url.is_empty()
                        && url.chars().count() <= MAX_PAGE_URL_CHARS
                        && url::Url::parse(url).is_ok()
                })
            };
            let selected_text: String = raw.selection.chars().take(MAX_SELECTION_CHARS).collect();
            Ok(PageMessage::ContextMenu(PageContextMenuRequest {
                x: coordinate(raw.x),
                y: coordinate(raw.y),
                link_url: url(raw.link),
                image_url: url(raw.image),
                selected_text: Some(selected_text).filter(|text| !text.trim().is_empty()),
                editable: raw.editable,
            }))
        }
        _ => Err(anyhow!("unknown kind of page message")),
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct ParentWindow(NonZeroIsize);

impl ParentWindow {
    fn new(window: &Window) -> Result<Self> {
        let handle =
            HasWindowHandle::window_handle(window).context("getting the native window handle")?;
        match handle.as_raw() {
            RawWindowHandle::Win32(handle) => Ok(Self(handle.hwnd)),
            other => Err(anyhow!("unsupported native window handle: {other:?}")),
        }
    }

    fn hwnd(&self) -> HWND {
        HWND(self.0.get() as *mut c_void)
    }
}

impl HasWindowHandle for ParentWindow {
    fn window_handle(&self) -> Result<WindowHandle<'_>, HandleError> {
        let handle = RawWindowHandle::Win32(Win32WindowHandle::new(self.0));
        // SAFETY: the handle came from a live GPUI window and is only handed
        // to Win32 calls, which reject a window that has since been destroyed.
        Ok(unsafe { WindowHandle::borrow_raw(handle) })
    }
}

fn wry_result<T>(result: wry::Result<T>) -> Result<T> {
    result.map_err(|error| anyhow!("{error}"))
}

/// GPUI lays out in logical pixels; the child window is placed in the
/// parent's client area in device pixels. Converting here, rather than
/// handing wry logical values, keeps the page on GPUI's own pixel grid
/// whatever DPI Windows reports for the child window.
fn physical_rect(bounds: Bounds<Pixels>, scale_factor: f32) -> Rect {
    let scale = |pixels: Pixels| (f32::from(pixels) * scale_factor).round() as i32;
    let left = scale(bounds.origin.x);
    let top = scale(bounds.origin.y);
    let right = scale(bounds.origin.x + bounds.size.width);
    let bottom = scale(bounds.origin.y + bounds.size.height);
    Rect {
        position: PhysicalPosition::new(left, top).into(),
        size: PhysicalSize::new((right - left).max(0) as u32, (bottom - top).max(0) as u32).into(),
    }
}

struct WebView {
    inner: wry::WebView,
    parent: Cell<ParentWindow>,
    placement: Cell<Option<Rect>>,
    visible: Cell<bool>,
    /// The holes last cut into the page's window, with the page size they
    /// were cut for.
    holes: RefCell<Option<(Vec<PageRect>, i32, i32)>>,
}

impl WebView {
    /// Blocks until WebView2 has started, pumping the Win32 message loop
    /// meanwhile. Must not be called while the `App` is borrowed (inside an
    /// entity or window update), because the pumped messages re-enter GPUI.
    fn build(
        parent: ParentWindow,
        url: &str,
        bounds: Bounds<Pixels>,
        scale_factor: f32,
        events: UnboundedSender<PageEvent>,
    ) -> Result<Self> {
        let send = move |event: PageEvent| {
            if let Err(error) = events.unbounded_send(event) {
                log::debug!("browser tab closed before a page event arrived: {error}");
            }
        };
        // Without an explicit folder WebView2 writes its profile next to the
        // executable, which an installed build may not be allowed to do.
        let mut web_context = WebContext::new(Some(paths::data_dir().join("browser")));
        let inner = WebViewBuilder::new_with_web_context(&mut web_context)
            .with_url(url)
            .with_bounds(physical_rect(bounds, scale_factor))
            .with_visible(false)
            .with_focused(false)
            .with_initialization_script(PAGE_SCRIPT)
            .with_ipc_handler({
                let send = send.clone();
                move |request| match parse_page_message(request.body()) {
                    Ok(PageMessage::ContextMenu(request)) => {
                        send(PageEvent::ContextMenuRequested(request))
                    }
                    Ok(PageMessage::PointerDown) => send(PageEvent::PointerDown),
                    Err(error) => log::debug!("ignoring a message from the page: {error:#}"),
                }
            })
            .with_on_page_load_handler({
                let send = send.clone();
                move |_, url| send(PageEvent::Navigated(url))
            })
            .with_document_title_changed_handler({
                let send = send.clone();
                move |title| send(PageEvent::TitleChanged(title))
            })
            .with_new_window_req_handler(move |url, _| {
                send(PageEvent::NewWindowRequested(url));
                NewWindowResponse::Deny
            })
            .build_as_child(&parent);
        Ok(Self {
            inner: wry_result(inner)?,
            parent: Cell::new(parent),
            placement: Cell::new(None),
            visible: Cell::new(false),
            holes: RefCell::new(None),
        })
    }

    fn show_at(
        &self,
        parent: ParentWindow,
        bounds: Bounds<Pixels>,
        scale_factor: f32,
    ) -> Result<()> {
        if self.parent.get() != parent {
            wry_result(self.inner.reparent(parent.0.get()))?;
            self.parent.set(parent);
            self.placement.set(None);
        }
        let rect = physical_rect(bounds, scale_factor);
        if self.placement.get() != Some(rect) {
            wry_result(self.inner.set_bounds(rect))?;
            self.placement.set(Some(rect));
        }
        if !self.visible.get() {
            wry_result(self.inner.set_visible(true))?;
            self.visible.set(true);
        }
        Ok(())
    }

    /// Cuts `holes` out of the page's window, so GPUI's drawing shows there
    /// and takes the mouse. Windows clips a child window (and the WebView2
    /// windows inside it) to its window region.
    fn set_holes(&self, holes: &[PageRect], width: i32, height: i32) -> Result<()> {
        let unchanged =
            self.holes
                .borrow()
                .as_ref()
                .is_some_and(|(current, current_width, current_height)| {
                    current == holes && (*current_width, *current_height) == (width, height)
                });
        if unchanged {
            return Ok(());
        }
        let mut page_window = HWND::default();
        // SAFETY: the controller is alive as long as `self.inner`, and the
        // regions are created, combined and released within this block; a
        // region handed to `SetWindowRgn` successfully is owned by the
        // system from then on.
        unsafe {
            self.inner
                .controller()
                .ParentWindow(&mut page_window)
                .context("getting the page's window")?;
            if holes.is_empty() {
                if SetWindowRgn(page_window, None, true) == 0 {
                    return Err(anyhow!("clearing the page window's region failed"));
                }
            } else {
                let region = CreateRectRgn(0, 0, width, height);
                for hole in holes {
                    let hole_region = CreateRectRgn(hole.left, hole.top, hole.right, hole.bottom);
                    CombineRgn(Some(region), Some(region), Some(hole_region), RGN_DIFF);
                    DeleteObject(hole_region.into()).ok().log_err();
                }
                if SetWindowRgn(page_window, Some(region), true) == 0 {
                    DeleteObject(region.into()).ok().log_err();
                    return Err(anyhow!("setting the page window's region failed"));
                }
            }
        }
        *self.holes.borrow_mut() = Some((holes.to_vec(), width, height));
        Ok(())
    }

    fn hide(&self) -> Result<()> {
        if !self.visible.get() {
            return Ok(());
        }
        // A hidden window keeps the keyboard focus, which would leave the
        // GPUI window deaf to typing.
        self.release_keyboard_focus()?;
        wry_result(self.inner.set_visible(false))?;
        self.visible.set(false);
        Ok(())
    }

    /// Whether a page, rather than GPUI's own window, receives typing.
    fn has_keyboard_focus(&self) -> bool {
        // SAFETY: both calls only read window state and accept any handle.
        unsafe {
            let focused = GetFocus();
            !focused.is_invalid() && IsChild(self.parent.get().hwnd(), focused).as_bool()
        }
    }

    fn release_keyboard_focus(&self) -> Result<()> {
        if self.has_keyboard_focus() {
            wry_result(self.inner.focus_parent())?;
        }
        Ok(())
    }
}

#[derive(Default)]
struct PaintTracker {
    generation: Cell<u64>,
    in_rendered_frame: Cell<bool>,
}

/// Lives in a mouse listener registered by the content element's paint, so
/// it is dropped when GPUI discards the frame holding that paint. If no
/// newer paint replaced it by then, the content left the screen (the tab was
/// switched away, its pane or workspace is no longer rendered, the item was
/// closed) and the native window must go with it.
struct PaintGuard {
    generation: u64,
    tracker: Rc<PaintTracker>,
    web_view: Rc<WebView>,
}

impl Drop for PaintGuard {
    fn drop(&mut self) {
        if self.tracker.generation.get() == self.generation {
            self.tracker.in_rendered_frame.set(false);
            self.web_view.hide().log_err();
        }
    }
}

struct PaintedContent {
    hitbox: Hitbox,
    bounds: Bounds<Pixels>,
}

enum Creation {
    NotStarted,
    InProgress,
    Finished,
    Failed(SharedString),
}

pub(crate) struct Page {
    web_view: Option<Rc<WebView>>,
    creation: Creation,
    events: UnboundedSender<PageEvent>,
    paint_tracker: Rc<PaintTracker>,
    painted: Option<PaintedContent>,
    remaining_checks: u8,
    check_scheduled: bool,
    _event_task: Task<()>,
}

impl Page {
    pub(crate) fn new(cx: &mut Context<BrowserTab>) -> Self {
        let (events, mut received_events) = mpsc::unbounded();
        let event_task = cx.spawn(async move |this, cx| {
            while let Some(event) = received_events.next().await {
                let Ok(window) = this.read_with(cx, |this, _| this.window) else {
                    break;
                };
                let this = this.clone();
                cx.update_window(window, |_, window, cx| {
                    this.update(cx, |this, cx| this.handle_page_event(event, window, cx))
                        .log_err();
                })
                .log_err();
            }
        });
        Self {
            web_view: None,
            creation: Creation::NotStarted,
            events,
            paint_tracker: Rc::default(),
            painted: None,
            remaining_checks: 0,
            check_scheduled: false,
            _event_task: event_task,
        }
    }

    pub(crate) fn load_url(&self, url: &str) {
        if let Some(web_view) = &self.web_view {
            wry_result(web_view.inner.load_url(url)).log_err();
        }
    }

    pub(crate) fn reload(&self) {
        if let Some(web_view) = &self.web_view {
            wry_result(web_view.inner.reload()).log_err();
        }
    }

    pub(crate) fn go_back(&self) {
        if let Some(web_view) = &self.web_view {
            wry_result(web_view.inner.go_back()).log_err();
        }
    }

    pub(crate) fn go_forward(&self) {
        if let Some(web_view) = &self.web_view {
            wry_result(web_view.inner.go_forward()).log_err();
        }
    }

    pub(crate) fn can_go_back(&self) -> bool {
        self.web_view
            .as_ref()
            .and_then(|web_view| wry_result(web_view.inner.can_go_back()).log_err())
            .unwrap_or(false)
    }

    pub(crate) fn can_go_forward(&self) -> bool {
        self.web_view
            .as_ref()
            .and_then(|web_view| wry_result(web_view.inner.can_go_forward()).log_err())
            .unwrap_or(false)
    }

    /// The page's own idea of its URL, which also follows in-page (history
    /// API) navigations that raise no page load event.
    pub(crate) fn url(&self) -> Option<String> {
        let web_view = self.web_view.as_ref()?;
        wry_result(web_view.inner.url()).log_err()
    }

    pub(crate) fn has_keyboard_focus(&self) -> bool {
        self.web_view
            .as_ref()
            .is_some_and(|web_view| web_view.has_keyboard_focus())
    }

    pub(crate) fn focus(&self) {
        if let Some(web_view) = &self.web_view
            && web_view.visible.get()
        {
            wry_result(web_view.inner.focus()).log_err();
        }
    }

    pub(crate) fn release_keyboard_focus(&self) {
        if let Some(web_view) = &self.web_view {
            web_view.release_keyboard_focus().log_err();
        }
    }

    pub(crate) fn run_script(&self, script: &str) {
        if let Some(web_view) = &self.web_view {
            wry_result(web_view.inner.evaluate_script(script)).log_err();
        }
    }

    /// Where in the window the page was last painted.
    pub(crate) fn content_bounds(&self) -> Option<Bounds<Pixels>> {
        self.painted.as_ref().map(|painted| painted.bounds)
    }

    /// Hides the page until its content element is painted again.
    pub(crate) fn hide(&self) {
        // Otherwise a check already scheduled for the frame before that
        // paint would show the page again.
        self.paint_tracker.in_rendered_frame.set(false);
        if let Some(web_view) = &self.web_view {
            web_view.hide().log_err();
        }
    }

    pub(crate) fn render(&self, tab: WeakEntity<BrowserTab>) -> AnyElement {
        if let Creation::Failed(message) = &self.creation {
            return div()
                .size_full()
                .p_4()
                .child(Label::new(message.clone()).color(Color::Error))
                .into_any_element();
        }

        let web_view = self.web_view.clone();
        let tracker = self.paint_tracker.clone();
        let content = canvas(
            |bounds, window, _| window.insert_hitbox(bounds, HitboxBehavior::Normal),
            move |bounds, hitbox, window, cx| {
                if bounds.size.width <= px(0.) || bounds.size.height <= px(0.) {
                    return;
                }
                if let Some(web_view) = web_view {
                    let generation = tracker.generation.get() + 1;
                    tracker.generation.set(generation);
                    tracker.in_rendered_frame.set(true);
                    let guard = PaintGuard {
                        generation,
                        tracker,
                        web_view,
                    };
                    // These listeners see every click in the window. A click
                    // that reaches GPUI at all landed outside the page.
                    window.on_mouse_event({
                        let tab = tab.clone();
                        move |_: &MouseDownEvent, phase, window, cx| {
                            if phase == DispatchPhase::Capture {
                                guard.web_view.release_keyboard_focus().log_err();
                                request_checks(&tab, CHECKS_AFTER_INPUT, window, cx);
                            }
                        }
                    });
                    window.on_mouse_event({
                        let tab = tab.clone();
                        move |_: &MouseUpEvent, phase, window, cx| {
                            if phase == DispatchPhase::Capture {
                                request_checks(&tab, CHECKS_AFTER_INPUT, window, cx);
                            }
                        }
                    });
                    window.on_mouse_event({
                        let tab = tab.clone();
                        move |event: &MouseMoveEvent, phase, window, cx| {
                            if phase == DispatchPhase::Capture && event.pressed_button.is_some() {
                                request_checks(&tab, CHECKS_AFTER_INPUT, window, cx);
                            }
                        }
                    });
                }
                // Deferred until the draw is over: the checks read the frame
                // being painted right now, and creating the web view needs
                // the entity.
                window.defer(cx, move |window, cx| {
                    tab.update(cx, |tab, cx| {
                        tab.content_painted(PaintedContent { hitbox, bounds }, window, cx)
                    })
                    .log_err();
                });
            },
        )
        .size_full();

        content.into_any_element()
    }
}

fn request_checks(
    tab: &WeakEntity<BrowserTab>,
    frames: u8,
    window: &mut Window,
    cx: &mut gpui::App,
) {
    tab.update(cx, |tab, cx| {
        tab.request_visibility_checks(frames, window, cx)
    })
    .log_err();
}

impl BrowserTab {
    fn content_painted(
        &mut self,
        painted: PaintedContent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let bounds = painted.bounds;
        self.page.painted = Some(painted);
        match self.page.creation {
            Creation::NotStarted => self.create_web_view(bounds, window, cx),
            Creation::Finished => self.sync_visibility(window, cx),
            Creation::InProgress | Creation::Failed(_) => {}
        }
    }

    fn create_web_view(
        &mut self,
        bounds: Bounds<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let parent = match ParentWindow::new(window) {
            Ok(parent) => parent,
            Err(error) => {
                self.web_view_failed(error, cx);
                return;
            }
        };
        self.page.creation = Creation::InProgress;
        let url = self.current_url.clone();
        let scale_factor = window.scale_factor();
        let events = self.page.events.clone();
        cx.spawn(async move |this, cx| {
            // Built here, between updates, because `WebView::build` pumps
            // the message loop and so must run with the `App` not borrowed.
            let result = WebView::build(parent, &url, bounds, scale_factor, events);
            this.update(cx, |this, cx| match result {
                Ok(web_view) => {
                    if this.current_url != url {
                        wry_result(web_view.inner.load_url(&this.current_url)).log_err();
                    }
                    this.page.web_view = Some(Rc::new(web_view));
                    this.page.creation = Creation::Finished;
                    cx.notify();
                }
                Err(error) => this.web_view_failed(error, cx),
            })
            .log_err();
        })
        .detach();
    }

    fn web_view_failed(&mut self, error: anyhow::Error, cx: &mut Context<Self>) {
        log::error!("failed to create the browser tab's web view: {error:#}");
        self.page.creation =
            Creation::Failed(format!("The page could not be shown: {error:#}").into());
        cx.notify();
    }

    /// Asks for the page's visibility to be re-evaluated after each of the
    /// next `frames` frames. Only ever scheduled in response to input or
    /// while the page is obscured, so an idle window schedules nothing.
    pub(crate) fn request_visibility_checks(
        &mut self,
        frames: u8,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.page.web_view.is_none() || !self.page.paint_tracker.in_rendered_frame.get() {
            return;
        }
        self.page.remaining_checks = self.page.remaining_checks.max(frames);
        if self.page.check_scheduled {
            return;
        }
        self.page.check_scheduled = true;
        let this = cx.weak_entity();
        window.on_next_frame(move |window, cx| {
            this.update(cx, |this, cx| {
                this.page.check_scheduled = false;
                this.page.remaining_checks = this.page.remaining_checks.saturating_sub(1);
                this.sync_visibility(window, cx);
                if this.page.remaining_checks > 0 {
                    this.request_visibility_checks(this.page.remaining_checks, window, cx);
                }
            })
            .log_err();
        });
    }

    /// Shows the page where its content element was last painted, with holes
    /// where GPUI draws over that element, or hides it when it is covered
    /// entirely.
    fn sync_visibility(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(web_view) = self.page.web_view.clone() else {
            return;
        };
        if !self.page.paint_tracker.in_rendered_frame.get() {
            return;
        }
        let Some(painted) = &self.page.painted else {
            return;
        };
        let bounds = painted.bounds;
        let scale_factor = window.scale_factor();

        // Anything GPUI draws over the page that takes the mouse (menus,
        // popovers, modals, notifications, a zoomed panel) is a hitbox in
        // front of this one.
        let occluded_areas = painted.hitbox.occluded_areas(window);
        let mut holes = page_holes(bounds, &occluded_areas, scale_factor);
        let page = physical_rect(bounds, scale_factor);
        let page_size = page.size.to_physical::<i32>(1.);
        // A backdrop (of a modal, say) takes the mouse everywhere without
        // being drawn over the whole page. Only what is in front of it has
        // to show over the page.
        if let Some(backdrop) = holes
            .iter()
            .position(|hole| hole.covers(page_size.width, page_size.height))
        {
            holes.truncate(backdrop);
        }
        // A panel or another pane zoomed over this one is opaque. A dragged
        // tab or file has no hitbox, and the drop targets GPUI draws for it
        // would be behind the page.
        let hidden = self.is_under_zoomed_view(cx)
            || cx.has_active_drag_of::<DraggedTab>()
            || cx.has_active_drag_of::<DraggedSelection>()
            || cx.has_active_drag_of::<ExternalPaths>();

        if hidden {
            web_view.hide().log_err();
        } else {
            let shown = ParentWindow::new(window).and_then(|parent| {
                web_view.show_at(parent, bounds, scale_factor)?;
                web_view.set_holes(&holes, page_size.width, page_size.height)
            });
            shown.log_err();
        }

        // Nothing announces an overlay moving or going away (it may be
        // dismissed from the keyboard), so keep looking while there is one.
        // Dock resize handles overlap the edges of a pane permanently and
        // are too thin to be one.
        let has_overlay = occluded_areas.iter().any(|area| {
            area.size.width >= MIN_OVERLAY_SIZE && area.size.height >= MIN_OVERLAY_SIZE
        });
        if hidden || has_overlay {
            self.request_visibility_checks(1, window, cx);
        }
    }

    fn is_under_zoomed_view(&self, cx: &Context<Self>) -> bool {
        let Some(workspace) = self
            .workspace
            .as_ref()
            .and_then(|workspace| workspace.upgrade())
        else {
            return false;
        };
        let workspace = workspace.read(cx);
        let Some(zoomed) = workspace.zoomed_item().and_then(|zoomed| zoomed.upgrade()) else {
            return false;
        };
        let own_pane = workspace.pane_for(&cx.entity());
        own_pane.is_none_or(|pane| pane.entity_id() != zoomed.entity_id())
    }
}

/// A rectangle in the page's own device pixels, relative to its top left.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PageRect {
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
}

impl PageRect {
    fn covers(&self, width: i32, height: i32) -> bool {
        self.left <= 0 && self.top <= 0 && self.right >= width && self.bottom >= height
    }
}

/// Where GPUI draws over the page, front to back, on the same pixel grid as
/// [`physical_rect`].
fn page_holes(
    bounds: Bounds<Pixels>,
    occluded_areas: &[Bounds<Pixels>],
    scale_factor: f32,
) -> Vec<PageRect> {
    let scale = |pixels: Pixels| (f32::from(pixels) * scale_factor).round() as i32;
    let page_left = scale(bounds.origin.x);
    let page_top = scale(bounds.origin.y);
    occluded_areas
        .iter()
        .map(|area| PageRect {
            left: scale(area.origin.x) - page_left,
            top: scale(area.origin.y) - page_top,
            right: scale(area.origin.x + area.size.width) - page_left,
            bottom: scale(area.origin.y + area.size.height) - page_top,
        })
        .filter(|hole| hole.right > hole.left && hole.bottom > hole.top)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{point, size};

    fn rect(x: i32, y: i32, width: u32, height: u32) -> Rect {
        Rect {
            position: PhysicalPosition::new(x, y).into(),
            size: PhysicalSize::new(width, height).into(),
        }
    }

    fn context_menu_request(message: &str) -> Option<PageContextMenuRequest> {
        match parse_page_message(message) {
            Ok(PageMessage::ContextMenu(request)) => Some(request),
            Ok(PageMessage::PointerDown) | Err(_) => None,
        }
    }

    #[test]
    fn a_context_menu_message_is_read() {
        let request = context_menu_request(
            r#"{"kind":"context-menu","x":120.5,"y":48,"link":"https://example.com/a?b=c",
                "image":"https://example.com/cat.png","selection":"some text","editable":true,
                "unknown":[1,2,3]}"#,
        );
        assert_eq!(
            request,
            Some(PageContextMenuRequest {
                x: 120.5,
                y: 48.,
                link_url: Some("https://example.com/a?b=c".to_string()),
                image_url: Some("https://example.com/cat.png".to_string()),
                selected_text: Some("some text".to_string()),
                editable: true,
            })
        );
    }

    #[test]
    fn missing_fields_of_a_context_menu_message_are_empty() {
        assert_eq!(
            context_menu_request(r#"{"kind":"context-menu"}"#),
            Some(PageContextMenuRequest {
                x: 0.,
                y: 0.,
                link_url: None,
                image_url: None,
                selected_text: None,
                editable: false,
            })
        );
        let request = context_menu_request(
            r#"{"kind":"context-menu","link":"","image":"not a url","selection":"  \n"}"#,
        );
        assert_eq!(
            request.map(|request| (request.link_url, request.image_url, request.selected_text)),
            Some((None, None, None))
        );
    }

    #[test]
    fn a_pointer_down_message_is_read() {
        assert_eq!(
            parse_page_message(r#"{"kind":"pointer-down"}"#).ok(),
            Some(PageMessage::PointerDown)
        );
    }

    #[test]
    fn ill_typed_and_unknown_messages_are_rejected() {
        for message in [
            "",
            "garbage",
            "null",
            "42",
            r#""context-menu""#,
            "[]",
            "{}",
            r#"{"kind":"something-else"}"#,
            r#"{"kind":7}"#,
            r#"{"kind":"context-menu","x":"12"}"#,
            r#"{"kind":"context-menu","y":null}"#,
            r#"{"kind":"context-menu","x":1e999}"#,
            r#"{"kind":"context-menu","link":["https://example.com"]}"#,
            r#"{"kind":"context-menu","selection":{"text":"a"}}"#,
            r#"{"kind":"context-menu","editable":"yes"}"#,
            r#"{"kind":"context-menu""#,
        ] {
            assert!(parse_page_message(message).is_err(), "{message}");
        }
    }

    #[test]
    fn context_menu_positions_are_bounded() {
        let request = context_menu_request(r#"{"kind":"context-menu","x":-40,"y":1e300}"#)
            .map(|request| (request.x, request.y));
        assert_eq!(request, Some((0., MAX_PAGE_COORDINATE as f32)));
    }

    #[test]
    fn oversized_strings_are_dropped_or_cut() -> Result<()> {
        let long_url = format!("https://example.com/{}", "a".repeat(MAX_PAGE_URL_CHARS));
        let message = serde_json::to_string(&serde_json::json!({
            "kind": "context-menu",
            "link": long_url,
            "image": long_url,
            "selection": "é".repeat(MAX_SELECTION_CHARS + 500),
        }))?;
        let request = context_menu_request(&message).context("message was rejected")?;
        assert_eq!(request.link_url, None);
        assert_eq!(request.image_url, None);
        assert_eq!(
            request.selected_text.map(|text| text.chars().count()),
            Some(MAX_SELECTION_CHARS)
        );

        let huge = serde_json::to_string(&serde_json::json!({
            "kind": "context-menu",
            "selection": "a".repeat(MAX_PAGE_MESSAGE_BYTES),
        }))?;
        assert!(parse_page_message(&huge).is_err());
        Ok(())
    }

    #[test]
    fn physical_rect_scales_logical_bounds() {
        let bounds = Bounds::new(point(px(100.), px(40.)), size(px(800.), px(600.)));
        assert_eq!(physical_rect(bounds, 1.), rect(100, 40, 800, 600));
        assert_eq!(physical_rect(bounds, 1.25), rect(125, 50, 1000, 750));
        assert_eq!(physical_rect(bounds, 1.5), rect(150, 60, 1200, 900));
    }

    #[test]
    fn physical_rect_rounds_edges_not_sizes() {
        let bounds = Bounds::new(point(px(0.5), px(0.)), size(px(10.5), px(1.)));
        // Left edge 0.75 -> 1, right edge 16.5 -> 17 (rounds half away from zero).
        assert_eq!(physical_rect(bounds, 1.5), rect(1, 0, 16, 2));
    }

    #[test]
    fn page_holes_are_relative_to_the_page() {
        let bounds = Bounds::new(point(px(100.), px(40.)), size(px(800.), px(600.)));
        let menu = Bounds::new(point(px(300.), px(40.)), size(px(200.), px(120.)));
        assert_eq!(
            page_holes(bounds, &[menu], 1.),
            [PageRect {
                left: 200,
                top: 0,
                right: 400,
                bottom: 120
            }]
        );
        assert_eq!(
            page_holes(bounds, &[menu], 1.5),
            [PageRect {
                left: 300,
                top: 0,
                right: 600,
                bottom: 180
            }]
        );
        assert!(page_holes(bounds, &[], 1.).is_empty());
    }

    #[test]
    fn a_hole_over_the_whole_page_covers_it() {
        let bounds = Bounds::new(point(px(100.), px(40.)), size(px(800.), px(600.)));
        let holes = page_holes(bounds, &[bounds], 1.25);
        assert_eq!(holes.len(), 1);
        assert!(holes[0].covers(1000, 750));

        let menu = Bounds::new(point(px(300.), px(40.)), size(px(200.), px(120.)));
        assert!(!page_holes(bounds, &[menu], 1.25)[0].covers(1000, 750));
    }
}
