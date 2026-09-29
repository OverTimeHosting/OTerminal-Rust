//! "Sign in with GitHub" in the browser, using GitHub's OAuth device flow.
//! Only offered when a GitHub OAuth App client id is configured
//! (`othcloud.github_oauth_client_id`). The resulting token is stored on this
//! PC like a pasted one.

use std::time::{Duration, Instant};

use gpui::{
    App, ClipboardItem, Context, DismissEvent, EventEmitter, FocusHandle, Focusable, Render,
    SharedString, Task, WeakEntity, Window,
};
use ui::{Button, ButtonStyle, Checkbox, Headline, HeadlineSize, ToggleState, prelude::*};
use workspace::{ModalView, Workspace};

use crate::{
    AddGithubToken,
    github_api::{self, DeviceCode, DevicePoll},
    othcloud_signed_in, show_status,
    token_modal::save_local_account,
};

enum State {
    Requesting,
    Waiting(DeviceCode),
    Finishing,
    Failed(SharedString),
}

pub struct GithubDeviceFlowModal {
    workspace: WeakEntity<Workspace>,
    client_id: String,
    focus_handle: FocusHandle,
    state: State,
    save_to_othcloud: bool,
    copied: bool,
    _task: Option<Task<()>>,
}

impl EventEmitter<DismissEvent> for GithubDeviceFlowModal {}
impl ModalView for GithubDeviceFlowModal {}

impl Focusable for GithubDeviceFlowModal {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl GithubDeviceFlowModal {
    pub fn new(
        workspace: WeakEntity<Workspace>,
        client_id: String,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut this = Self {
            workspace,
            client_id,
            focus_handle: cx.focus_handle(),
            state: State::Requesting,
            save_to_othcloud: false,
            copied: false,
            _task: None,
        };
        this.start(cx);
        this
    }

    fn fail(&mut self, message: impl Into<SharedString>, cx: &mut Context<Self>) {
        self.state = State::Failed(message.into());
        cx.notify();
    }

    fn start(&mut self, cx: &mut Context<Self>) {
        self.state = State::Requesting;
        self.copied = false;
        cx.notify();
        let http = cx.http_client();
        let client_id = self.client_id.clone();
        let workspace = self.workspace.clone();
        self._task = Some(cx.spawn(async move |this, cx| {
            let code = cx
                .background_spawn({
                    let http = http.clone();
                    let client_id = client_id.clone();
                    async move { github_api::request_device_code(http, &client_id).await }
                })
                .await;
            let code = match code {
                Ok(code) => code,
                Err(error) => {
                    let message = match &error {
                        github_api::GithubError::InvalidResponse(message) => message.clone(),
                        error => error.message(),
                    };
                    this.update(cx, |this, cx| this.fail(message, cx)).ok();
                    return;
                }
            };

            let opened = this
                .update(cx, |this, cx| {
                    cx.write_to_clipboard(ClipboardItem::new_string(code.user_code.clone()));
                    this.copied = true;
                    cx.open_url(&code.verification_uri);
                    this.state = State::Waiting(code.clone());
                    cx.notify();
                })
                .is_ok();
            if !opened {
                return;
            }

            let deadline = Instant::now() + Duration::from_secs(code.expires_in.max(60));
            let mut interval = code.interval.max(1);
            let token = loop {
                cx.background_executor()
                    .timer(Duration::from_secs(interval))
                    .await;
                if Instant::now() > deadline {
                    this.update(cx, |this, cx| {
                        this.fail("The code expired. Start the sign-in again.", cx)
                    })
                    .ok();
                    return;
                }
                let poll = cx
                    .background_spawn({
                        let http = http.clone();
                        let client_id = client_id.clone();
                        let device_code = code.device_code.clone();
                        async move {
                            github_api::poll_device_token(http, &client_id, &device_code, interval)
                                .await
                        }
                    })
                    .await;
                match poll {
                    Ok(DevicePoll::Pending) => {}
                    Ok(DevicePoll::SlowDown(new_interval)) => interval = new_interval.max(1),
                    Ok(DevicePoll::Token(token)) => break token,
                    Ok(DevicePoll::Failed(message)) => {
                        this.update(cx, |this, cx| this.fail(message, cx)).ok();
                        return;
                    }
                    // Transient network trouble: keep polling until the deadline.
                    Err(github_api::GithubError::Network(error)) => {
                        log::warn!("polling GitHub's device flow failed: {error}");
                    }
                    Err(error) => {
                        this.update(cx, |this, cx| this.fail(error.message(), cx))
                            .ok();
                        return;
                    }
                }
            };

            let save_to_othcloud = this
                .update(cx, |this, cx| {
                    this.state = State::Finishing;
                    cx.notify();
                    this.save_to_othcloud
                })
                .unwrap_or(false);
            let save_to_othcloud = save_to_othcloud && cx.update(|cx| othcloud_signed_in(cx));

            let validation = cx
                .background_spawn({
                    let token = token.clone();
                    async move { github_api::validate_token(http, &token).await }
                })
                .await;
            let result = match validation {
                Ok(validated) => {
                    save_local_account(token, validated, "device", save_to_othcloud, cx).await
                }
                Err(error) => Err(error.message()),
            };
            match result {
                Ok(message) => {
                    workspace
                        .update(cx, |workspace, cx| {
                            show_status(workspace, message, false, cx)
                        })
                        .ok();
                    this.update(cx, |_, cx| cx.emit(DismissEvent)).ok();
                }
                Err(message) => {
                    this.update(cx, |this, cx| this.fail(message, cx)).ok();
                }
            }
        }));
    }

