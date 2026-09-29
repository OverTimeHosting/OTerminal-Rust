//! "Add a GitHub account with a token": validates a personal access token
//! with GitHub, stores it in the system credential store on this PC, and
//! makes it the active account. Optionally also saves it on OTHCloud.

use editor::Editor;
use gpui::{
    App, AppContext as _, AsyncApp, Context, DismissEvent, Entity, EventEmitter, FocusHandle,
    Focusable, Render, SharedString, Task, WeakEntity, Window,
};
use othcloud_client::ApiError;
use ui::{Button, ButtonStyle, Checkbox, Headline, HeadlineSize, ToggleState, prelude::*};
use workspace::{ModalView, Workspace};

use crate::{
    GithubAccountStore, LocalGithubAccount, SignInToGithub, github_api, oauth_client_id,
    othcloud_api, othcloud_signed_in, show_status,
};

pub struct GithubTokenModal {
    workspace: WeakEntity<Workspace>,
    editor: Entity<Editor>,
    error: Option<SharedString>,
    saving: bool,
    save_to_othcloud: bool,
    _save_task: Option<Task<()>>,
}

impl EventEmitter<DismissEvent> for GithubTokenModal {}
impl ModalView for GithubTokenModal {}

impl Focusable for GithubTokenModal {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.editor.focus_handle(cx)
    }
}

impl GithubTokenModal {
    pub fn new(
        workspace: WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_masked(true, cx);
            editor.set_placeholder_text("ghp_… or github_pat_…", window, cx);
            editor
        });
        Self {
            workspace,
            editor,
            error: None,
            saving: false,
            save_to_othcloud: false,
            _save_task: None,
        }
    }

    fn cancel(&mut self, _: &menu::Cancel, _window: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }

    fn confirm(&mut self, _: &menu::Confirm, window: &mut Window, cx: &mut Context<Self>) {
        self.save(window, cx);
    }

    fn save(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        if self.saving {
            return;
        }
        let token = github_api::normalize_token(&self.editor.read(cx).text(cx));
        if token.is_empty() {
            self.error = Some("Paste a GitHub token first.".into());
            cx.notify();
            return;
        }
        self.saving = true;
        self.error = None;
        cx.notify();

        let http = cx.http_client();
        let save_to_othcloud = self.save_to_othcloud && othcloud_signed_in(cx);
        let workspace = self.workspace.clone();
        self._save_task = Some(cx.spawn(async move |this, cx| {
            let validation = cx
                .background_spawn({
                    let token = token.clone();
                    async move { github_api::validate_token(http, &token).await }
                })
                .await;
            let result = match validation {
                Ok(validated) => {
                    save_local_account(token, validated, "token", save_to_othcloud, cx).await
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
                    this.update(cx, |this, cx| {
                        this.saving = false;
                        this.error = Some(message.into());
                        cx.notify();
                    })
                    .ok();
                }
            }
        }));
    }
}

/// Stores a validated token as a local account (and, if asked, on OTHCloud)
/// and makes it active. Returns the message to show on success.
pub(crate) async fn save_local_account(
    token: String,
    validated: github_api::ValidatedToken,
    source: &str,
    save_to_othcloud: bool,
    cx: &mut AsyncApp,
) -> Result<String, String> {
    let mut account = LocalGithubAccount::from_validated(&validated, source);
    let login = account.login.clone();

    let mut othcloud_warning = None;
    if save_to_othcloud && let Some(api) = cx.update(|cx| othcloud_api(cx)) {
        let host = api.host().to_string();
        let token_for_othcloud = token.clone();
        let saved = cx
            .background_spawn(
                async move { api.add_github_account(&token_for_othcloud, true).await },
            )
            .await;
        match saved {
            Ok(_) => {
                account.saved_to_othcloud = true;
                cx.update(|cx| {
                    if let Some(store) = GithubAccountStore::global(cx) {
                        store.update(cx, |store, cx| store.refresh_othcloud_accounts(cx));
                    }
                });
            }
            Err(error) => {
                if error.is_unauthorized() {
                    cx.update(crate::handle_othcloud_unauthorized);
                }
                othcloud_warning = Some(othcloud_save_error_message(&error, &host));
            }
        }
    }

    let Some(store) = cx.update(|cx| GithubAccountStore::global(cx)) else {
        return Err("GitHub accounts aren't available.".to_string());
    };
    let task =
        cx.update(|cx| store.update(cx, |store, cx| store.add_local_account(account, token, cx)));
    task.await.map_err(|error| format!("{error:#}"))?;

    Ok(match othcloud_warning {
        None if save_to_othcloud => {
            format!("OTerminal now uses GitHub as {login} (also saved to OTHCloud)")
        }
        None => format!("OTerminal now uses GitHub as {login}"),
        Some(warning) => format!(
            "OTerminal now uses GitHub as {login}, but saving it to OTHCloud failed: {warning}"
        ),
    })
}

/// The message shown for a failed `POST /api/desktop/github-accounts`.
pub(crate) fn othcloud_save_error_message(error: &ApiError, host: &str) -> String {
    match (error.status, error.code.as_str()) {
        (_, "invalid_token") => "GitHub rejected that token.".to_string(),
        (_, "missing_repo_scope") => "The token needs the repo scope.".to_string(),
        (_, "linked_to_other_user") => {
            "That GitHub account is linked to another OTHCloud user.".to_string()
        }
        (404 | 405, _) => "This OTHCloud server can't save GitHub accounts yet.".to_string(),
        (_, "github_unavailable") => "GitHub didn't answer. Try again in a moment.".to_string(),
        (401, _) => "Your OTHCloud session expired. Sign in to OTHCloud again.".to_string(),
        _ => error.friendly_message(host),
    }
}

