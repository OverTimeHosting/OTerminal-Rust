use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Context as _, Result};
use collections::{HashMap, HashSet};
use db::kvp::KeyValueStore;
use futures::StreamExt as _;
use gpui::{
    Action, AsyncWindowContext, ClipboardItem, Entity, EventEmitter, FocusHandle, Focusable,
    PromptLevel, ScrollHandle, Subscription, Task, WeakEntity, relative,
};
use othcloud_client::{
    ApiError, DevEnvState as DevEnvStatusState, DevEnvStatus, LoadState, OthcloudAccount,
    OthcloudAccountEvent, OthcloudApi, Row, User,
};
use release_channel::AppVersion;
use serde::{Deserialize, Serialize};
use settings::SettingsStore;
use ui::{
    Avatar, Banner, CommonAnimationExt, ContextMenu, Disclosure, Divider, Indicator, ListHeader,
    ListItem, Severity, Tooltip, prelude::*, right_click_menu,
};
use util::ResultExt as _;
use workspace::{
    Workspace,
    dock::{DockPosition, Panel, PanelEvent},
};

use crate::{
    OpenConsole, OpenDashboard, OpenGameServers, PastePairingCode, RefreshServices, SignIn,
    SignOut, ToggleFocus, report_api_error,
};

const OTHCLOUD_PANEL_KEY: &str = "OthcloudPanel";
const REFRESH_ON_SHOW_AFTER: Duration = Duration::from_secs(60);
const MAX_CONCURRENT_STATUS_REQUESTS: usize = 4;

#[derive(Serialize, Deserialize, Default)]
struct SerializedOthcloudPanel {
    #[serde(default)]
    position: Option<SerializedDockPosition>,
}

#[derive(Serialize, Deserialize, Clone, Copy)]
#[serde(rename_all = "snake_case")]
enum SerializedDockPosition {
    Left,
    Right,
}

/// The status of a dev environment as the panel shows it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DevEnvState {
    Running,
    Stopped,
    Unavailable,
    Unknown,
}

#[derive(Clone, Debug)]
struct DevEnvRowState {
    state: DevEnvState,
    /// Why the environment is unavailable, or the last error.
    detail: Option<SharedString>,
    /// A start/stop request is in flight.
    busy: bool,
}

impl DevEnvRowState {
    fn from_status(status: &DevEnvStatus) -> Self {
        let (state, detail) = dev_env_state(status);
        Self {
            state,
            detail,
            busy: false,
        }
    }
}

pub struct OthcloudPanel {
    focus_handle: FocusHandle,
    workspace: WeakEntity<Workspace>,
    position: DockPosition,
    expanded_projects: HashSet<String>,
    dev_env_status: HashMap<String, DevEnvRowState>,
    dev_env_tasks: HashMap<String, Task<()>>,
    status_refresh_task: Option<Task<()>>,
    scroll_handle: ScrollHandle,
    last_refresh_requested: Option<Instant>,
    /// Whether the panel is currently shown in its dock.
    active: bool,
    /// Services changed while the panel was hidden; dev environment statuses
    /// are fetched the next time it is shown. This keeps hidden panels (other
    /// windows or project tabs) from issuing requests nobody looks at.
    statuses_dirty: bool,
    pending_serialization: Task<Option<()>>,
    _subscriptions: Vec<Subscription>,
}

impl OthcloudPanel {
    pub async fn load(
        workspace: WeakEntity<Workspace>,
        mut cx: AsyncWindowContext,
    ) -> Result<Entity<Self>> {
        let kvp = cx.update(|_, cx| KeyValueStore::global(cx))?;
        let serialized = cx
            .background_spawn(async move { kvp.read_kvp(OTHCLOUD_PANEL_KEY) })
            .await
            .context("reading OTHCloud panel state")
            .log_err()
            .flatten()
            .and_then(|json| serde_json::from_str::<SerializedOthcloudPanel>(&json).log_err());

        workspace.update_in(&mut cx, |workspace, window, cx| {
            let weak_workspace = workspace.weak_handle();
            cx.new(|cx| {
                let mut panel = Self::new(weak_workspace, window, cx);
                if let Some(position) = serialized.and_then(|serialized| serialized.position) {
                    panel.position = match position {
                        SerializedDockPosition::Left => DockPosition::Left,
                        SerializedDockPosition::Right => DockPosition::Right,
                    };
                }
                panel
            })
        })
    }

