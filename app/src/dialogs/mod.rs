//! Modal questions. Each returns `None`/`false` when cancelled.

use adw::prelude::*;
use gtk4 as gtk;
use gtk4::glib;
use libadwaita as adw;
use rdesk_core::{CertificateInfo, CertificateProblem, Credentials};

/// Asks for a user name and password. `error` explains why we're asking again.
pub async fn ask_credentials(
    parent: &impl IsA<gtk::Widget>,
    host: &str,
    username: Option<&str>,
    error: Option<&str>,
) -> Option<Credentials> {
    let user_row = adw::EntryRow::builder()
        .title("User name")
        .activates_default(true)
        .build();
    let password_row = adw::PasswordEntryRow::builder()
        .title("Password")
        .activates_default(true)
        .build();
    if let Some(username) = username {
        user_row.set_text(username);
    }
    let list = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .css_classes(["boxed-list"])
        .build();
    list.append(&user_row);
    list.append(&password_row);

    let body = match error {
        Some(error) => format!("{error}\n\nEnter your credentials for {host}."),
        None => format!("Enter your credentials for {host}."),
    };
    let dialog = adw::AlertDialog::builder()
        .heading("Sign In")
        .body(body)
        .extra_child(&list)
        .default_response("connect")
        .close_response("cancel")
        .build();
    dialog.add_responses(&[("cancel", "Cancel"), ("connect", "Connect")]);
    dialog.set_response_appearance("connect", adw::ResponseAppearance::Suggested);
    dialog.set_response_enabled("connect", username.is_some_and(|u| !u.is_empty()));
    user_row.connect_changed(glib::clone!(
        #[weak]
        dialog,
        move |row| dialog.set_response_enabled("connect", !row.text().trim().is_empty())
    ));
    if username.is_some() {
        dialog.set_focus(Some(&password_row));
    } else {
        dialog.set_focus(Some(&user_row));
    }

    let response = dialog.choose_future(Some(parent)).await;
    (response == "connect").then(|| Credentials::parse(user_row.text().trim(), password_row.text().as_str()))
}

/// Shows why the certificate couldn't be verified and asks whether to connect anyway.
pub async fn ask_certificate_trust(
    parent: &impl IsA<gtk::Widget>,
    certificate: &CertificateInfo,
    problem: &CertificateProblem,
) -> bool {
    let details = gtk::Grid::builder()
        .column_spacing(12)
        .row_spacing(6)
        .width_request(440)
        .build();
    let rows = [
        ("Issued to", certificate.subject.as_str()),
        ("Issued by", certificate.issuer.as_str()),
        ("Valid from", certificate.not_before.as_str()),
        ("Valid until", certificate.not_after.as_str()),
        ("SHA-256", certificate.sha256_fingerprint.as_str()),
    ];
    for (i, (label, value)) in rows.into_iter().enumerate() {
        let name = gtk::Label::builder()
            .label(label)
            .xalign(0.0)
            .yalign(0.0)
            .css_classes(["dim-label"])
            .build();
        let value = gtk::Label::builder()
            .label(value)
            .xalign(0.0)
            .wrap(true)
            .wrap_mode(gtk4::pango::WrapMode::WordChar)
            .selectable(true)
            // Selectable labels grab focus and pre-select their text when the dialog opens.
            .focusable(false)
            .hexpand(true)
            .build();
        if label == "SHA-256" {
            value.add_css_class("monospace");
            value.add_css_class("caption");
        }
        details.attach(&name, 0, i as i32, 1, 1);
        details.attach(&value, 1, i as i32, 1, 1);
    }

    let dialog = adw::AlertDialog::builder()
        .heading("Can't Verify the Remote PC")
        .body(format!(
            "{problem}.\n\nOnly connect if you trust {}. Otherwise someone could be intercepting the connection.",
            certificate.host
        ))
        .extra_child(&details)
        .default_response("cancel")
        .close_response("cancel")
        .build();
    dialog.add_responses(&[("cancel", "Cancel"), ("connect", "Connect Anyway")]);
    dialog.set_response_appearance("connect", adw::ResponseAppearance::Destructive);
    dialog.choose_future(Some(parent)).await == "connect"
}
