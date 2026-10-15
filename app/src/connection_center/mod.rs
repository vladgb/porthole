//! Main window: quick connect and the list of saved PCs.

use std::rc::Rc;

use adw::prelude::*;
use gtk4 as gtk;
use gtk4::glib;
use libadwaita as adw;
use rdesk_store::Pc;

use crate::dialogs;
use crate::session_window::{SessionWindow, Target};

pub struct ConnectionCenter;

struct Inner {
    window: adw::ApplicationWindow,
    list: gtk::ListBox,
    empty: gtk::Label,
}

type State = Rc<Inner>;

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
            .build();
        quick.append(&row);
        quick.append(&error);

        let add = gtk::Button::builder()
            .icon_name("list-add-symbolic")
            .tooltip_text("Add PC")
            .css_classes(["flat"])
            .build();
        let list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .css_classes(["boxed-list"])
            .build();
        let empty = gtk::Label::builder()
            .label("PCs you add with + show up here.")
            .css_classes(["dim-label"])
            .xalign(0.0)
            .build();
        let saved = adw::PreferencesGroup::builder()
            .title("Saved PCs")
            .header_suffix(&add)
            .build();
        saved.add(&list);
        saved.add(&empty);

        let content = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(24)
            .build();
        content.append(&quick);
        content.append(&saved);
        let clamp = adw::Clamp::builder().maximum_size(560).child(&content).build();

        let status = adw::StatusPage::builder()
            .icon_name("network-server-symbolic")
            .title("Connect to a PC")
            .description("Enter the name or address of a Windows PC with Remote Desktop turned on.")
            .child(&clamp)
            .build();

        let banner = adw::Banner::builder().use_markup(false).button_label("Dismiss").build();
        banner.connect_button_clicked(|banner| banner.set_revealed(false));
        if let Some(problem) = crate::config().problem.take() {
            banner.set_title(&problem);
            banner.set_revealed(true);
        }
        let toolbar = adw::ToolbarView::new();
        toolbar.add_top_bar(&adw::HeaderBar::new());
        toolbar.add_top_bar(&banner);
        toolbar.set_content(Some(&status));

        let window = adw::ApplicationWindow::builder()
            .application(app)
            .title("Remote Desktop")
            .default_width(960)
            .default_height(640)
            .content(&toolbar)
            .default_widget(&connect)
            .build();
        window.set_widget_name("connection-center");

        let state = Rc::new(Inner {
            window: window.clone(),
            list,
            empty,
        });
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
            move |_| match rdesk_core::parse_address(&entry.text()) {
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
        add.connect_clicked(move |_| {
            glib::spawn_future_local(edit(state.clone(), None));
        });

        window
    }
}

/// Rebuilds the saved PC list from the store.
fn refresh(state: &State) {
    state.list.remove_all();
    let config = crate::config();
    let store = config.store.borrow();
    for pc in store.pcs() {
        state.list.append(&pc_row(state, pc));
    }
    state.list.set_visible(!store.pcs().is_empty());
    state.empty.set_visible(store.pcs().is_empty());
}

fn pc_row(state: &State, pc: &Pc) -> adw::ActionRow {
    let subtitle = match &pc.username {
        Some(user) => format!("{} · {user}", pc.address()),
        None => pc.address(),
    };
    let row = adw::ActionRow::builder()
        .title(glib::markup_escape_text(&pc.display_name()))
        .subtitle(glib::markup_escape_text(&subtitle))
        .activatable(true)
        .build();
    row.add_prefix(&gtk::Image::from_icon_name("computer-symbolic"));
    let edit_button = gtk::Button::builder()
        .icon_name("document-edit-symbolic")
        .tooltip_text("Edit")
        .valign(gtk::Align::Center)
        .css_classes(["flat"])
        .build();
    let remove_button = gtk::Button::builder()
        .icon_name("user-trash-symbolic")
        .tooltip_text("Remove")
        .valign(gtk::Align::Center)
        .css_classes(["flat"])
        .build();
    row.add_suffix(&edit_button);
    row.add_suffix(&remove_button);

    let id = pc.id;
    let saved = |id| crate::config().store.borrow().pc(id).cloned();
    row.connect_activated(glib::clone!(
        #[strong]
        state,
        move |_| {
            if let Some(pc) = saved(id) {
                glib::spawn_future_local(connect_saved(state.window.clone(), pc));
            }
        }
    ));
    edit_button.connect_clicked(glib::clone!(
        #[strong]
        state,
        move |_| {
            if let Some(pc) = saved(id) {
                glib::spawn_future_local(edit(state.clone(), Some(pc)));
            }
        }
    ));
    remove_button.connect_clicked(glib::clone!(
        #[strong]
        state,
        move |_| {
            glib::spawn_future_local(remove(state.clone(), id));
        }
    ));
    row
}

async fn edit(state: State, pc: Option<Pc>) {
    let Some(pc) = dialogs::edit_pc(&state.window, pc.as_ref()).await else {
        return;
    };
    let saved = crate::config().store.borrow_mut().save_pc(pc);
    if let Err(e) = saved {
        dialogs::show_error(&state.window, "Couldn't Save the PC", &e.to_string());
    }
    refresh(&state);
}

async fn remove(state: State, id: rdesk_store::Uuid) {
    let Some(name) = crate::config().store.borrow().pc(id).map(Pc::display_name) else {
        return;
    };
    if !dialogs::confirm_remove(&state.window, &name).await {
        return;
    }
    let removed = crate::config().store.borrow_mut().remove_pc(id);
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
    match rdesk_core::parse_address(&address) {
        Ok((host, port)) => {
            glib::spawn_future_local(quick_connect(window, host, port, launch.user));
        }
        Err(e) => dialogs::show_error(&window, "Can't Connect", &format!("“{address}”: {e}")),
    }
}

async fn quick_connect(window: adw::ApplicationWindow, host: String, port: u16, user: Option<String>) {
    let target = Target {
        name: Pc::new(host.clone(), port).address(),
        host,
        port,
        pc: None,
        nla: true,
        clipboard: true,
    };
    open_session(window, target, user).await;
}

async fn connect_saved(window: adw::ApplicationWindow, pc: Pc) {
    let target = Target {
        name: pc.display_name(),
        host: pc.host,
        port: pc.port,
        pc: Some(pc.id),
        nla: pc.nla,
        clipboard: pc.clipboard,
    };
    open_session(window, target, pc.username).await;
}

async fn open_session(window: adw::ApplicationWindow, target: Target, user: Option<String>) {
    let Some(credentials) = dialogs::ask_credentials(&window, &target.name, user.as_deref(), None).await else {
        return;
    };
    let Some(app) = window.application().and_downcast::<adw::Application>() else {
        return;
    };
    SessionWindow::open(&app, Some(&window), target, credentials);
}
