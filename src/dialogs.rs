//! Two-answer questions, as the window asks them: a cancel and an accept response, Escape
//! answering cancel.

use adw::prelude::*;

/// A question with a `cancel` and an `accept` response, each `(id, label)`. `look` styles the
/// accept button; `default_accept` makes Enter accept rather than cancel. An empty `body` sets
/// none.
pub fn confirm_dialog(
    heading: &str,
    body: &str,
    cancel: (&str, &str),
    accept: (&str, &str),
    look: adw::ResponseAppearance,
    default_accept: bool,
) -> adw::AlertDialog {
    let dialog = adw::AlertDialog::new(Some(heading), (!body.is_empty()).then_some(body));
    dialog.add_responses(&[cancel, accept]);
    dialog.set_response_appearance(accept.0, look);
    dialog.set_default_response(Some(if default_accept { accept.0 } else { cancel.0 }));
    dialog.set_close_response(cancel.0);
    dialog
}

/// Shows `dialog` over `parent` and waits for its answer: whether it was `accept_id`.
pub async fn confirmed(
    dialog: &adw::AlertDialog,
    parent: &impl IsA<gtk4::Widget>,
    accept_id: &str,
) -> bool {
    dialog.clone().choose_future(Some(parent)).await == accept_id
}
