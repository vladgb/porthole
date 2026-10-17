//! Main window: quick connect and the saved PCs, as a grid of last-session thumbnails or a list,
//! grouped, searchable and sortable.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::{Rc, Weak};

use adw::prelude::*;
use gtk4 as gtk;
use gtk4::{gio, glib};
use libadwaita as adw;
use porthole_core::Credentials;
use porthole_store::{Layout, Pc, Settings, SortOrder, Uuid, keyring};

use crate::dialogs;
use crate::session_window::{SessionWindow, Target};

pub struct ConnectionCenter;

struct Inner {
    window: adw::ApplicationWindow,
    /// "empty" (no saved PCs) or "pcs".
    stack: gtk::Stack,
    search: gtk::SearchEntry,
    /// Rebuilt by [`refresh`]: one section per group.
    sections: gtk::Box,
}

type State = Rc<Inner>;

thread_local! {
    /// The main window, so other windows can ask it to refresh (e.g. after a new thumbnail).
    static CENTER: RefCell<Weak<Inner>> = RefCell::default();
}

impl ConnectionCenter {
    #[allow(clippy::new_ret_no_self)]
    pub fn new(app: &adw::Application) -> adw::ApplicationWindow {
        let entry = gtk::Entry::builder()
            .placeholder_text("PC name or IP address")
            .input_purpose(gtk::InputPurpose::Url)
            .activates_default(true)
            .hexpand(true)
            .build();
        let connect = gtk::Button::builder()
            .label("Connect")
            .css_classes(["suggested-action"])
            .sensitive(false)
            .build();
        let error = gtk::Label::builder()
            .css_classes(["error", "caption"])
            .xalign(0.0)
            .visible(false)
            .build();
        let row = gtk::Box::builder().spacing(6).build();
        row.append(&entry);
        row.append(&connect);
        let quick = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(6)
            .margin_top(18)
            .margin_bottom(6)
            .margin_start(12)
            .margin_end(12)
            .build();
        quick.append(&row);
        quick.append(&error);
        let quick = adw::Clamp::builder().maximum_size(560).child(&quick).build();

        let add_first = gtk::Button::builder()
            .label("Add PC")
            .halign(gtk::Align::Center)
            .css_classes(["pill", "suggested-action"])
            .action_name("win.add-pc")
            .build();
        let empty = adw::StatusPage::builder()
            .icon_name("network-server-symbolic")
            .title("No Saved PCs")
            .description("Connect to a PC above, or save one to connect with a click.")
            .child(&add_first)
            .vexpand(true)
            .build();

        let search = gtk::SearchEntry::builder().placeholder_text("Search saved PCs").build();
        let sections = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(24)
            .build();
        let pcs = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(18)
            .margin_top(12)
            .margin_bottom(24)
            .margin_start(12)
            .margin_end(12)
            .build();
        pcs.append(&adw::Clamp::builder().maximum_size(560).child(&search).build());
        pcs.append(&sections);
        let pcs = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .child(&adw::Clamp::builder().maximum_size(1200).child(&pcs).build())
            .build();

        let stack = gtk::Stack::new();
        stack.add_named(&empty, Some("empty"));
        stack.add_named(&pcs, Some("pcs"));
        let content = gtk::Box::builder().orientation(gtk::Orientation::Vertical).build();
        content.append(&quick);
        content.append(&stack);

        let header = adw::HeaderBar::new();
        header.pack_start(
            &gtk::Button::builder()
                .icon_name("list-add-symbolic")
                .tooltip_text("Add PC")
                .action_name("win.add-pc")
                .build(),
        );
        header.pack_end(&view_menu_button());

        let banner = adw::Banner::builder().use_markup(false).button_label("Dismiss").build();
        banner.connect_button_clicked(|banner| banner.set_revealed(false));
        if let Some(problem) = crate::config().problem.take() {
            banner.set_title(&problem);
            banner.set_revealed(true);
        }
        let toolbar = adw::ToolbarView::new();
        toolbar.add_top_bar(&header);
        toolbar.add_top_bar(&banner);
        toolbar.set_content(Some(&content));

        let window = adw::ApplicationWindow::builder()
            .application(app)
            .title("Porthole")
            .default_width(1000)
            .default_height(700)
            .content(&toolbar)
            .default_widget(&connect)
            .build();
        window.set_widget_name("connection-center");

        let state = Rc::new(Inner {
            window: window.clone(),
            stack,
            search: search.clone(),
            sections,
        });
        CENTER.with(|center| center.replace(Rc::downgrade(&state)));
        add_actions(&state);
        refresh(&state);

        entry.connect_changed(glib::clone!(
            #[weak]
            connect,
            #[weak]
            error,
            move |entry| {
                connect.set_sensitive(!entry.text().trim().is_empty());
                error.set_visible(false);
            }
        ));
        connect.connect_clicked(glib::clone!(
            #[weak]
            window,
            #[weak]
            entry,
            #[weak]
            error,
            move |_| match porthole_core::parse_address(&entry.text()) {
                Ok((host, port)) => {
                    glib::spawn_future_local(quick_connect(window, host, port, None));
                }
                Err(e) => {
                    error.set_label(&e.to_string());
                    error.set_visible(true);
                }
            }
        ));
        // Handlers hold the state; GTK drops them (and the cycle) when the window is destroyed.
        search.connect_search_changed(glib::clone!(
            #[strong]
            state,
            move |_| refresh(&state)
        ));

        window
    }
}