    fn new(workspace: WeakEntity<Workspace>, _window: &mut Window, cx: &mut Context<Self>) -> Self {
        let mut subscriptions = Vec::new();
        if let Some(account) = OthcloudAccount::global(cx) {
            subscriptions.push(cx.observe(&account, |_, _, cx| cx.notify()));
            subscriptions.push(cx.subscribe(&account, |this, _, event, cx| {
                match event {
                    OthcloudAccountEvent::ServicesChanged => {
                        if this.active {
                            this.refresh_dev_env_statuses(cx);
                        } else {
                            this.statuses_dirty = true;
                        }
                    }
                    OthcloudAccountEvent::SignedOut => {
                        this.dev_env_status.clear();
                        this.dev_env_tasks.clear();
                        this.status_refresh_task = None;
                        this.statuses_dirty = false;
                    }
                    OthcloudAccountEvent::SignedIn | OthcloudAccountEvent::UserChanged => {}
                }
                cx.notify();
            }));
        }
        if let Some(github) = othcloud_github::GithubAccountStore::global(cx) {
            subscriptions.push(cx.observe(&github, |_, _, cx| cx.notify()));
        }

        Self {
            focus_handle: cx.focus_handle(),
            workspace,
            position: DockPosition::Right,
            expanded_projects: HashSet::default(),
            dev_env_status: HashMap::default(),
            dev_env_tasks: HashMap::default(),
            status_refresh_task: None,
            scroll_handle: ScrollHandle::new(),
            last_refresh_requested: None,
            active: false,
            // Services may already have been loaded by the account before the
            // panel was created; statuses are fetched once the panel is shown.
            statuses_dirty: true,
            pending_serialization: Task::ready(None),
            _subscriptions: subscriptions,
        }
    }

    fn serialize(&mut self, cx: &mut Context<Self>) {
        let position = match self.position {
            DockPosition::Left => SerializedDockPosition::Left,
            _ => SerializedDockPosition::Right,
        };
        let Some(json) = serde_json::to_string(&SerializedOthcloudPanel {
            position: Some(position),
        })
        .log_err() else {
            return;
        };
        let kvp = KeyValueStore::global(cx);
        self.pending_serialization = cx.background_spawn(async move {
            kvp.write_kvp(OTHCLOUD_PANEL_KEY.into(), json)
                .await
                .log_err()
        });
    }

    fn account(cx: &App) -> Option<Entity<OthcloudAccount>> {
        OthcloudAccount::global(cx)
    }

    fn api(cx: &App) -> Option<Arc<OthcloudApi>> {
        Self::account(cx).and_then(|account| account.read(cx).api())
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        self.last_refresh_requested = Some(Instant::now());
        crate::refresh_services(cx);
    }

