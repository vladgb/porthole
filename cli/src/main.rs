//! `rdesk-cli`: drive `rdesk-core` without a UI (debugging, integration tests).
//!
//! ```shell
//! RDESK_PASSWORD=... rdesk-cli screenshot pc.example -u 'CONTOSO\alice' -o desktop.png
//! ```

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context as _, bail};
use clap::{Parser, Subcommand};
use rdesk_core::{Credentials, MonitorRect, Security, SessionConfig, SessionEvent};
use tokio::time::Instant;
use tracing::{info, warn};

#[derive(Parser)]
#[command(about = "Headless rdesk client for debugging and tests")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Connect, wait for the screen to settle, save it as PNG, and disconnect.
    Screenshot(ScreenshotArgs),
}

#[derive(Parser)]
struct ScreenshotArgs {
    /// Host name or IP address, optionally with `:port`.
    host: String,
    /// User name, optionally as DOMAIN\user. The password comes from RDESK_PASSWORD or a prompt.
    #[arg(short, long)]
    username: String,
    #[arg(short, long, default_value = "screenshot.png")]
    output: PathBuf,
    #[arg(long, default_value_t = 1920)]
    width: u16,
    #[arg(long, default_value_t = 1080)]
    height: u16,
    /// Use TLS security without NLA.
    #[arg(long)]
    no_nla: bool,
    /// Accept the server certificate if its SHA-256 fingerprint matches (as printed on rejection).
    #[arg(long, value_name = "SHA256")]
    trust: Option<String>,
    /// Accept any server certificate. For test servers only.
    #[arg(long, conflicts_with = "trust")]
    insecure: bool,
    /// Seconds without screen updates before taking the screenshot.
    #[arg(long, default_value_t = 3)]
    settle: u64,
    /// Overall time limit in seconds.
    #[arg(long, default_value_t = 60)]
    timeout: u64,
    /// Monitors to ask for once connected, e.g. `1280x800,1024x768+1280+0` (the first is the
    /// primary, at 0,0). The screenshot then shows the whole multi-monitor desktop.
    #[arg(long, value_parser = parse_monitors)]
    monitors: Option<Monitors>,
    /// Don't use the Graphics Pipeline (H.264): legacy bitmap updates only.
    #[arg(long)]
    no_gfx: bool,
}

#[derive(Debug, Clone)]
struct Monitors(Vec<MonitorRect>);

fn parse_monitors(text: &str) -> Result<Monitors, String> {
    text.split(',')
        .map(|monitor| {
            let (size, position) = monitor.split_once('+').map_or((monitor, None), |(s, p)| (s, Some(p)));
            let (width, height) = size.split_once('x').ok_or(format!("{monitor}: expected WxH"))?;
            let (left, top) = match position {
                Some(position) => position.split_once('+').ok_or(format!("{monitor}: expected +X+Y"))?,
                None => ("0", "0"),
            };
            let number = |s: &str| s.parse::<i64>().map_err(|e| format!("{monitor}: {e}"));
            Ok(MonitorRect {
                left: number(left)? as i32,
                top: number(top)? as i32,
                width: number(width)? as u32,
                height: number(height)? as u32,
            })
        })
        .collect::<Result<_, _>>()
        .map(Monitors)
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("RDESK_LOG").unwrap_or_else(|_| "info,sspi=warn".into()),
        )
        .compact()
        .with_writer(std::io::stderr)
        .init();

    let Command::Screenshot(args) = Cli::parse().command;
    let runtime = tokio::runtime::Runtime::new()?;
    let code = runtime.block_on(screenshot(args, runtime.handle().clone()))?;
    std::process::exit(code);
}