/// Window actions: add, and connect/edit/remove a PC by id; the view settings.
fn add_actions(state: &State) {
    let add = gio::SimpleAction::new("add-pc", None);
    add.connect_activate(glib::clone!(
        #[strong]
        state,
        move |_, _| {
            glib::spawn_future_local(edit(state.clone(), None));
        }
    ));
    state.window.add_action(&add);

    let pc_action = |name: &str, run: fn(State, Pc)| {
        let action = gio::SimpleAction::new(name, Some(glib::VariantTy::STRING));
        action.connect_activate(glib::clone!(
            #[strong]
            state,
            move |_, id| {
                let id = id.and_then(|id| id.str()).and_then(|id| Uuid::parse_str(id).ok());
                let pc = id.and_then(|id| crate::config().store.borrow().pc(id).cloned());
                if let Some(pc) = pc {
                    run(state.clone(), pc);
                }
            }
        ));
        state.window.add_action(&action);
    };
    pc_action("connect-pc", |state, pc| {
        glib::spawn_future_local(connect_saved(state.window.clone(), pc));
    });
    pc_action("edit-pc", |state, pc| {
        glib::spawn_future_local(edit(state, Some(pc)));
    });
    pc_action("remove-pc", |state, pc| {
        glib::spawn_future_local(remove(state, pc));
    });

    let settings = crate::config().store.borrow().settings().clone();
    let setting_action = |name: &str, current: &str, apply: fn(&mut Settings, &str)| {
        let action = gio::SimpleAction::new_stateful(name, Some(glib::VariantTy::STRING), &current.to_variant());
        action.connect_activate(glib::clone!(
            #[strong]
            state,
            move |action, value| {
                let Some(value) = value.and_then(|v| v.str()) else {
                    return;
                };
                action.set_state(&value.to_variant());
                update_settings(|settings| apply(settings, value));
                refresh(&state);
            }
        ));
        state.window.add_action(&action);
    };
    let layout = match settings.layout {
        Layout::Grid => "grid",
        Layout::List => "list",
    };
    setting_action("layout", layout, |settings, value| {
        settings.layout = if value == "list" { Layout::List } else { Layout::Grid };
    });
    let sort = match settings.sort {
        SortOrder::Name => "name",
        SortOrder::Recent => "recent",
    };
    setting_action("sort", sort, |settings, value| {
        settings.sort = if value == "recent" {
            SortOrder::Recent
        } else {
            SortOrder::Name
        };
    });
}

fn update_settings(change: impl FnOnce(&mut Settings)) {
    let config = crate::config();
    let mut settings = config.store.borrow().settings().clone();
    change(&mut settings);
    if let Err(e) = config.store.borrow_mut().save_settings(settings) {
        tracing::warn!(error = %e, "Couldn't save the view settings");
    }
}