    fn refresh_if_stale(&mut self, cx: &mut Context<Self>) {
        let Some(account) = Self::account(cx) else {
            return;
        };
        let account = account.read(cx);
        if !account.is_signed_in() {
            return;
        }
        let last_loaded = match account.services_state() {
            LoadState::Loading => return,
            LoadState::Loaded { at } => Some(*at),
            LoadState::Idle | LoadState::Error { .. } => None,
        };
        let last = match (last_loaded, self.last_refresh_requested) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (a, b) => a.or(b),
        };
        if last.is_none_or(|last| last.elapsed() >= REFRESH_ON_SHOW_AFTER) {
            self.refresh(cx);
        }
    }

    /// Fetches the live status of every dev environment (application), at
    /// most [`MAX_CONCURRENT_STATUS_REQUESTS`] at a time.
    fn refresh_dev_env_statuses(&mut self, cx: &mut Context<Self>) {
        self.statuses_dirty = false;
        let Some(account) = Self::account(cx) else {
            return;
        };
        let account = account.read(cx);
        let Some(api) = account.api() else {
            return;
        };
        let ids: Vec<String> = account
            .services()
            .and_then(|services| services.applications.as_ref())
            .map(|applications| applications.iter().map(|row| row.id.clone()).collect())
            .unwrap_or_default();
        self.dev_env_status
            .retain(|id, _| ids.iter().any(|known| known == id));
        if ids.is_empty() {
            self.status_refresh_task = None;
            return;
        }

        let workspace = self.workspace.clone();
        self.status_refresh_task = Some(cx.spawn(async move |this, cx| {
            let results: Vec<(String, Result<DevEnvStatus, ApiError>)> = futures::stream::iter(ids)
                .map(|id| {
                    let api = api.clone();
                    async move {
                        let result = api.dev_env_status(&id).await;
                        (id, result)
                    }
                })
                .buffer_unordered(MAX_CONCURRENT_STATUS_REQUESTS)
                .collect()
                .await;

            this.update(cx, |this, cx| {
                for (id, result) in results {
                    // Don't clobber the optimistic state of an in-flight start/stop.
                    if this.dev_env_status.get(&id).is_some_and(|state| state.busy) {
                        continue;
                    }
                    match result {
                        Ok(status) => {
                            this.dev_env_status
                                .insert(id, DevEnvRowState::from_status(&status));
                        }
                        Err(error) if error.is_unauthorized() => {
                            report_api_error(&workspace, error, cx);
                            break;
                        }
                        Err(error) => {
                            let message = error.friendly_message(&crate::othcloud_host());
                            log::warn!("othcloud: dev environment {id} status: {message}");
                            this.dev_env_status.insert(
                                id,
                                DevEnvRowState {
                                    state: DevEnvState::Unknown,
                                    detail: Some(message.into()),
                                    busy: false,
                                },
                            );
                        }
                    }
                }
                cx.notify();
            })
            .ok();
        }));
    }

    fn start_dev_env(&mut self, id: String, cx: &mut Context<Self>) {
        let Some(api) = Self::api(cx) else {
            return;
        };
        let version = AppVersion::global(cx);
        let version = format!("{}.{}.{}", version.major, version.minor, version.patch);
        self.set_dev_env_busy(&id, true, cx);
        let workspace = self.workspace.clone();
        let task_id = id.clone();
        let task = cx.spawn(async move |this, cx| {
            let result = api.dev_env_start(&id, Some(version), false).await;
            this.update(cx, |this, cx| {
                this.dev_env_tasks.remove(&id);
                match result {
                    Ok(status) => {
                        this.dev_env_status
                            .insert(id, DevEnvRowState::from_status(&status));
                    }
                    Err(error) => {
                        this.set_dev_env_busy(&id, false, cx);
                        report_api_error(&workspace, error, cx);
                    }
                }
                cx.notify();
            })
            .ok();
        });
        self.dev_env_tasks.insert(task_id, task);
    }

    fn confirm_stop_dev_env(&mut self, id: String, window: &mut Window, cx: &mut Context<Self>) {
        let answer = window.prompt(
            PromptLevel::Warning,
            "Stop dev environment?",
            Some("Anyone connected is disconnected. Your files are kept."),
            &["Stop", "Cancel"],
            cx,
        );
        cx.spawn(async move |this, cx| {
            if answer.await.ok() == Some(0) {
                this.update(cx, |this, cx| this.stop_dev_env(id, cx)).ok();
            }
        })
        .detach();
    }

    fn stop_dev_env(&mut self, id: String, cx: &mut Context<Self>) {
        let Some(api) = Self::api(cx) else {
            return;
        };
        self.set_dev_env_busy(&id, true, cx);
        let workspace = self.workspace.clone();
        let task_id = id.clone();
        let task = cx.spawn(async move |this, cx| {
            let result = api.dev_env_stop(&id).await;
            this.update(cx, |this, cx| {
                this.dev_env_tasks.remove(&id);
                match result {
                    Ok(_) => {
                        this.dev_env_status.insert(
                            id,
                            DevEnvRowState {
                                state: DevEnvState::Stopped,
                                detail: None,
                                busy: false,
                            },
                        );
                    }
                    Err(error) => {
                        this.set_dev_env_busy(&id, false, cx);
                        report_api_error(&workspace, error, cx);
                    }
                }
                cx.notify();
            })
            .ok();
        });
        self.dev_env_tasks.insert(task_id, task);
    }

    fn set_dev_env_busy(&mut self, id: &str, busy: bool, cx: &mut Context<Self>) {
        self.dev_env_status
            .entry(id.to_string())
            .or_insert_with(|| DevEnvRowState {
                state: DevEnvState::Unknown,
                detail: None,
                busy,
            })
            .busy = busy;
        cx.notify();
    }

    fn toggle_project(&mut self, id: &str, cx: &mut Context<Self>) {
        if !self.expanded_projects.remove(id) {
            self.expanded_projects.insert(id.to_string());
        }
        cx.notify();
    }

    // ---------------------------------------------------------------------
    // Rendering
    // ---------------------------------------------------------------------

    fn render_signed_out(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors();
        div().p_2().child(
            v_flex()
                .p_4()
                .gap_2()
                .bg(colors.surface_background)
                .border_1()
                .border_color(colors.border)
                .child(
                    h_flex()
                        .gap_2()
                        .child(gpui::img("images/oth_logo_28.png").flex_none().size(px(28.)))
                        .child(Headline::new("Sign in to OTHCloud").size(HeadlineSize::Small)),
                )
                .child(
                    Label::new(
                        "Link this terminal to your othcloud.xyz account to see and manage your projects.",
                    )
                    .size(LabelSize::Small)
                    .color(Color::Muted),
                )
                .child(
                    v_flex()
                        .pt_1()
                        .gap_1()
                        .child(
                            Button::new("othcloud-sign-in", "Sign in at othcloud.xyz")
                                .style(ButtonStyle::Filled)
                                .full_width()
                                .on_click(|_, window, cx| {
                                    window.dispatch_action(SignIn.boxed_clone(), cx)
                                }),
                        )
                        .child(
                            Button::new("othcloud-paste-code", "Paste pairing link or code")
                                .style(ButtonStyle::Subtle)
                                .full_width()
                                .on_click(|_, window, cx| {
                                    window.dispatch_action(PastePairingCode.boxed_clone(), cx)
                                }),
                        ),
                ),
        )
    }

    fn render_header(&self, user: Option<&User>, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors();
        let services_state = Self::account(cx)
            .map(|account| account.read(cx).services_state().clone())
            .unwrap_or(LoadState::Idle);
        let is_loading = matches!(services_state, LoadState::Loading);
        let refresh_tooltip: SharedString = match &services_state {
            LoadState::Loaded { at } => {
                format!("Refresh (updated {})", format_ago(at.elapsed())).into()
            }
            _ => "Refresh".into(),
        };

        let avatar = match user.and_then(user_avatar_url) {
            Some(url) => Avatar::new(url).size(rems(1.75)).into_any_element(),
            None => div()
                .size(rems(1.75))
                .flex()
                .items_center()
                .justify_center()
                .border_1()
                .border_color(colors.border)
                .child(
                    Icon::new(IconName::Person)
                        .size(IconSize::Small)
                        .color(Color::Muted),
                )
                .into_any_element(),
        };

        let name: SharedString = user
            .map(user_display_name)
            .unwrap_or_else(|| "Signed in".into());
        let email = user.and_then(user_email);
        let role = user.and_then(user_role_label);

        let refresh_button = if is_loading {
            div()
                .size_6()
                .flex()
                .items_center()
                .justify_center()
                .child(
                    Icon::new(IconName::RotateCw)
                        .size(IconSize::Small)
                        .color(Color::Muted)
                        .with_rotate_animation(1),
                )
                .into_any_element()
        } else {
            IconButton::new("othcloud-refresh", IconName::RotateCw)
                .icon_size(IconSize::Small)
                .tooltip(Tooltip::text(refresh_tooltip))
                .on_click(|_, window, cx| window.dispatch_action(RefreshServices.boxed_clone(), cx))
                .into_any_element()
        };

        h_flex()
            .w_full()
            .px_2()
            .py_2()
            .gap_2()
            .border_b_1()
            .border_color(colors.border)
            .child(avatar)
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .child(
                        h_flex()
                            .gap_1p5()
                            .min_w_0()
                            .child(Label::new(name).size(LabelSize::Small).truncate())
                            .children(role.map(|role| {
                                div().px_1().border_1().border_color(colors.border).child(
                                    Label::new(role).size(LabelSize::XSmall).color(Color::Muted),
                                )
                            })),
                    )
                    .children(email.map(|email| {
                        Label::new(email)
                            .size(LabelSize::XSmall)
                            .color(Color::Muted)
                            .truncate()
                    })),
            )
            .child(
                h_flex()
                    .gap_0p5()
                    .child(
                        IconButton::new("othcloud-open-console", IconName::ArrowUpRight)
                            .icon_size(IconSize::Small)
                            .tooltip(Tooltip::text("Open OTHCloud Console"))
                            .on_click(|_, window, cx| {
                                window.dispatch_action(OpenConsole.boxed_clone(), cx)
                            }),
                    )
                    .child(refresh_button)
                    .child(
                        IconButton::new("othcloud-sign-out", IconName::Exit)
                            .icon_size(IconSize::Small)
                            .tooltip(Tooltip::text("Sign out"))
                            .on_click(|_, window, cx| {
                                window.dispatch_action(SignOut.boxed_clone(), cx)
                            }),
                    ),
            )
    }

    fn render_stale_banner(&self, message: &str) -> impl IntoElement {
        div().px_2().pt_2().child(
            Banner::new()
                .severity(Severity::Warning)
                .wrap_content(true)
                .child(
                    Label::new(format!("{message}. Showing the last loaded list."))
                        .size(LabelSize::Small),
                )
                .action_slot(
                    Button::new("othcloud-retry", "Retry")
                        .label_size(LabelSize::Small)
                        .on_click(|_, window, cx| {
                            window.dispatch_action(RefreshServices.boxed_clone(), cx)
                        }),
                ),
        )
    }

    fn render_skeleton(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors();
        v_flex()
            .px_2()
            .py_2()
            .gap_1()
            .children([0.7_f32, 0.5, 0.6].into_iter().map(|fraction| {
                h_flex()
                    .h_8()
                    .gap_2()
                    .child(div().size_2().bg(colors.element_background))
                    .child(
                        div()
                            .h_2()
                            .w(relative(fraction))
                            .bg(colors.element_background),
                    )
            }))
    }

    fn render_section_header(
        &self,
        title: &'static str,
        count: Option<usize>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let colors = cx.theme().colors();
        ListHeader::new(title).end_slot::<Div>(count.map(|count| {
            div().px_1().border_1().border_color(colors.border).child(
                Label::new(count.to_string())
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
            )
        }))
    }

    fn render_empty(
        &self,
        id: &'static str,
        message: &'static str,
        button: Option<(&'static str, Box<dyn Action>)>,
    ) -> impl IntoElement {
        v_flex()
            .px_3()
            .py_2()
            .gap_1()
            .child(
                Label::new(message)
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .children(button.map(|(label, action)| {
                Button::new(id, label)
                    .label_size(LabelSize::Small)
                    .color(Color::Accent)
                    .on_click(move |_, window, cx| window.dispatch_action(action.boxed_clone(), cx))
            }))
    }

    fn render_projects(&self, projects: &[Row], cx: &mut Context<Self>) -> AnyElement {
        if projects.is_empty() {
            return self
                .render_empty(
                    "othcloud-empty-projects",
                    "No projects yet.",
                    Some(("Open dashboard", OpenDashboard.boxed_clone())),
                )
                .into_any_element();
        }

        let mut rows = Vec::new();
        for (ix, project) in projects.iter().enumerate() {
            let expanded = self.expanded_projects.contains(&project.id);
            let has_children = !project.children.is_empty();
            let project_id = project.id.clone();
            let url = row_url(project, "/dashboard");

            let item = ListItem::new(("othcloud-project", ix))
                .spacing(ui::ListItemSpacing::Sparse)
                .start_slot(
                    h_flex()
                        .gap_1()
                        .child(if has_children {
                            Disclosure::new(("othcloud-project-disclosure", ix), expanded)
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.toggle_project(&project_id, cx)
                                }))
                                .into_any_element()
                        } else {
                            div().w_4().into_any_element()
                        })
                        .child(
                            Icon::new(IconName::Folder)
                                .size(IconSize::Small)
                                .color(Color::Muted),
                        ),
                )
                .child(row_label(project, cx))
                .end_slot::<AnyElement>(status_dot(project.status.as_deref()))
                .on_click({
                    let url = url.clone();
                    move |_, _, cx| cx.open_url(&url)
                });
            rows.push(self.with_row_context_menu(("othcloud-project-menu", ix), item, url, None));

            if expanded {
                for (child_ix, child) in project.children.iter().enumerate() {
                    let url = row_url(child, "/dashboard");
                    let item = ListItem::new(SharedString::from(format!(
                        "othcloud-project-{ix}-env-{child_ix}"
                    )))
                    .indent_level(1)
                    .indent_step_size(px(20.))
                    .start_slot(
                        Icon::new(IconName::Box)
                            .size(IconSize::Small)
                            .color(Color::Muted),
                    )
                    .child(row_label(child, cx))
                    .end_slot::<AnyElement>(status_dot(child.status.as_deref()))
                    .on_click({
                        let url = url.clone();
                        move |_, _, cx| cx.open_url(&url)
                    });
                    rows.push(self.with_row_context_menu(
                        SharedString::from(format!("othcloud-project-{ix}-env-{child_ix}-menu")),
                        item,
                        url,
                        None,
                    ));
                }
            }
        }
        v_flex().children(rows).into_any_element()
    }

    fn render_dev_envs(&self, applications: Option<&[Row]>, cx: &mut Context<Self>) -> AnyElement {
        let Some(applications) = applications else {
            return self
                .render_empty(
                    "othcloud-dev-envs-unsupported",
                    "This will show up once othcloud.xyz is updated.",
                    None,
                )
                .into_any_element();
        };
        if applications.is_empty() {
            return self
                .render_empty(
                    "othcloud-empty-dev-envs",
                    "No dev environments yet.",
                    Some(("Open dashboard", OpenDashboard.boxed_clone())),
                )
                .into_any_element();
        }

        let mut sorted: Vec<(&Row, DevEnvRowState)> = applications
            .iter()
            .map(|row| {
                let state = self
                    .dev_env_status
                    .get(&row.id)
                    .cloned()
                    .unwrap_or_else(|| DevEnvRowState {
                        state: state_from_status_str(row.status.as_deref()),
                        detail: None,
                        busy: false,
                    });
                (row, state)
            })
            .collect();
        // Running first, keeping the server's order otherwise.
        sorted.sort_by_key(|(_, state)| state.state != DevEnvState::Running);

        let rows = sorted
            .into_iter()
            .map(|(row, state)| {
                // Key elements by the environment id, not the sorted index, so
                // hover/menu/spinner state stays with the right row on reorder.
                let key = row.id.as_str();
                let el_id =
                    |part: &str| SharedString::from(format!("othcloud-dev-env-{part}-{key}"));
                let url = row_url(row, "/dashboard");
                let dot_color = match state.state {
                    DevEnvState::Running => Color::Success,
                    DevEnvState::Stopped | DevEnvState::Unknown => Color::Muted,
                    DevEnvState::Unavailable => Color::Warning,
                };
                let secondary = meta_line(row, &["project", "toolchain"]);

                let action_button = if state.busy {
                    div()
                        .size_6()
                        .flex()
                        .items_center()
                        .justify_center()
                        .child(
                            Icon::new(IconName::LoadCircle)
                                .size(IconSize::Small)
                                .color(Color::Muted)
                                .with_keyed_rotate_animation(el_id("spinner"), 1),
                        )
                        .into_any_element()
                } else if state.state == DevEnvState::Running {
                    let id = row.id.clone();
                    IconButton::new(el_id("stop"), IconName::Stop)
                        .icon_size(IconSize::Small)
                        .tooltip(Tooltip::text("Stop"))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.confirm_stop_dev_env(id.clone(), window, cx)
                        }))
                        .into_any_element()
                } else {
                    let id = row.id.clone();
                    let unavailable = state.state == DevEnvState::Unavailable;
                    // Show why the last status check failed, if it did.
                    let tooltip: SharedString = match (unavailable, state.detail) {
                        (true, detail) => detail.unwrap_or_else(|| "Unavailable".into()),
                        (false, Some(detail)) => format!("Start ({detail})").into(),
                        (false, None) => "Start".into(),
                    };
                    IconButton::new(el_id("start"), IconName::PlayFilled)
                        .icon_size(IconSize::Small)
                        .disabled(unavailable)
                        .tooltip(Tooltip::text(tooltip))
                        .on_click(
                            cx.listener(move |this, _, _, cx| this.start_dev_env(id.clone(), cx)),
                        )
                        .into_any_element()
                };

                let open_url = url.clone();
                let item = ListItem::new(el_id("row"))
                    .spacing(ui::ListItemSpacing::Sparse)
                    .start_slot(Indicator::dot().color(dot_color))
                    .child(
                        v_flex()
                            .min_w_0()
                            .child(
                                Label::new(row.name.clone())
                                    .size(LabelSize::Small)
                                    .truncate(),
                            )
                            .children(secondary.map(|secondary| {
                                Label::new(secondary)
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted)
                                    .truncate()
                            })),
                    )
                    .end_slot(
                        h_flex().gap_0p5().child(action_button).child(
                            IconButton::new(el_id("open"), IconName::ArrowUpRight)
                                .icon_size(IconSize::Small)
                                .tooltip(Tooltip::text("Open on OTHCloud"))
                                .on_click(move |_, _, cx| cx.open_url(&open_url)),
                        ),
                    );
                self.with_row_context_menu(el_id("menu"), item, url, None)
            })
            .collect::<Vec<_>>();
        v_flex().children(rows).into_any_element()
    }

    fn render_game_servers(&self, servers: Option<&[Row]>, _cx: &mut Context<Self>) -> AnyElement {
        let servers = servers.unwrap_or_default();
        if servers.is_empty() {
            return self
                .render_empty(
                    "othcloud-empty-game-servers",
                    "No game servers yet.",
                    Some(("Browse game servers", OpenGameServers.boxed_clone())),
                )
                .into_any_element();
        }
        let rows = servers
            .iter()
            .enumerate()
            .map(|(ix, row)| {
                let url = row_url(row, "/dashboard/games");
                let address = row.meta.get("address").cloned();
                let secondary = meta_line(row, &["type", "address"]);
                let item = ListItem::new(("othcloud-game-server", ix))
                    .spacing(ui::ListItemSpacing::Sparse)
                    .start_slot::<AnyElement>(status_dot(row.status.as_deref()))
                    .child(
                        v_flex()
                            .min_w_0()
                            .child(
                                Label::new(row.name.clone())
                                    .size(LabelSize::Small)
                                    .truncate(),
                            )
                            .children(secondary.map(|secondary| {
                                Label::new(secondary)
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted)
                                    .truncate()
                            })),
                    )
                    .on_click({
                        let url = url.clone();
                        move |_, _, cx| cx.open_url(&url)
                    });
                self.with_row_context_menu(("othcloud-game-server-menu", ix), item, url, address)
            })
            .collect::<Vec<_>>();
        v_flex().children(rows).into_any_element()
    }

    /// Wraps a row in a right-click menu with Open in Browser, Copy Link,
    /// (optionally) Copy Server Address and Refresh.
    fn with_row_context_menu(
        &self,
        id: impl Into<ElementId>,
        item: ListItem,
        url: String,
        address: Option<String>,
    ) -> AnyElement {
        right_click_menu(id)
            .trigger(move |_, _, _| item)
            .menu(move |window, cx| {
                let url = url.clone();
                let address = address.clone();
                ContextMenu::build(window, cx, move |menu, _, _| {
                    let open_url = url.clone();
                    let copy_url = url.clone();
                    let mut menu = menu
                        .entry("Open in Browser", None, move |_, cx| cx.open_url(&open_url))
                        .entry("Copy Link", None, move |_, cx| {
                            cx.write_to_clipboard(ClipboardItem::new_string(copy_url.clone()))
                        });
                    if let Some(address) = address {
                        menu = menu.entry("Copy Server Address", None, move |_, cx| {
                            cx.write_to_clipboard(ClipboardItem::new_string(address.clone()))
                        });
                    }
                    menu.separator()
                        .entry("Refresh", None, move |_, cx| crate::refresh_services(cx))
                })
            })
            .into_any_element()
    }

    /// GitHub: the account git uses on github.com, and cloning.
    fn render_github(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors();
        let (label, problem, source) = othcloud_github::GithubAccountStore::global(cx)
            .map(|store| {
                let store = store.read(cx);
                let source = store.active_account().map(|active| {
                    if active.is_local() {
                        "stored on this PC"
                    } else {
                        "via OTHCloud"
                    }
                });
                (store.active_label(cx), store.problem().cloned(), source)
            })
            .unwrap_or_default();
        let subtitle: SharedString = match (&problem, source, &label) {
            (Some(problem), _, _) => problem.clone(),
            (None, Some(source), _) => source.into(),
            (None, None, Some(_)) => "connecting…".into(),
            (None, None, None) => "No account: git uses your credential manager".into(),
        };
        let title: SharedString = label.unwrap_or_else(|| "Not signed in".to_string()).into();
        v_flex()
            .w_full()
            .border_t_1()
            .border_color(colors.border)
            .child(ListHeader::new("GitHub"))
            .child(
                h_flex()
                    .px_3()
                    .pb_1()
                    .gap_2()
                    .child(Icon::new(IconName::Github).size(IconSize::Small).color(
                        if problem.is_some() {
                            Color::Warning
                        } else {
                            Color::Muted
                        },
                    ))
                    .child(
                        v_flex()
                            .min_w_0()
                            .child(Label::new(title).size(LabelSize::Small))
                            .child(
                                Label::new(subtitle)
                                    .size(LabelSize::XSmall)
                                    .color(if problem.is_some() {
                                        Color::Warning
                                    } else {
                                        Color::Muted
                                    })
                                    .truncate(),
                            ),
                    ),
            )
            .child(
                h_flex()
                    .px_2()
                    .pb_2()
                    .gap_1()
                    .child(
                        Button::new("othcloud-github-clone", "Clone Repository")
                            .style(ButtonStyle::Filled)
                            .label_size(LabelSize::Small)
                            .start_icon(Icon::new(IconName::Download).size(IconSize::XSmall))
                            .on_click(|_, window, cx| {
                                window.dispatch_action(
                                    othcloud_github::CloneRepository.boxed_clone(),
                                    cx,
                                )
                            }),
                    )
                    .child(
                        Button::new("othcloud-github-switch", "Switch Account")
                            .style(ButtonStyle::Subtle)
                            .label_size(LabelSize::Small)
                            .on_click(|_, window, cx| {
                                window.dispatch_action(
                                    othcloud_github::SwitchGithubAccount.boxed_clone(),
                                    cx,
                                )
                            }),
                    ),
            )
    }

    fn render_footer(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors();
        let link = |id: &'static str, label: &'static str, url: String| {
            Button::new(id, label)
                .label_size(LabelSize::XSmall)
                .color(Color::Muted)
                .on_click(move |_, _, cx| cx.open_url(&url))
        };
        h_flex()
            .w_full()
            .px_1()
            .py_1()
            .gap_1()
            .border_t_1()
            .border_color(colors.border)
            .child(link(
                "othcloud-footer-github",
                "GitHub",
                "https://github.com".to_string(),
            ))
            .child(link(
                "othcloud-footer-overtime",
                "Overtime Hosting",
                "https://overtime.hosting".to_string(),
            ))
            .child(link(
                "othcloud-footer-othcloud",
                "OTHCloud",
                othcloud_client::base_url(),
            ))
    }
}

