use editor::Editor;
use gpui::{DismissEvent, Entity, EventEmitter, FocusHandle, Focusable};
use ui::prelude::*;
use workspace::ModalView;

use crate::handle_account_error;

/// A small modal that completes OTHCloud sign-in from a pasted
/// `othcloud-terminal://auth?code=...` link or a bare pairing code.
pub struct PairingCodeModal {
    editor: Entity<Editor>,
}

impl EventEmitter<DismissEvent> for PairingCodeModal {}
impl ModalView for PairingCodeModal {}

impl Focusable for PairingCodeModal {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.editor.focus_handle(cx)
    }
}

impl PairingCodeModal {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text(
                "Paste othcloud-terminal://auth?code=... or the code",
                window,
                cx,
            );
            editor
        });
        Self { editor }
    }

    fn cancel(&mut self, _: &menu::Cancel, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }

    fn confirm(&mut self, _: &menu::Confirm, _: &mut Window, cx: &mut Context<Self>) {
        let input = self.editor.read(cx).text(cx);
        let input = input.trim();
        if input.is_empty() {
            return;
        }
        if let Some(account) = crate::account(cx) {
            // The account shows a notification with the outcome (signed in, or
            // why pairing failed), so errors only need handling for 401/logs.
            let task = account.update(cx, |account, cx| {
                account.complete_pairing_from_link_or_code(input, cx)
            });
            cx.spawn(async move |_, cx| {
                if let Err(error) = task.await {
                    cx.update(|cx| handle_account_error(&error, cx));
                }
            })
            .detach();
        } else {
            log::error!(
                "othcloud: pairing code entered but the OTHCloud client is not initialized"
            );
        }
        cx.emit(DismissEvent);
    }
}

impl Render for PairingCodeModal {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .key_context("OthcloudPairingModal")
            .on_action(cx.listener(Self::cancel))
            .on_action(cx.listener(Self::confirm))
            .elevation_2(cx)
            .w(rems(34.))
            .child(
                v_flex()
                    .px_3()
                    .pt_2()
                    .pb_1()
                    .gap_0p5()
                    .child(
                        h_flex()
                            .gap_1p5()
                            .child(gpui::img("images/oth_logo.png").flex_none().size(px(18.)))
                            .child(Headline::new("Pair with OTHCloud").size(HeadlineSize::XSmall)),
                    )
                    .child(
                        Label::new(
                            "Paste the link or code shown on the OTHCloud pairing page, then press Enter.",
                        )
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                    ),
            )
            .child(
                div()
                    .mt_1()
                    .px_3()
                    .py_2()
                    .bg(cx.theme().colors().editor_background)
                    .border_t_1()
                    .border_color(cx.theme().colors().border_variant)
                    .child(self.editor.clone()),
            )
    }
}