fn view_menu_button() -> gtk::MenuButton {
    let view = gio::Menu::new();
    view.append(Some("Grid"), Some("win.layout::grid"));
    view.append(Some("List"), Some("win.layout::list"));
    let sort = gio::Menu::new();
    sort.append(Some("Sort by Name"), Some("win.sort::name"));
    sort.append(Some("Sort by Last Used"), Some("win.sort::recent"));
    let menu = gio::Menu::new();
    menu.append_section(None, &view);
    menu.append_section(None, &sort);
    gtk::MenuButton::builder()
        .icon_name("view-more-symbolic")
        .tooltip_text("View")
        .menu_model(&menu)
        .build()
}

/// Rebuilds the saved PCs, e.g. after a session saved a new thumbnail.
pub fn refresh_saved() {
    if let Some(state) = CENTER.with(|center| center.borrow().upgrade()) {
        refresh(&state);
    }
}

fn refresh(state: &State) {
    while let Some(child) = state.sections.first_child() {
        state.sections.remove(&child);
    }
    let config = crate::config();
    let store = config.store.borrow();
    if store.pcs().is_empty() {
        state.stack.set_visible_child_name("empty");
        return;
    }
    state.stack.set_visible_child_name("pcs");

    let query = state.search.text().trim().to_lowercase();
    let mut pcs: Vec<&Pc> = store.pcs().iter().filter(|pc| matches(pc, &query)).collect();
    if store.settings().sort == SortOrder::Recent {
        // Stable: PCs never connected keep their name order, after the others.
        pcs.sort_by_key(|pc| std::cmp::Reverse(pc.last_connected));
    }
    if pcs.is_empty() {
        state.sections.append(
            &adw::StatusPage::builder()
                .icon_name("edit-find-symbolic")
                .title("No Results")
                .description(glib::markup_escape_text(&format!(
                    "No saved PC matches “{}”.",
                    state.search.text().trim()
                )))
                .css_classes(["compact"])
                .build(),
        );
        return;
    }

    // Ungrouped PCs first, then groups by name.
    let mut groups: BTreeMap<(bool, String), (String, Vec<&Pc>)> = BTreeMap::new();
    for pc in pcs {
        let name = pc.group.clone().unwrap_or_default();
        groups
            .entry((!name.is_empty(), name.to_lowercase()))
            .or_insert_with(|| (name, Vec::new()))
            .1
            .push(pc);
    }
    let titled = groups.keys().any(|(grouped, _)| *grouped);
    for (name, pcs) in groups.into_values() {
        let section = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(12)
            .build();
        if titled {
            let title = if name.is_empty() { "Other PCs" } else { name.as_str() };
            section.append(
                &gtk::Label::builder()
                    .label(title)
                    .xalign(0.0)
                    .css_classes(["heading"])
                    .build(),
            );
        }
        match store.settings().layout {
            Layout::Grid => section.append(&grid(&store, &pcs)),
            Layout::List => section.append(&list(&pcs)),
        }
        state.sections.append(&section);
    }
}

fn matches(pc: &Pc, query: &str) -> bool {
    query.is_empty()
        || [Some(&pc.name), Some(&pc.host), pc.username.as_ref(), pc.group.as_ref()]
            .into_iter()
            .flatten()
            .any(|field| field.to_lowercase().contains(query))
}

fn subtitle(pc: &Pc) -> String {
    match &pc.username {
        Some(user) => format!("{} · {user}", pc.address()),
        None => pc.address(),
    }
}

fn pc_menu(pc: &Pc) -> gtk::MenuButton {
    let menu = gio::Menu::new();
    menu.append(Some("Connect"), Some(&format!("win.connect-pc::{}", pc.id)));
    menu.append(Some("Edit…"), Some(&format!("win.edit-pc::{}", pc.id)));
    menu.append(Some("Remove…"), Some(&format!("win.remove-pc::{}", pc.id)));
    gtk::MenuButton::builder()
        .icon_name("view-more-symbolic")
        .tooltip_text("More")
        .valign(gtk::Align::Center)
        .css_classes(["flat"])
        .menu_model(&menu)
        .build()
}