impl Render for OthcloudPanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let panel_background = cx.theme().colors().panel_background;
        let account = Self::account(cx);
        let signed_in = account
            .as_ref()
            .is_some_and(|account| account.read(cx).is_signed_in());

        let body: AnyElement = if let Some(account) = account.filter(|_| signed_in) {
            let account = account.read(cx);
            let user = account.user().cloned();
            let services = account.services().cloned();
            let services_state = account.services_state().clone();

            let mut content = v_flex()
                .w_full()
                .child(self.render_header(user.as_ref(), cx));

            if let LoadState::Error {
                message,
                stale: true,
            } = &services_state
            {
                content = content.child(self.render_stale_banner(message));
            }

            match services {
                None => {
                    if let LoadState::Error {
                        message,
                        stale: false,
                    } = &services_state
                    {
                        content = content.child(
                            v_flex()
                                .px_3()
                                .py_2()
                                .gap_1()
                                .child(
                                    Label::new(message.clone())
                                        .size(LabelSize::Small)
                                        .color(Color::Error),
                                )
                                .child(
                                    Button::new("othcloud-retry-empty", "Retry")
                                        .label_size(LabelSize::Small)
                                        .on_click(|_, window, cx| {
                                            window
                                                .dispatch_action(RefreshServices.boxed_clone(), cx)
                                        }),
                                ),
                        );
                    } else {
                        content = content.child(self.render_skeleton(cx));
                    }
                }
                Some(services) => {
                    let applications = services.applications.as_deref();
                    let game_servers = services.game_servers.as_deref();
                    content = content
                        .child(self.render_section_header(
                            "Projects",
                            Some(services.projects.len()),
                            cx,
                        ))
                        .child(self.render_projects(&services.projects, cx))
                        .child(Divider::horizontal())
                        .child(self.render_section_header(
                            "Dev Environments",
                            applications.map(|applications| applications.len()),
                            cx,
                        ))
                        .child(self.render_dev_envs(applications, cx))
                        .child(Divider::horizontal())
                        .child(self.render_section_header(
                            "Game Servers",
                            Some(game_servers.map_or(0, |servers| servers.len())),
                            cx,
                        ))
                        .child(self.render_game_servers(game_servers, cx));
                }
            }
            content.into_any_element()
        } else {
            self.render_signed_out(cx).into_any_element()
        };

        v_flex()
            .id("othcloud-panel")
            .key_context("OthcloudPanel")
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(panel_background)
            .child(
                div()
                    .id("othcloud-panel-scroll")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .track_scroll(&self.scroll_handle)
                    .child(body),
            )
            .child(self.render_github(cx))
            .child(self.render_footer(cx))
    }
}

