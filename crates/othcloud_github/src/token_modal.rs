//! "Sign in to GitHub": saves a GitHub token (OAuth, classic or fine-grained
//! PAT) as one of the user's GitHub accounts on OTHCloud and switches to it.

use std::sync::Arc;

use editor::Editor;
use gpui::{
    App, AppContext as _, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable,
    Render, SharedString, Task, WeakEntity, Window,
};
use othcloud_client::{ApiError, OthcloudApi};
use ui::{Button, ButtonStyle, Headline, HeadlineSize, prelude::*};
use workspace::{ModalView, Workspace};

use crate::{GithubAccountStore, show_status};

const CREATE_TOKEN_URL: &str =
    "https://github.com/settings/tokens/new?scopes=repo,workflow,read:org&description=OTerminal";

pub struct GithubTokenModal {
    workspace: WeakEntity<Workspace>,
    api: Arc<OthcloudApi>,
    editor: Entity<Editor>,
    error: Option<SharedString>,
    saving: bool,
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
        api: Arc<OthcloudApi>,
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
            api,
            editor,
            error: None,
            saving: false,
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
        let token = self.editor.read(cx).text(cx).trim().to_string();
        if token.is_empty() {
            self.error = Some("Paste a GitHub token first.".into());
            cx.notify();
            return;
        }
        self.saving = true;
        self.error = None;
        cx.notify();

        let api = self.api.clone();
        let host = api.host().to_string();
        let workspace = self.workspace.clone();
        let save = cx.background_spawn(async move {
            let account = api.add_github_account(&token).await?;
            let response = api.github_token(Some(&account.id)).await;
            Ok::<_, ApiError>((account, response))
        });
        self._save_task = Some(cx.spawn(async move |this, cx| {
            match save.await {
                Ok((account, token_response)) => {
                    let label = account.label.clone();
                    cx.update(|cx| {
                        let Some(store) = GithubAccountStore::global(cx) else {
                            return;
                        };
                        store.update(cx, |store, cx| match token_response {
                            Ok(response) if !response.token.is_empty() => {
                                store.use_account(account.id.clone(), response, cx)
                            }
                            // Saved, but no token yet: select it and let the
                            // store fetch one.
                            _ => store.select_account(account.id.clone(), cx),
                        });
                    });
                    workspace
                        .update(cx, |workspace, cx| {
                            show_status(
                                workspace,
                                format!("OTerminal now uses GitHub as {label}"),
                                false,
                                cx,
                            )
                        })
                        .ok();
                    this.update(cx, |_, cx| cx.emit(DismissEvent)).ok();
                }
                Err(error) => {
                    if error.is_unauthorized() {
                        cx.update(|cx| {
                            if let Some(account) = othcloud_client::OthcloudAccount::global(cx) {
                                account.update(cx, |account, cx| account.handle_unauthorized(cx));
                            }
                        });
                    }
                    let message = sign_in_error_message(&error, &host);
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

/// The message shown for a failed `POST /api/desktop/github-accounts`.
pub(crate) fn sign_in_error_message(error: &ApiError, host: &str) -> String {
    match (error.status, error.code.as_str()) {
        (_, "invalid_token") => "GitHub rejected that token.".to_string(),
        (_, "missing_repo_scope") => "The token needs the repo scope.".to_string(),
        (_, "linked_to_other_user") => {
            "That GitHub account is linked to another OTHCloud user.".to_string()
        }
        (404 | 405, _) => "This OTHCloud server can't save GitHub accounts yet.".to_string(),
        (_, "github_unavailable") => "GitHub didn't answer. Try again in a moment.".to_string(),
        _ => error.friendly_message(host),
    }
}

impl Render for GithubTokenModal {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors();
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
            .w(rems(30.))
            .p_4()
            .gap_3()
            .child(
                h_flex()
                    .gap_2()
                    .child(Icon::new(IconName::Github).size(IconSize::Small))
                    .child(Headline::new("Sign in to GitHub").size(HeadlineSize::Small)),
            )
            .child(
                Label::new(
                    "Paste a GitHub token. OTHCloud saves it as one of your GitHub accounts, \
                     and OTerminal uses it to clone and push.",
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
            .child(
                h_flex()
                    .justify_between()
                    .child(
                        Button::new("create-github-token", "Create a token on GitHub")
                            .style(ButtonStyle::Subtle)
                            .label_size(LabelSize::Small)
                            .end_icon(
                                Icon::new(IconName::ArrowUpRight)
                                    .size(IconSize::XSmall)
                                    .color(Color::Muted),
                            )
                            .on_click(|_, _, cx| cx.open_url(CREATE_TOKEN_URL)),
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
                                    if self.saving { "Saving…" } else { "Sign In" },
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
        ApiError {
            status,
            code: code.to_string(),
        }
    }

    #[test]
    fn maps_server_errors() {
        let host = "othcloud.xyz";
        assert_eq!(
            sign_in_error_message(&error(400, "invalid_token"), host),
            "GitHub rejected that token."
        );
        assert_eq!(
            sign_in_error_message(&error(400, "missing_repo_scope"), host),
            "The token needs the repo scope."
        );
        assert_eq!(
            sign_in_error_message(&error(409, "linked_to_other_user"), host),
            "That GitHub account is linked to another OTHCloud user."
        );
        assert_eq!(
            sign_in_error_message(&error(405, "method_not_allowed"), host),
            "This OTHCloud server can't save GitHub accounts yet."
        );
        assert_eq!(
            sign_in_error_message(&error(404, "HTTP 404"), host),
            "This OTHCloud server can't save GitHub accounts yet."
        );
    }
}
