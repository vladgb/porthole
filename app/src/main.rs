mod connection_center;
mod dialogs;
mod session_window;

use std::sync::OnceLock;

use adw::prelude::*;
use gtk4::glib;
use libadwaita as adw;

const APP_ID: &str = "io.github.rdesk.Rdesk";

/// One multi-threaded runtime for all sessions; GTK stays on the main thread.
fn runtime() -> &'static tokio::runtime::Runtime {
    static RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("rdesk-session")
            .enable_all()
            .build()
            .expect("failed to start the tokio runtime")
    })
}

fn main() -> glib::ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("RDESK_LOG").unwrap_or_else(|_| "info,sspi=warn".into()),
        )
        .compact()
        .init();

    let app = adw::Application::builder()
        .application_id(APP_ID)
        .flags(gtk4::gio::ApplicationFlags::HANDLES_COMMAND_LINE)
        .build();
    app.connect_command_line(|app, command_line| {
        let args: Vec<String> = command_line
            .arguments()
            .iter()
            .skip(1)
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        match parse_args(&args) {
            Ok(launch) => {
                connection_center::launch(app, launch);
                glib::ExitCode::SUCCESS
            }
            Err(message) => {
                eprint!("{message}\nUsage: rdesk [HOST[:PORT]] [-u|--user USER]\n");
                glib::ExitCode::FAILURE
            }
        }
    });
    app.run()
}

/// What to do when the app is started (or started again while running).
#[derive(Debug, Default, PartialEq)]
pub struct Launch {
    pub address: Option<String>,
    pub user: Option<String>,
}

fn parse_args(args: &[String]) -> Result<Launch, String> {
    let mut launch = Launch::default();
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-u" | "--user" => launch.user = Some(args.next().ok_or("--user needs a value")?.clone()),
            other if other.starts_with('-') => return Err(format!("Unknown option {other}")),
            address if launch.address.is_none() => launch.address = Some(address.to_owned()),
            extra => return Err(format!("Unexpected argument {extra}")),
        }
    }
    Ok(launch)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn parses_launch_arguments() {
        assert_eq!(parse_args(&[]), Ok(Launch::default()));
        assert_eq!(
            parse_args(&args(&["pc:3390", "-u", r"D\a"])),
            Ok(Launch {
                address: Some("pc:3390".into()),
                user: Some(r"D\a".into())
            })
        );
        assert!(parse_args(&args(&["--bogus"])).is_err());
        assert!(parse_args(&args(&["a", "b"])).is_err());
        assert!(parse_args(&args(&["-u"])).is_err());
    }
}