impl Focusable for OthcloudPanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<PanelEvent> for OthcloudPanel {}

impl Panel for OthcloudPanel {
    fn persistent_name() -> &'static str {
        "OTHCloud Panel"
    }

    fn panel_key() -> &'static str {
        OTHCLOUD_PANEL_KEY
    }

    fn position(&self, _: &Window, _: &App) -> DockPosition {
        self.position
    }

    fn position_is_valid(&self, position: DockPosition) -> bool {
        matches!(position, DockPosition::Left | DockPosition::Right)
    }

    fn set_position(&mut self, position: DockPosition, _: &mut Window, cx: &mut Context<Self>) {
        self.position = position;
        self.serialize(cx);
        // Docks re-read panel positions when settings change.
        cx.update_global::<SettingsStore, _>(|_, _| {});
        cx.notify();
    }

    fn default_size(&self, _: &Window, _: &App) -> Pixels {
        px(300.)
    }

    /// No dock button: the status bar's OTHCloud item already toggles the
    /// panel.
    fn icon(&self, _: &Window, _: &App) -> Option<IconName> {
        None
    }

    fn icon_tooltip(&self, _: &Window, _: &App) -> Option<&'static str> {
        Some("OTHCloud")
    }

    fn toggle_action(&self) -> Box<dyn Action> {
        Box::new(ToggleFocus)
    }

    fn starts_open(&self, _: &Window, _: &App) -> bool {
        false
    }

    fn set_active(&mut self, active: bool, _: &mut Window, cx: &mut Context<Self>) {
        self.active = active;
        if active {
            self.refresh_if_stale(cx);
            // If a reload just started, its ServicesChanged fetches statuses.
            let reloading = Self::account(cx).is_some_and(|account| {
                matches!(account.read(cx).services_state(), LoadState::Loading)
            });
            if self.statuses_dirty && !reloading {
                self.refresh_dev_env_statuses(cx);
            }
        }
    }

    fn activation_priority(&self) -> u32 {
        8
    }
}