fn list(pcs: &[&Pc]) -> gtk::ListBox {
    let list = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .css_classes(["boxed-list"])
        .build();
    for pc in pcs {
        let row = adw::ActionRow::builder()
            .title(glib::markup_escape_text(&pc.display_name()))
            .subtitle(glib::markup_escape_text(&subtitle(pc)))
            .activatable(true)
            .action_name("win.connect-pc")
            .action_target(&pc.id.to_string().to_variant())
            .build();
        row.add_prefix(&gtk::Image::from_icon_name("computer-symbolic"));
        row.add_suffix(&pc_menu(pc));
        list.append(&row);
    }
    list
}

const THUMBNAIL_WIDTH: i32 = 240;
const THUMBNAIL_HEIGHT: i32 = 150;

fn grid(store: &porthole_store::Store, pcs: &[&Pc]) -> gtk::FlowBox {
    let grid = gtk::FlowBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .homogeneous(true)
        .min_children_per_line(1)
        .max_children_per_line(8)
        .column_spacing(12)
        .row_spacing(12)
        .build();
    for pc in pcs {
        let thumbnail: gtk::Widget = match store.thumbnail_path(pc.id).filter(|path| path.exists()) {
            Some(path) => {
                let picture = gtk::Picture::builder()
                    .file(&gio::File::for_path(path))
                    .content_fit(gtk::ContentFit::Cover)
                    .build();
                // A picture wants its image's size; cap it to the tile in both directions.
                let clamp = |orientation, size, child: &gtk::Widget| {
                    adw::Clamp::builder()
                        .orientation(orientation)
                        .maximum_size(size)
                        .tightening_threshold(size)
                        .child(child)
                        .build()
                        .upcast::<gtk::Widget>()
                };
                let width = clamp(gtk::Orientation::Horizontal, THUMBNAIL_WIDTH, picture.upcast_ref());
                clamp(gtk::Orientation::Vertical, THUMBNAIL_HEIGHT, &width)
            }
            None => gtk::Image::builder()
                .icon_name("computer-symbolic")
                .pixel_size(64)
                .css_classes(["dim-label"])
                .build()
                .upcast(),
        };
        let frame = gtk::Frame::builder()
            .child(&thumbnail)
            .width_request(THUMBNAIL_WIDTH)
            .height_request(THUMBNAIL_HEIGHT)
            .overflow(gtk::Overflow::Hidden)
            .build();
        let name = gtk::Label::builder()
            .label(pc.display_name())
            .xalign(0.0)
            .ellipsize(gtk4::pango::EllipsizeMode::End)
            .css_classes(["heading"])
            .build();
        let detail = gtk::Label::builder()
            .label(subtitle(pc))
            .xalign(0.0)
            .ellipsize(gtk4::pango::EllipsizeMode::End)
            .css_classes(["caption", "dim-label"])
            .build();
        let labels = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .hexpand(true)
            .build();
        labels.append(&name);
        labels.append(&detail);
        let open = gtk::Button::builder()
            .child(&{
                let tile = gtk::Box::builder()
                    .orientation(gtk::Orientation::Vertical)
                    .spacing(8)
                    .build();
                tile.append(&frame);
                tile.append(&labels);
                tile
            })
            .tooltip_text(format!("Connect to {}", pc.display_name()))
            .action_name("win.connect-pc")
            .action_target(&pc.id.to_string().to_variant())
            .css_classes(["flat"])
            .build();
        // Overlaid rather than inside the button, so it gets its own clicks.
        let menu = pc_menu(pc);
        menu.set_halign(gtk::Align::End);
        menu.set_valign(gtk::Align::End);
        menu.set_margin_bottom(6);
        let overlay = gtk::Overlay::builder().child(&open).build();
        overlay.add_overlay(&menu);
        grid.append(&overlay);
    }
    grid
}