    fn cancel(&mut self, _: &menu::Cancel, _window: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }
}

impl Render for GithubDeviceFlowModal {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors();
        let signed_in = othcloud_signed_in(cx);

        let body: AnyElement = match &self.state {
            State::Requesting => Label::new("Contacting GitHub…")
                .size(LabelSize::Small)
                .color(Color::Muted)
                .into_any_element(),
            State::Finishing => Label::new("Signed in. Saving the account…")
                .size(LabelSize::Small)
                .color(Color::Muted)
                .into_any_element(),
            State::Failed(message) => v_flex()
                .gap_2()
                .child(
                    Label::new(message.clone())
                        .size(LabelSize::Small)
                        .color(Color::Error),
                )
                .child(
                    h_flex()
                        .gap_1()
                        .child(
                            Button::new("github-device-retry", "Try Again")
                                .style(ButtonStyle::Filled)
                                .on_click(cx.listener(|this, _, _, cx| this.start(cx))),
                        )
                        .child(
                            Button::new("github-device-use-token", "Use a Token Instead")
                                .style(ButtonStyle::Subtle)
                                .on_click(cx.listener(|this, _, window, cx| {
                                    let workspace = this.workspace.clone();
                                    cx.emit(DismissEvent);
                                    window.defer(cx, move |window, cx| {
                                        if let Some(workspace) = workspace.upgrade() {
                                            window.focus(&workspace.focus_handle(cx), cx);
                                        }
                                        window.dispatch_action(Box::new(AddGithubToken), cx);
                                    });
                                })),
                        ),
                )
                .into_any_element(),
            State::Waiting(code) => {
                let user_code = code.user_code.clone();
                let verification_uri = code.verification_uri.clone();
                v_flex()
                    .gap_2()
                    .child(
                        Label::new(format!(
                            "Enter this code on {} to let OTerminal use your GitHub account{}:",
                            verification_uri
                                .trim_start_matches("https://")
                                .trim_end_matches('/'),
                            if self.copied {
                                " (it's on your clipboard)"
                            } else {
                                ""
                            }
                        ))
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                    )
                    .child(
                        h_flex()
                            .justify_center()
                            .py_2()
                            .rounded_sm()
                            .border_1()
                            .border_color(colors.border)
                            .bg(colors.editor_background)
                            .child(
                                Label::new(user_code.clone())
                                    .size(LabelSize::Large)
                                    .buffer_font(cx),
                            ),
                    )
                    .child(
                        h_flex()
                            .gap_1()
                            .child(
                                Button::new("github-device-copy", "Copy Code")
                                    .style(ButtonStyle::Subtle)
                                    .on_click(move |_, _, cx| {
                                        cx.write_to_clipboard(ClipboardItem::new_string(
                                            user_code.clone(),
                                        ))
                                    }),
                            )
                            .child(
                                Button::new("github-device-open", "Open GitHub")
                                    .style(ButtonStyle::Subtle)
                                    .end_icon(
                                        Icon::new(IconName::ArrowUpRight)
                                            .size(IconSize::XSmall)
                                            .color(Color::Muted),
                                    )
                                    .on_click(move |_, _, cx| cx.open_url(&verification_uri)),
                            ),
                    )
                    .child(
                        Label::new("Waiting for you to authorize OTerminal in the browser…")
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .into_any_element()
            }
        };

        v_flex()
            .key_context("GithubDeviceFlowModal")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::cancel))
            .elevation_3(cx)
            .w(rems(30.))
            .p_4()
            .gap_3()
            .child(
                h_flex()
                    .gap_2()
                    .child(Icon::new(IconName::Github).size(IconSize::Small))
                    .child(Headline::new("Sign in with GitHub").size(HeadlineSize::Small)),
            )
            .child(body)
            .when(
                signed_in && !matches!(self.state, State::Failed(_)),
                |this| {
                    this.child(
                        Checkbox::new(
                            "github-device-save-to-othcloud",
                            ToggleState::from(self.save_to_othcloud),
                        )
                        .label("Also save to OTHCloud (so OTHCloud can deploy from it)")
                        .on_click(cx.listener(
                            |this, state: &ToggleState, _, cx| {
                                this.save_to_othcloud = state.selected();
                                cx.notify();
                            },
                        )),
                    )
                },
            )
            .child(
                Label::new(
                    "The token is stored in the system credential store on this PC \
                     (Windows Credential Manager).",
                )
                .size(LabelSize::XSmall)
                .color(Color::Muted),
            )
            .child(
                h_flex().justify_end().child(
                    Button::new("github-device-cancel", "Cancel")
                        .style(ButtonStyle::Subtle)
                        .on_click(cx.listener(|_, _, _, cx| cx.emit(DismissEvent))),
                ),
            )
    }
}