// -------------------------------------------------------------------------
// Helpers over the othcloud_client types.
// -------------------------------------------------------------------------

pub(crate) fn user_display_name(user: &User) -> SharedString {
    user.display_name().to_string().into()
}

fn user_email(user: &User) -> Option<SharedString> {
    let email = user.email.trim();
    (!email.is_empty()).then(|| email.to_string().into())
}

fn user_avatar_url(user: &User) -> Option<SharedString> {
    let url = user.fixed_avatar_url()?;
    // gpui fetches image URIs over HTTP; inline `data:` avatars fall back to
    // the placeholder icon.
    if url.starts_with("data:") {
        return None;
    }
    Some(othcloud_client::absolute_url(&url).into())
}

/// The user's highest role, or `None` for plain members.
fn user_role_label(user: &User) -> Option<SharedString> {
    user.role_label().map(Into::into)
}

fn dev_env_state(status: &DevEnvStatus) -> (DevEnvState, Option<SharedString>) {
    match &status.state {
        DevEnvStatusState::Running { .. } => (DevEnvState::Running, None),
        DevEnvStatusState::Stopped { .. } => (DevEnvState::Stopped, None),
        DevEnvStatusState::Unavailable { reason } => (
            DevEnvState::Unavailable,
            Some(reason.clone())
                .filter(|reason| !reason.is_empty())
                .map(Into::into),
        ),
    }
}