async fn edit(state: State, pc: Option<Pc>) {
    let groups = crate::config().store.borrow().groups();
    let Some(pc) = dialogs::edit_pc(&state.window, pc.as_ref(), &groups).await else {
        return;
    };
    let saved = crate::config().store.borrow_mut().save_pc(pc);
    if let Err(e) = saved {
        dialogs::show_error(&state.window, "Couldn't Save the PC", &e.to_string());
    }
    refresh(&state);
}

async fn remove(state: State, pc: Pc) {
    if !dialogs::confirm_remove(&state.window, &pc.display_name()).await {
        return;
    }
    let removed = crate::config().store.borrow_mut().remove_pc(pc.id);
    let id = pc.id;
    if let Some(Err(e)) = crate::on_runtime(keyring::forget_password(id)).await {
        tracing::warn!(error = %e, "Couldn't remove the saved password");
    }
    if let Err(e) = removed {
        dialogs::show_error(&state.window, "Couldn't Remove the PC", &e.to_string());
    }
    refresh(&state);
}

/// Handles a (re)launch: show the main window and, if an address was given, start connecting.
pub fn launch(app: &adw::Application, launch: crate::Launch) {
    let window = app
        .windows()
        .into_iter()
        .find_map(|w| {
            w.downcast::<adw::ApplicationWindow>()
                .ok()
                .filter(|w| w.widget_name() == "connection-center")
        })
        .unwrap_or_else(|| ConnectionCenter::new(app));
    window.present();
    let Some(address) = launch.address else { return };
    match porthole_core::parse_address(&address) {
        Ok((host, port)) => {
            glib::spawn_future_local(quick_connect(window, host, port, launch.user));
        }
        Err(e) => dialogs::show_error(&window, "Can't Connect", &format!("“{address}”: {e}")),
    }
}

async fn quick_connect(window: adw::ApplicationWindow, host: String, port: u16, user: Option<String>) {
    let mut target = Target {
        name: Pc::new(host.clone(), port).address(),
        host,
        port,
        pc: None,
        nla: true,
        clipboard: true,
        audio: true,
        microphone: false,
        camera: false,
        all_monitors: false,
        best_quality: false,
        shared_folders: Vec::new(),
        save_password: false,
        password_saved: false,
    };
    let Some(sign_in) = dialogs::ask_credentials(&window, &target.name, user.as_deref(), None, None).await else {
        return;
    };
    target.save_password = sign_in.remember;
    open_session(&window, target, sign_in.credentials);
}

async fn connect_saved(window: adw::ApplicationWindow, pc: Pc) {
    let mut target = Target {
        name: pc.display_name(),
        host: pc.host.clone(),
        port: pc.port,
        pc: Some(pc.id),
        nla: pc.nla,
        clipboard: pc.clipboard,
        audio: pc.audio,
        microphone: pc.microphone,
        camera: pc.camera,
        all_monitors: pc.all_monitors,
        best_quality: pc.best_quality,
        shared_folders: pc.shared_folders.clone(),
        save_password: false,
        password_saved: false,
    };
    // A saved password connects straight away.
    if let Some(username) = pc.username.clone() {
        let id = pc.id;
        let lookup = crate::on_runtime(async move {
            let password = keyring::password(id, &username).await;
            (username, password)
        });
        match lookup.await {
            Some((username, Ok(Some(password)))) => {
                target.password_saved = true;
                open_session(&window, target, Credentials::parse(&username, password.as_str()));
                return;
            }
            Some((_, Err(e))) => tracing::warn!(error = %e, "Couldn't read the saved password"),
            _ => {}
        }
    }
    let Some(sign_in) =
        dialogs::ask_credentials(&window, &target.name, pc.username.as_deref(), None, Some(false)).await
    else {
        return;
    };
    target.save_password = sign_in.remember;
    open_session(&window, target, sign_in.credentials);
}

fn open_session(window: &adw::ApplicationWindow, target: Target, credentials: Credentials) {
    let Some(app) = window.application().and_downcast::<adw::Application>() else {
        return;
    };
    SessionWindow::open(&app, Some(window), target, credentials);
}
