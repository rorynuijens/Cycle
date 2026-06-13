//! Small shared dialog helpers.

use adw::prelude::*;

/// Present a standard destructive-confirmation dialog (Cancel + a destructive
/// confirm button). `on_confirm` runs on the GTK main thread only if the user
/// chooses the confirm action.
///
/// Centralises the AlertDialog boilerplate (responses, destructive styling,
/// close-on-cancel) that the delete buttons across the app would otherwise
/// each repeat.
pub fn confirm_destructive(
    parent: &impl IsA<gtk::Widget>,
    heading: &str,
    body: &str,
    confirm_label: &str,
    on_confirm: impl Fn() + 'static,
) {
    let dialog = adw::AlertDialog::builder()
        .heading(heading)
        .body(body)
        .build();
    dialog.add_response("cancel", "_Cancel");
    dialog.add_response("confirm", confirm_label);
    dialog.set_response_appearance("confirm", adw::ResponseAppearance::Destructive);
    // Cancel is the default/close response so Enter or Esc never triggers the
    // destructive action by accident.
    dialog.set_default_response(Some("cancel"));
    dialog.set_close_response("cancel");
    dialog.connect_response(None, move |_, resp| {
        if resp == "confirm" {
            on_confirm();
        }
    });
    dialog.present(Some(parent));
}