fn state_from_status_str(status: Option<&str>) -> DevEnvState {
    match status.map(|status| status.to_ascii_lowercase()).as_deref() {
        Some("running" | "online" | "started" | "active") => DevEnvState::Running,
        Some("stopped" | "offline" | "exited" | "idle" | "sleeping") => DevEnvState::Stopped,
        Some("unavailable" | "error" | "failed") => DevEnvState::Unavailable,
        _ => DevEnvState::Unknown,
    }
}

fn row_url(row: &Row, fallback: &str) -> String {
    othcloud_client::absolute_url(row.url.as_deref().unwrap_or(fallback))
}

fn row_label(row: &Row, _cx: &App) -> impl IntoElement {
    Label::new(row.name.clone())
        .size(LabelSize::Small)
        .truncate()
}

fn meta_line(row: &Row, keys: &[&str]) -> Option<SharedString> {
    let parts: Vec<&str> = keys
        .iter()
        .filter_map(|key| row.meta.get(*key))
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
        .collect();
    (!parts.is_empty()).then(|| parts.join(" · ").into())
}

fn status_dot(status: Option<&str>) -> Option<AnyElement> {
    let status = status?.to_ascii_lowercase();
    let color = match status.as_str() {
        "running" | "online" | "started" | "active" | "done" | "healthy" => Color::Success,
        "error" | "failed" | "crashed" | "unhealthy" => Color::Error,
        "stopped" | "offline" | "exited" | "idle" | "sleeping" => Color::Muted,
        _ => Color::Warning,
    };
    Some(
        div()
            .id(SharedString::from(format!("othcloud-status-{status}")))
            .child(Indicator::dot().color(color))
            .tooltip(Tooltip::text(status))
            .into_any_element(),
    )
}

fn format_ago(elapsed: Duration) -> String {
    let secs = elapsed.as_secs();
    match secs {
        0..=9 => "just now".to_string(),
        10..=59 => format!("{secs}s ago"),
        60..=3599 => format!("{}m ago", secs / 60),
        _ => format!("{}h ago", secs / 3600),
    }
}
