//! Main window. For now: quick connect. Saved PCs come with `rdesk-store`.

use adw::prelude::*;
use gtk4 as gtk;
use gtk4::glib;
use libadwaita as adw;

use crate::dialogs;
use crate::session_window::SessionWindow;

pub struct ConnectionCenter;

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
        let form = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(6)
            .width_request(360)
            .halign(gtk::Align::Center)
            .build();
        form.append(&row);
        form.append(&error);

        let status = adw::StatusPage::builder()
            .icon_name("network-server-symbolic")
            .title("Connect to a PC")
            .description("Enter the name or address of a Windows PC with Remote Desktop turned on.")
            .child(&form)
            .build();

        let toolbar = adw::ToolbarView::new();
        toolbar.add_top_bar(&adw::HeaderBar::new());
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

        window
    }
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
        Err(e) => {
            let dialog = adw::AlertDialog::new(Some("Can't Connect"), Some(&format!("“{address}”: {e}")));
            dialog.add_response("close", "Close");
            dialog.present(Some(&window));
        }
    }
}

async fn quick_connect(window: adw::ApplicationWindow, host: String, port: u16, user: Option<String>) {
    let Some(credentials) = dialogs::ask_credentials(&window, &host, user.as_deref(), None).await else {
        return;
    };
    let Some(app) = window.application().and_downcast::<adw::Application>() else {
        return;
    };
    SessionWindow::open(&app, Some(&window), host, port, credentials);
}
