//! Modal questions. Each returns `None`/`false` when cancelled.

use adw::prelude::*;
use gtk4 as gtk;
use gtk4::glib;
use libadwaita as adw;
use rdesk_core::{CertificateInfo, CertificateProblem, Credentials};
use rdesk_store::Pc;

/// What the sign-in dialog returns.
pub struct SignIn {
    pub credentials: Credentials,
    /// "Remember password" was ticked (always false when it wasn't offered).
    pub remember: bool,
}

/// Asks for a user name and password. `error` explains why we're asking again. `remember`
/// offers saving the password, ticked or not; `None` doesn't offer it (e.g. quick connect).
pub async fn ask_credentials(
    parent: &impl IsA<gtk::Widget>,
    host: &str,
    username: Option<&str>,
    error: Option<&str>,
    remember: Option<bool>,
) -> Option<SignIn> {
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
    let remember_row = remember.map(|active| {
        let row = adw::SwitchRow::builder()
            .title("Remember Password")
            .active(active)
            .build();
        list.append(&row);
        row
    });

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
    (response == "connect").then(|| SignIn {
        credentials: Credentials::parse(user_row.text().trim(), password_row.text().as_str()),
        remember: remember_row.is_some_and(|row| row.is_active()),
    })
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
/// `groups`: existing group names, offered as suggestions.
pub async fn edit_pc(parent: &impl IsA<gtk::Widget>, pc: Option<&Pc>, groups: &[String]) -> Option<Pc> {
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
    let group_row = adw::EntryRow::builder()
        .title("Group (optional)")
        .activates_default(true)
        .build();
    if !groups.is_empty() {
        // Pick an existing group from a menu, or type a new one.
        let menu = gtk4::gio::Menu::new();
        for group in groups {
            let item = gtk4::gio::MenuItem::new(Some(group), None);
            item.set_action_and_target_value(Some("pc.group"), Some(&group.to_variant()));
            menu.append_item(&item);
        }
        let pick = gtk::MenuButton::builder()
            .icon_name("pan-down-symbolic")
            .tooltip_text("Existing Groups")
            .valign(gtk::Align::Center)
            .css_classes(["flat"])
            .menu_model(&menu)
            .build();
        group_row.add_suffix(&pick);
        let actions = gtk4::gio::SimpleActionGroup::new();
        let set_group = gtk4::gio::SimpleAction::new("group", Some(glib::VariantTy::STRING));
        set_group.connect_activate(glib::clone!(
            #[weak]
            group_row,
            move |_, value| {
                if let Some(value) = value.and_then(|v| v.str()) {
                    group_row.set_text(value);
                }
            }
        ));
        actions.add_action(&set_group);
        group_row.insert_action_group("pc", Some(&actions));
    }
    let audio_row = adw::SwitchRow::builder()
        .title("Play Audio")
        .subtitle("Play the PC's sound on this computer")
        .active(pc.is_none_or(|pc| pc.audio))
        .build();
    let monitors_row = adw::SwitchRow::builder()
        .title("Use All Monitors")
        .subtitle("In full screen, spread the PC's desktop over every monitor")
        .active(pc.is_some_and(|pc| pc.all_monitors))
        .build();
    let quality_row = adw::ComboRow::builder()
        .title("Picture Quality")
        .subtitle("Best: sharper text and exact colours, but needs a fast network")
        .model(&gtk::StringList::new(&["Balanced", "Best"]))
        .selected(u32::from(pc.is_some_and(|pc| pc.best_quality)))
        .build();
    let microphone_row = adw::SwitchRow::builder()
        .title("Use Microphone")
        .subtitle("Let the PC record from this computer's microphone")
        .active(pc.is_some_and(|pc| pc.microphone))
        .build();
    let camera_row = adw::SwitchRow::builder()
        .title("Use Camera")
        .subtitle("Let the PC use this computer's cameras")
        .active(pc.is_some_and(|pc| pc.camera))
        .build();
    let nla_row = adw::SwitchRow::builder()
        .title("Network Level Authentication")
        .subtitle("Turn off only for PCs that don't support it")
        .active(pc.is_none_or(|pc| pc.nla))
        .build();
    let clipboard_row = adw::SwitchRow::builder()
        .title("Share Clipboard")
        .subtitle("Copy and paste text between this computer and the PC")
        .active(pc.is_none_or(|pc| pc.clipboard))
        .build();
    if let Some(pc) = pc {
        address_row.set_text(&pc.address());
        name_row.set_text(&pc.name);
        user_row.set_text(pc.username.as_deref().unwrap_or_default());
        group_row.set_text(pc.group.as_deref().unwrap_or_default());
    }
    let list = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .css_classes(["boxed-list"])
        .width_request(360)
        .build();
    list.append(&address_row);
    list.append(&name_row);
    list.append(&user_row);
    list.append(&group_row);
    list.append(&clipboard_row);
    list.append(&audio_row);
    list.append(&microphone_row);
    list.append(&camera_row);
    list.append(&monitors_row);
    list.append(&quality_row);
    let folders = SharedFolders::new(pc.map_or(&[][..], |pc| &pc.shared_folders));
    list.append(&folders.row);
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
    saved.clipboard = clipboard_row.is_active();
    saved.audio = audio_row.is_active();
    saved.microphone = microphone_row.is_active();
    saved.camera = camera_row.is_active();
    saved.all_monitors = monitors_row.is_active();
    saved.best_quality = quality_row.selected() == 1;
    saved.shared_folders = folders.paths.borrow().clone();
    saved.group = Some(group_row.text().trim().to_owned()).filter(|g| !g.is_empty());
    saved.username = Some(user_row.text().trim().to_owned()).filter(|u| !u.is_empty());
    Some(saved)
}

/// The "Shared Folders" row: an expandable list with remove buttons and "Add Folder…".
struct SharedFolders {
    row: adw::ExpanderRow,
    paths: std::rc::Rc<std::cell::RefCell<Vec<std::path::PathBuf>>>,
}

impl SharedFolders {
    fn new(initial: &[std::path::PathBuf]) -> Self {
        let row = adw::ExpanderRow::builder()
            .title("Shared Folders")
            .subtitle("Folders the PC can open as drives")
            .build();
        let paths = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let add = adw::ActionRow::builder().title("Add Folder…").activatable(true).build();
        add.add_prefix(&gtk::Image::from_icon_name("list-add-symbolic"));
        row.add_row(&add);
        let folders = Self { row, paths };
        for path in initial {
            folders.add(path.clone(), &add);
        }
        add.connect_activated(glib::clone!(
            #[strong(rename_to = paths)]
            folders.paths,
            #[weak(rename_to = row)]
            folders.row,
            move |add| {
                let dialog = gtk::FileDialog::builder().title("Share a Folder").modal(true).build();
                let window = add.root().and_downcast::<gtk::Window>();
                let (paths, row, add) = (paths.clone(), row.clone(), add.clone());
                glib::spawn_future_local(async move {
                    let Ok(folder) = dialog.select_folder_future(window.as_ref()).await else {
                        return;
                    };
                    if let Some(path) = folder.path() {
                        SharedFolders { row, paths }.add(path, &add);
                    }
                });
            }
        ));
        folders
    }

    fn add(&self, path: std::path::PathBuf, add_row: &adw::ActionRow) {
        if self.paths.borrow().contains(&path) {
            return;
        }
        let name = path
            .file_name()
            .map_or_else(|| path.display().to_string(), |n| n.to_string_lossy().into_owned());
        let entry = adw::ActionRow::builder()
            .title(glib::markup_escape_text(&name))
            .subtitle(glib::markup_escape_text(&path.display().to_string()))
            .build();
        let remove = gtk::Button::builder()
            .icon_name("list-remove-symbolic")
            .tooltip_text("Stop Sharing")
            .valign(gtk::Align::Center)
            .css_classes(["flat"])
            .build();
        entry.add_suffix(&remove);
        remove.connect_clicked(glib::clone!(
            #[strong(rename_to = paths)]
            self.paths,
            #[weak(rename_to = row)]
            self.row,
            #[weak]
            entry,
            #[strong]
            path,
            move |_| {
                paths.borrow_mut().retain(|p| *p != path);
                row.remove(&entry);
            }
        ));
        self.paths.borrow_mut().push(path);
        // Keep "Add Folder…" last.
        self.row.remove(add_row);
        self.row.add_row(&entry);
        self.row.add_row(add_row);
    }
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
