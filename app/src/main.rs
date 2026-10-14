use adw::prelude::*;
use libadwaita as adw;

const APP_ID: &str = "io.github.rdesk.Rdesk";

fn main() -> gtk4::glib::ExitCode {
    let app = adw::Application::builder().application_id(APP_ID).build();
    app.connect_activate(build_window);
    app.run()
}

fn build_window(app: &adw::Application) {
    let status = adw::StatusPage::builder()
        .icon_name("network-server-symbolic")
        .title("No PCs yet")
        .description("Add a PC to get started.")
        .build();

    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&adw::HeaderBar::new());
    toolbar.set_content(Some(&status));

    adw::ApplicationWindow::builder()
        .application(app)
        .title("Remote Desktop")
        .default_width(960)
        .default_height(640)
        .content(&toolbar)
        .build()
        .present();
}