impl Render for GithubTokenModal {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors();
        let signed_in = othcloud_signed_in(cx);
        let browser_sign_in = oauth_client_id(cx).is_some();
        // `AskPass > Editor` binds enter to `menu::Confirm`.
        v_flex()
            .key_context("AskPass")
            .on_action(cx.listener(Self::cancel))
            .on_action(cx.listener(Self::confirm))
            // macOS has no `AskPass > Editor` binding, so plain Enter is also
            // handled here; `save` ignores a second call while saving.
            .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, window, cx| {
                if event.keystroke.key == "enter" && !event.keystroke.modifiers.modified() {
                    cx.stop_propagation();
                    this.save(window, cx);
                }
            }))
            .elevation_3(cx)
            .w(rems(32.))
            .p_4()
            .gap_3()
            .child(
                h_flex()
                    .gap_2()
                    .child(Icon::new(IconName::Github).size(IconSize::Small))
                    .child(Headline::new("Add a GitHub account").size(HeadlineSize::Small)),
            )
            .child(
                Label::new(
                    "Paste a GitHub personal access token — classic with the repo, workflow \
                     and read:org scopes, or fine-grained. OTerminal checks it with GitHub and \
                     keeps it in the system credential store on this PC (Windows Credential \
                     Manager), never in settings files.",
                )
                .size(LabelSize::Small)
                .color(Color::Muted),
            )
            .child(
                div()
                    .px_2()
                    .py_1()
                    .rounded_sm()
                    .border_1()
                    .border_color(if self.error.is_some() {
                        cx.theme().status().error_border
                    } else {
                        colors.border
                    })
                    .bg(colors.editor_background)
                    .child(self.editor.clone()),
            )
            .when_some(self.error.clone(), |this, error| {
                this.child(Label::new(error).size(LabelSize::Small).color(Color::Error))
            })
            .when(signed_in, |this| {
                this.child(
                    Checkbox::new(
                        "github-token-save-to-othcloud",
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
            })
            .child(
                h_flex()
                    .justify_between()
                    .child(
                        h_flex()
                            .gap_1()
                            .child(
                                Button::new("create-github-token", "Create a token")
                                    .style(ButtonStyle::Subtle)
                                    .label_size(LabelSize::Small)
                                    .end_icon(
                                        Icon::new(IconName::ArrowUpRight)
                                            .size(IconSize::XSmall)
                                            .color(Color::Muted),
                                    )
                                    .on_click(|_, _, cx| cx.open_url(github_api::CREATE_TOKEN_URL)),
                            )
                            .when(browser_sign_in, |this| {
                                this.child(
                                    Button::new("github-browser-sign-in", "Use the browser")
                                        .style(ButtonStyle::Subtle)
                                        .label_size(LabelSize::Small)
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            let workspace = this.workspace.clone();
                                            cx.emit(DismissEvent);
                                            window.defer(cx, move |window, cx| {
                                                if let Some(workspace) = workspace.upgrade() {
                                                    window.focus(&workspace.focus_handle(cx), cx);
                                                }
                                                window
                                                    .dispatch_action(Box::new(SignInToGithub), cx);
                                            });
                                        })),
                                )
                            }),
                    )
                    .child(
                        h_flex()
                            .gap_1()
                            .child(
                                Button::new("cancel-github-token", "Cancel")
                                    .style(ButtonStyle::Subtle)
                                    .on_click(cx.listener(|_, _, _, cx| cx.emit(DismissEvent))),
                            )
                            .child(
                                Button::new(
                                    "save-github-token",
                                    if self.saving {
                                        "Checking…"
                                    } else {
                                        "Add Account"
                                    },
                                )
                                .style(ButtonStyle::Filled)
                                .disabled(self.saving)
                                .on_click(cx.listener(|this, _, window, cx| this.save(window, cx))),
                            ),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn error(status: u16, code: &str) -> ApiError {
        ApiError::new(status, code)
    }

    #[test]
    fn maps_server_errors() {
        let host = "othcloud.xyz";
        assert_eq!(
            othcloud_save_error_message(&error(400, "invalid_token"), host),
            "GitHub rejected that token."
        );
        assert_eq!(
            othcloud_save_error_message(&error(400, "missing_repo_scope"), host),
            "The token needs the repo scope."
        );
        assert_eq!(
            othcloud_save_error_message(&error(409, "linked_to_other_user"), host),
            "That GitHub account is linked to another OTHCloud user."
        );
        assert_eq!(
            othcloud_save_error_message(&error(405, "method_not_allowed"), host),
            "This OTHCloud server can't save GitHub accounts yet."
        );
        assert_eq!(
            othcloud_save_error_message(&error(404, "HTTP 404"), host),
            "This OTHCloud server can't save GitHub accounts yet."
        );
        assert_eq!(
            othcloud_save_error_message(&ApiError::network(), host),
            "Can't reach othcloud.xyz"
        );
    }
}
