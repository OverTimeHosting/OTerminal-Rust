//! The native page of a browser tab on Windows: a WebView2 hosted in a child
//! window of the GPUI window, and the logic that keeps that child window in
//! step with what GPUI draws.
//!
//! A child window is composed above everything GPUI paints and stays on
//! screen when GPUI stops painting the element it stands for, so it is shown
//! only while the content element is in the rendered frame with nothing in
//! front of it, and hidden otherwise.

use std::{cell::Cell, ffi::c_void, num::NonZeroIsize, rc::Rc};

use anyhow::{Context as _, Result, anyhow};
use futures::{
    StreamExt as _,
    channel::mpsc::{self, UnboundedSender},
};
use gpui::{
    AnyElement, AppContext as _, Bounds, Context, DispatchPhase, Hitbox, HitboxBehavior,
    IntoElement, MouseDownEvent, MouseMoveEvent, MouseUpEvent, ParentElement, Pixels, Point,
    SharedString, Styled, Task, WeakEntity, Window, canvas, div, point, px,
};
use raw_window_handle::{
    HandleError, HasWindowHandle, RawWindowHandle, Win32WindowHandle, WindowHandle,
};
use ui::{Color, FluentBuilder as _, Label, LabelCommon as _, LabelSize};
use util::ResultExt as _;
use windows::Win32::{
    Foundation::HWND,
    UI::{Input::KeyboardAndMouse::GetFocus, WindowsAndMessaging::IsChild},
};
use wry::{
    NewWindowResponse, Rect, WebContext, WebViewBuilder, WebViewExtWindows as _,
    dpi::{PhysicalPosition, PhysicalSize},
};

use crate::{BrowserTab, PageEvent};

/// Overlays that open a few frames after the input that caused them are
/// still noticed.
const CHECKS_AFTER_INPUT: u8 = 3;
const MAX_SAMPLE_COLUMNS: usize = 16;
const MAX_SAMPLE_ROWS: usize = 28;
const SAMPLE_COLUMN_SPACING: f32 = 120.;
const SAMPLE_ROW_SPACING: f32 = 32.;
const SAMPLE_EDGE_INSET: f32 = 12.;

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
    obscured: bool,
    obscured_at: Option<Point<Pixels>>,
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
            obscured: false,
            obscured_at: None,
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

        div()
            .relative()
            .size_full()
            .child(content)
            .when(self.obscured, |this| {
                this.child(
                    div().absolute().inset_0().p_4().child(
                        Label::new("The page is hidden while something is shown over it.")
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    ),
                )
            })
            .into_any_element()
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

/// Points spread over `bounds`, dense enough that a menu, popover, modal or
/// notification laid over the page covers at least one of them.
fn sample_points(bounds: Bounds<Pixels>) -> Vec<Point<Pixels>> {
    // Dock resize handles overlap the edges of a pane by a few pixels and
    // take the mouse there without covering the page.
    let inset = SAMPLE_EDGE_INSET;
    let width = f32::from(bounds.size.width) - 2. * inset;
    let height = f32::from(bounds.size.height) - 2. * inset;
    if width <= 0. || height <= 0. {
        return Vec::new();
    }
    let columns =
        ((width / SAMPLE_COLUMN_SPACING).ceil() as usize + 1).clamp(2, MAX_SAMPLE_COLUMNS);
    let rows = ((height / SAMPLE_ROW_SPACING).ceil() as usize + 1).clamp(2, MAX_SAMPLE_ROWS);
    let left = f32::from(bounds.origin.x) + inset;
    let top = f32::from(bounds.origin.y) + inset;
    let mut points = Vec::with_capacity(columns * rows);
    for row in 0..rows {
        for column in 0..columns {
            points.push(point(
                px(left + width * column as f32 / (columns - 1) as f32),
                px(top + height * row as f32 / (rows - 1) as f32),
            ));
        }
    }
    points
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

    /// Shows the page where its content element was last painted, or hides
    /// it when something in the rendered frame is in front of that element.
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

        // Anything drawn over the page that takes the mouse (menus,
        // popovers, modals, notifications, a zoomed panel) also stops this
        // hitbox from being hovered there. A dragged tab or file has no
        // hitbox but is drawn over the page too.
        let obscured_at = if cx.has_active_drag() {
            Some(bounds.center())
        } else {
            self.page
                .obscured_at
                .into_iter()
                .filter(|point| bounds.contains(point))
                .chain(sample_points(bounds))
                .find(|point| !painted.hitbox.is_hovered_at(*point, window))
        };
        self.page.obscured_at = obscured_at;

        let obscured = obscured_at.is_some();
        if obscured {
            web_view.hide().log_err();
            // Nothing announces the overlay going away (it may be dismissed
            // from the keyboard), so keep looking until it has.
            self.request_visibility_checks(1, window, cx);
        } else {
            let shown = ParentWindow::new(window)
                .and_then(|parent| web_view.show_at(parent, bounds, window.scale_factor()));
            shown.log_err();
        }
        if obscured != self.page.obscured {
            self.page.obscured = obscured;
            cx.notify();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::size;

    fn rect(x: i32, y: i32, width: u32, height: u32) -> Rect {
        Rect {
            position: PhysicalPosition::new(x, y).into(),
            size: PhysicalSize::new(width, height).into(),
        }
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
    fn sample_points_cover_the_edges_and_stay_inside() {
        let bounds = Bounds::new(point(px(10.), px(20.)), size(px(1000.), px(500.)));
        let points = sample_points(bounds);
        assert!(points.len() <= MAX_SAMPLE_COLUMNS * MAX_SAMPLE_ROWS);
        assert!(points.iter().all(|point| bounds.contains(point)));
        assert!(points.contains(&point(
            px(10. + SAMPLE_EDGE_INSET),
            px(20. + SAMPLE_EDGE_INSET)
        )));
        assert!(points.contains(&point(
            px(1010. - SAMPLE_EDGE_INSET),
            px(520. - SAMPLE_EDGE_INSET)
        )));

        let mut columns: Vec<f32> = points.iter().map(|point| f32::from(point.x)).collect();
        columns.sort_by(f32::total_cmp);
        columns.dedup();
        assert!(
            columns
                .windows(2)
                .all(|pair| pair[1] - pair[0] <= SAMPLE_COLUMN_SPACING)
        );
    }

    #[test]
    fn sample_points_of_empty_bounds() {
        let bounds = Bounds::new(point(px(0.), px(0.)), size(px(1.), px(0.)));
        assert!(sample_points(bounds).is_empty());
    }
}