async fn screenshot(args: ScreenshotArgs, runtime: tokio::runtime::Handle) -> anyhow::Result<i32> {
    // Never taken from argv, where other local users could read it via /proc.
    let password = match std::env::var("RDESK_PASSWORD") {
        Ok(p) => p,
        Err(_) => rpassword::prompt_password(format!("Password for {}: ", args.username))?,
    };
    let (host, port) = rdesk_core::parse_address(&args.host)?;
    let mut config = SessionConfig::new(host, Credentials::parse(&args.username, password));
    config.port = port;
    config.desktop_width = args.width;
    config.desktop_height = args.height;
    // A screenshot has no business with the remote clipboard.
    config.clipboard = false;
    if args.no_nla {
        config.security = Security::Tls;
    }
    config.graphics_pipeline = !args.no_gfx;

    let (session, events) = rdesk_core::connect(config, &runtime);
    let started = Instant::now();
    let deadline = started + Duration::from_secs(args.timeout);
    let settle = Duration::from_secs(args.settle);
    let mut last_frame: Option<Instant> = None;
    let mut frames = 0u32;
    let mut saved = false;

    loop {
        let wait_until = match last_frame {
            Some(t) if !saved => (t + settle).min(deadline),
            _ => deadline,
        };
        let event = match tokio::time::timeout_at(wait_until, events.recv()).await {
            Ok(Ok(event)) => event,
            Ok(Err(_)) => bail!("session ended without a Disconnected event"),
            Err(_) => {
                if !saved && last_frame.is_some() {
                    save(&session, &args.output)?;
                    info!(frames, output = %args.output.display(), elapsed_ms = started.elapsed().as_millis(), "Saved screenshot");
                    saved = true;
                } else if !saved {
                    warn!("Time limit reached before any frame arrived");
                }
                session.disconnect();
                // Keep reading until the Disconnected event.
                last_frame = None;
                continue;
            }
        };
        match event {
            SessionEvent::CertificatePrompt(prompt) => {
                let cert = &prompt.certificate;
                let trusted = args.insecure
                    || args
                        .trust
                        .as_deref()
                        .is_some_and(|t| same_fingerprint(t, &cert.sha256_fingerprint));
                info!(
                    problem = %prompt.problem,
                    subject = %cert.subject,
                    issuer = %cert.issuer,
                    valid = %format_args!("{} – {}", cert.not_before, cert.not_after),
                    sha256 = %cert.sha256_fingerprint,
                    trusted,
                    "Certificate check"
                );
                if trusted {
                    prompt.accept();
                } else {
                    eprintln!(
                        "Rejected the certificate. To trust it, pass --trust {}",
                        cert.sha256_fingerprint
                    );
                    prompt.reject();
                }
            }
            SessionEvent::Connected { width, height } => {
                info!(width, height, elapsed_ms = started.elapsed().as_millis(), "Connected");
                if let Some(Monitors(monitors)) = &args.monitors {
                    session.set_monitors(monitors.clone(), 100);
                }
            }
            SessionEvent::DesktopResized { width, height } => info!(width, height, "Desktop resized"),
            SessionEvent::FrameReady => {
                frames += 1;
                last_frame = Some(Instant::now());
                // Acknowledge, so the next update notifies again.
                session.framebuffer().take_damage(|_| ());
            }
            SessionEvent::Pointer(_) | SessionEvent::Clipboard(_) => {}
            SessionEvent::Disconnected(reason) => {
                info!(%reason, "Disconnected");
                return Ok(if saved { 0 } else { 1 });
            }
        }
    }
}

fn save(session: &rdesk_core::SessionHandle, path: &std::path::Path) -> anyhow::Result<()> {
    let (width, height, rgba) = session.framebuffer().take_damage(|d| {
        let mut rgba = Vec::with_capacity(d.data.len());
        for px in d.data.as_chunks::<4>().0 {
            rgba.extend_from_slice(&[px[2], px[1], px[0], 0xFF]);
        }
        (d.width, d.height, rgba)
    });
    let image = image::RgbaImage::from_raw(width, height, rgba).context("framebuffer size mismatch")?;
    image.save(path).with_context(|| format!("save {}", path.display()))
}

fn same_fingerprint(a: &str, b: &str) -> bool {
    let normalize = |s: &str| {
        s.chars()
            .filter(char::is_ascii_hexdigit)
            .collect::<String>()
            .to_ascii_uppercase()
    };
    normalize(a) == normalize(b)
}
