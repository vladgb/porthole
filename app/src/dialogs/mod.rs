//! Modal questions. Each returns `None`/`false` when cancelled.

use adw::prelude::*;
use gtk4 as gtk;
use gtk4::glib;
use libadwaita as adw;
use rdesk_core::{CertificateInfo, CertificateProblem, Credentials};
use rdesk_store::Pc;

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

/// What the user decided about a certificate that couldn't be verified.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CertificateDecision {
    Reject,
    AcceptOnce,
    AcceptAlways,
}

/// Shows why the certificate couldn't be verified and asks whether to connect anyway.
/// `changed`: a different certificate was trusted for this PC before.
pub async fn ask_certificate_trust(
    parent: &impl IsA<gtk::Widget>,
    certificate: &CertificateInfo,
    problem: &CertificateProblem,
    changed: bool,
) -> CertificateDecision {
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
    let always = gtk::CheckButton::builder()
        .label("Always trust this certificate for this PC")
        .margin_top(6)
        .build();
    details.attach(&always, 0, rows.len() as i32, 2, 1);

    let (heading, body) = if changed {
        (
            "The Remote PC's Certificate Changed",
            format!(
                "{} is presenting a different certificate from the one you chose to trust. {problem}.\n\n\
                 This is expected if the PC was reinstalled or its certificate renewed. Otherwise someone \
                 could be intercepting the connection.",
                certificate.host
            ),
        )
    } else {
        (
            "Can't Verify the Remote PC",
            format!(
                "{problem}.\n\nOnly connect if you trust {}. Otherwise someone could be intercepting the connection.",
                certificate.host
            ),
        )
    };
    let dialog = adw::AlertDialog::builder()
        .heading(heading)
        .body(body)
        .extra_child(&details)
        .default_response("cancel")
        .close_response("cancel")
        .build();
    dialog.add_responses(&[("cancel", "Cancel"), ("connect", "Connect Anyway")]);
    dialog.set_response_appearance("connect", adw::ResponseAppearance::Destructive);
    match dialog.choose_future(Some(parent)).await.as_str() {
        "connect" if always.is_active() => CertificateDecision::AcceptAlways,
        "connect" => CertificateDecision::AcceptOnce,
        _ => CertificateDecision::Reject,
    }
}

/// Add (`pc` is `None`) or edit a saved PC. Returns the PC to save.
pub async fn edit_pc(parent: &impl IsA<gtk::Widget>, pc: Option<&Pc>) -> Option<Pc> {
    let address_row = adw::EntryRow::builder()
        .title("PC name or IP address")
        .input_purpose(gtk::InputPurpose::Url)
        .activates_default(true)
        .build();
    let name_row = adw::EntryRow::builder()
        .title("Display name (optional)")
        .activates_default(true)
        .build();
    let user_row = adw::EntryRow::builder()
        .title("User name (optional)")
        .activates_default(true)
        .build();
    let nla_row = adw::SwitchRow::builder()
        .title("Network Level Authentication")
        .subtitle("Turn off only for PCs that don't support it")
        .active(pc.is_none_or(|pc| pc.nla))
        .build();
    if let Some(pc) = pc {
        address_row.set_text(&pc.address());
        name_row.set_text(&pc.name);
        user_row.set_text(pc.username.as_deref().unwrap_or_default());
    }
    let list = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .css_classes(["boxed-list"])
        .width_request(360)
        .build();
    list.append(&address_row);
    list.append(&name_row);
    list.append(&user_row);
    list.append(&nla_row);
    let error = gtk::Label::builder()
        .css_classes(["error", "caption"])
        .xalign(0.0)
        .visible(false)
        .build();
    let content = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(6)
        .build();
    content.append(&list);
    content.append(&error);

    let dialog = adw::AlertDialog::builder()
        .heading(if pc.is_some() { "Edit PC" } else { "Add PC" })
        .extra_child(&content)
        .default_response("save")
        .close_response("cancel")
        .build();
    dialog.add_responses(&[
        ("cancel", "Cancel"),
        ("save", if pc.is_some() { "Save" } else { "Add" }),
    ]);
    dialog.set_response_appearance("save", adw::ResponseAppearance::Suggested);
    let validate = glib::clone!(
        #[weak]
        dialog,
        #[weak]
        error,
        move |row: &adw::EntryRow| {
            let text = row.text();
            let result = rdesk_core::parse_address(&text);
            dialog.set_response_enabled("save", result.is_ok());
            match result {
                Err(e) if !text.trim().is_empty() => {
                    error.set_label(&e.to_string());
                    error.set_visible(true);
                }
                _ => error.set_visible(false),
            }
        }
    );
    validate(&address_row);
    address_row.connect_changed(validate);
    dialog.set_focus(Some(&address_row));

    if dialog.choose_future(Some(parent)).await != "save" {
        return None;
    }
    let (host, port) = rdesk_core::parse_address(&address_row.text()).ok()?;
    let mut saved = pc.cloned().unwrap_or_else(|| Pc::new(host.clone(), port));
    saved.host = host;
    saved.port = port;
    saved.name = name_row.text().trim().to_owned();
    saved.nla = nla_row.is_active();
    saved.username = Some(user_row.text().trim().to_owned()).filter(|u| !u.is_empty());
    Some(saved)
}

/// Asks before removing a saved PC.
pub async fn confirm_remove(parent: &impl IsA<gtk::Widget>, name: &str) -> bool {
    let dialog = adw::AlertDialog::builder()
        .heading("Remove PC?")
        .body(format!("“{name}” will be removed from your saved PCs."))
        .default_response("cancel")
        .close_response("cancel")
        .build();
    dialog.add_responses(&[("cancel", "Cancel"), ("remove", "Remove")]);
    dialog.set_response_appearance("remove", adw::ResponseAppearance::Destructive);
    dialog.choose_future(Some(parent)).await == "remove"
}

/// Reports something that went wrong; there's nothing to decide.
pub fn show_error(parent: &impl IsA<gtk::Widget>, heading: &str, body: &str) {
    let dialog = adw::AlertDialog::new(Some(heading), Some(body));
    dialog.add_response("close", "Close");
    dialog.present(Some(parent));
}
