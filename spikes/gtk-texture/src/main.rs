//! Phase 0 spike: can GTK4 present a 1920×1080 framebuffer, written by another thread, at 60 fps?
//!
//! A producer thread stands in for the RDP session task: it draws into a shared BGRX framebuffer at
//! 60 Hz and records dirty rectangles. The UI uploads on every frame-clock tick, either as a fresh
//! texture each frame (`--mode full`) or as an incremental update of the previous texture limited to
//! the dirty region (`--mode dirty`, GTK 4.16 `update_texture`/`update_region`).
//!
//! ```shell
//! cargo run --release -p gtk-texture -- --mode dirty --seconds 10
//! ```

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use clap::{Parser, ValueEnum};
use gtk::prelude::*;
use gtk::{cairo, gdk, glib};
use gtk4 as gtk;

static SIZE: std::sync::OnceLock<(usize, usize)> = std::sync::OnceLock::new();

fn width() -> usize {
    SIZE.get().unwrap().0
}

fn height() -> usize {
    SIZE.get().unwrap().1
}

fn stride() -> usize {
    width() * 4
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum Mode {
    /// Rebuild the whole texture every frame.
    Full,
    /// Update the previous texture, re-uploading only dirty rectangles.
    Dirty,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum Scene {
    /// Typical desktop: a moving window, typing, and a full repaint every 3 s.
    Desktop,
    /// Every pixel changes every frame (video / full-screen animation).
    Video,
}

#[derive(Parser, Debug, Clone)]
struct Args {
    #[arg(long, value_enum, default_value_t = Mode::Dirty)]
    mode: Mode,
    #[arg(long, value_enum, default_value_t = Scene::Desktop)]
    scene: Scene,
    /// Quit after this many seconds and print a summary (0 = run until closed).
    #[arg(long, default_value_t = 10)]
    seconds: u64,
    /// Framebuffer size, e.g. 1920x1080 or 3840x2160.
    #[arg(long, default_value = "1920x1080", value_parser = parse_size)]
    size: (usize, usize),
    /// Producer rate in frames per second.
    #[arg(long, default_value_t = 60)]
    rate: u32,
}

#[derive(Clone, Copy)]
struct Rect {
    x: usize,
    y: usize,
    w: usize,
    h: usize,
}

/// What the session task would own: the framebuffer plus damage accumulated since the last upload.
struct Shared {
    fb: Vec<u8>,
    dirty: Vec<Rect>,
    full_damage: bool,
    seq: u64,
}

fn parse_size(s: &str) -> Result<(usize, usize), String> {
    let (w, h) = s.split_once('x').ok_or("expected WIDTHxHEIGHT")?;
    let (w, h) = (
        w.parse().map_err(|_| "bad width")?,
        h.parse().map_err(|_| "bad height")?,
    );
    if w < 640 || h < 480 {
        return Err("minimum size is 640x480".into());
    }
    Ok((w, h))
}

fn main() -> glib::ExitCode {
    let args = Args::parse();
    SIZE.set(args.size).unwrap();
    let app = gtk::Application::builder()
        .application_id("io.github.porthole.GtkTextureSpike")
        .build();
    app.connect_activate(move |app| activate(app, args.clone()));
    // Our own flags are parsed by clap; don't let GApplication see them.
    app.run_with_args(&[] as &[&str])
}

fn activate(app: &gtk::Application, args: Args) {
    let shared = Arc::new(Mutex::new(Shared {
        fb: vec![0; stride() * height()],
        dirty: Vec::new(),
        full_damage: true,
        seq: 0,
    }));
    let stop = Arc::new(AtomicBool::new(false));
    let producer = {
        let (shared, stop, scene, rate) = (shared.clone(), stop.clone(), args.scene, args.rate);
        std::thread::spawn(move || produce(&shared, &stop, scene, rate))
    };

    let picture = gtk::Picture::builder()
        .content_fit(gtk::ContentFit::Contain)
        .can_shrink(true)
        .build();
    let window = gtk::ApplicationWindow::builder()
        .application(app)
        .title(format!("gtk-texture spike ({:?}, {:?})", args.mode, args.scene))
        .default_width(1280)
        .default_height(720)
        .child(&picture)
        .build();

    let stats = Rc::new(RefCell::new(Stats::new()));
    let ui = Rc::new(RefCell::new(UiState {
        last_seq: 0,
        prev: None,
    }));

    {
        let (shared, stats, ui, mode) = (shared.clone(), stats.clone(), ui.clone(), args.mode);
        picture.add_tick_callback(move |picture, clock| {
            stats.borrow_mut().tick(clock);
            if let Some(texture) = upload(&shared, &mut ui.borrow_mut(), &mut stats.borrow_mut(), mode) {
                picture.set_paintable(Some(&texture));
            }
            glib::ControlFlow::Continue
        });
    }

    {
        let stats = stats.clone();
        glib::timeout_add_seconds_local(1, move || {
            stats.borrow_mut().report_second();
            glib::ControlFlow::Continue
        });
    }

    let shutdown = {
        let (stop, stats, args) = (stop.clone(), stats.clone(), args.clone());
        let producer = RefCell::new(Some(producer));
        move || {
            stop.store(true, Ordering::Relaxed);
            if let Some(handle) = producer.borrow_mut().take() {
                let _ = handle.join();
            }
            stats.borrow().summary(&args);
        }
    };
    let shutdown = Rc::new(shutdown);

    if args.seconds > 0 {
        let (app, shutdown) = (app.clone(), shutdown.clone());
        glib::timeout_add_local_once(Duration::from_secs(args.seconds), move || {
            shutdown();
            app.quit();
        });
    } else {
        window.connect_close_request(move |_| {
            shutdown();
            glib::Propagation::Proceed
        });
    }

    window.present();
}

struct UiState {
    last_seq: u64,
    prev: Option<gdk::Texture>,
}

/// Takes the latest frame and damage from the producer and builds the texture to show.
fn upload(shared: &Mutex<Shared>, ui: &mut UiState, stats: &mut Stats, mode: Mode) -> Option<gdk::Texture> {
    let started = Instant::now();

    let (bytes, dirty, full_damage, seq, waited) = {
        let mut s = shared.lock().unwrap();
        let waited = started.elapsed();
        if s.seq == ui.last_seq {
            return None;
        }
        // GdkMemoryTexture keeps a reference to its bytes, so each texture needs its own copy of the frame.
        let bytes = glib::Bytes::from(&s.fb[..]);
        let dirty = std::mem::take(&mut s.dirty);
        let full = std::mem::replace(&mut s.full_damage, false);
        (bytes, dirty, full, s.seq, waited)
    };
    let copied = started.elapsed() - waited;

    let mut builder = gdk::MemoryTextureBuilder::new()
        .set_bytes(Some(&bytes))
        .set_width(width() as i32)
        .set_height(height() as i32)
        .set_stride(stride())
        .set_format(gdk::MemoryFormat::B8g8r8x8);

    let mut dirty_px = width() * height();
    if mode == Mode::Dirty
        && !full_damage
        && let Some(prev) = &ui.prev
    {
        let rects: Vec<cairo::RectangleInt> = dirty
            .iter()
            .map(|r| cairo::RectangleInt::new(r.x as i32, r.y as i32, r.w as i32, r.h as i32))
            .collect();
        let region = cairo::Region::create_rectangles(&rects);
        dirty_px = region_area(&region);
        builder = builder.set_update_texture(Some(prev)).set_update_region(Some(&region));
    }

    let texture = builder.build();
    stats.uploaded(seq - ui.last_seq, waited, copied, started.elapsed(), dirty_px);
    ui.last_seq = seq;
    ui.prev = Some(texture.clone());
    Some(texture)
}

fn region_area(region: &cairo::Region) -> usize {
    (0..region.num_rectangles())
        .map(|i| {
            let r = region.rectangle(i);
            (r.width() * r.height()) as usize
        })
        .sum()
}

// ---------------------------------------------------------------------------------------------
// Producer (stands in for the RDP session task)
// ---------------------------------------------------------------------------------------------

fn produce(shared: &Mutex<Shared>, stop: &AtomicBool, scene: Scene, rate: u32) {
    let period = Duration::from_secs_f64(1.0 / f64::from(rate));
    let mut next = Instant::now();
    let mut frame: u64 = 0;
    let (bw, bh) = (320usize, 240usize);
    let (mut bx, mut by, mut dx, mut dy) = (100isize, 100isize, 7isize, 5isize);

    while !stop.load(Ordering::Relaxed) {
        {
            let mut s = shared.lock().unwrap();
            match scene {
                Scene::Video => {
                    fill(
                        &mut s.fb,
                        Rect {
                            x: 0,
                            y: 0,
                            w: width(),
                            h: height(),
                        },
                        |x, y| gradient(x, y, frame),
                    );
                    s.full_damage = true;
                }
                Scene::Desktop => {
                    if frame.is_multiple_of(u64::from(rate) * 3) {
                        let shade = (frame / (u64::from(rate) * 3)) as u8;
                        fill(
                            &mut s.fb,
                            Rect {
                                x: 0,
                                y: 0,
                                w: width(),
                                h: height(),
                            },
                            |x, y| gradient(x, y, u64::from(shade) * 97),
                        );
                        s.full_damage = true;
                    }
                    // Moving "window": damage covers old and new positions.
                    let old = Rect {
                        x: bx as usize,
                        y: by as usize,
                        w: bw,
                        h: bh,
                    };
                    fill(&mut s.fb, old, |x, y| gradient(x, y, 0));
                    if bx + dx < 0 || (bx + dx) as usize + bw > width() {
                        dx = -dx;
                    }
                    if by + dy < 0 || (by + dy) as usize + bh > height() {
                        dy = -dy;
                    }
                    bx += dx;
                    by += dy;
                    let new = Rect {
                        x: bx as usize,
                        y: by as usize,
                        w: bw,
                        h: bh,
                    };
                    fill(&mut s.fb, new, |x, y| [(x * 3) as u8, (y * 2) as u8, 0xC0]);
                    s.dirty.push(old);
                    s.dirty.push(new);
                    // "Typing": one 16×32 glyph cell per frame along a text line.
                    let col = (frame as usize % 100) * 16;
                    let cell = Rect {
                        x: 40 + col,
                        y: 900,
                        w: 16,
                        h: 32,
                    };
                    let ink = (frame * 37) as u8;
                    fill(&mut s.fb, cell, |x, y| {
                        if (x + y) % 3 == 0 { [ink, 0, 0] } else { [255, 255, 255] }
                    });
                    s.dirty.push(cell);
                }
            }
            s.seq += 1;
        }
        frame += 1;
        next += period;
        let now = Instant::now();
        if next > now {
            std::thread::sleep(next - now);
        } else {
            next = now; // fell behind; don't try to catch up
        }
    }
}

fn gradient(x: usize, y: usize, t: u64) -> [u8; 3] {
    let t = t as usize;
    [(x + t) as u8, (y + 2 * t) as u8, ((x + y) / 4 + 3 * t) as u8]
}

/// Fills `rect` with `color(x, y) -> [b, g, r]`, writing BGRX pixels.
fn fill(fb: &mut [u8], rect: Rect, color: impl Fn(usize, usize) -> [u8; 3]) {
    for y in rect.y..rect.y + rect.h {
        let row = &mut fb[y * stride() + rect.x * 4..y * stride() + (rect.x + rect.w) * 4];
        for (i, px) in row.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            let [b, g, r] = color(rect.x + i, y);
            *px = [b, g, r, 0xFF];
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Statistics
// ---------------------------------------------------------------------------------------------

struct Stats {
    started: Instant,
    last_frame_time: Option<i64>,
    intervals_us: Vec<i64>,
    upload_us: Vec<u128>,
    copy_us: Vec<u128>,
    wait_us: Vec<u128>,
    uploads: u64,
    produced: u64,
    dirty_px: u64,
    second: (u64, u64, u128, i64), // uploads, produced, upload µs total, ticks
    fps: f64,
}

impl Stats {
    fn new() -> Self {
        Self {
            started: Instant::now(),
            last_frame_time: None,
            intervals_us: Vec::new(),
            upload_us: Vec::new(),
            copy_us: Vec::new(),
            wait_us: Vec::new(),
            uploads: 0,
            produced: 0,
            dirty_px: 0,
            second: (0, 0, 0, 0),
            fps: 0.0,
        }
    }

    /// Startup (window mapping, first uploads) is excluded from the summary.
    fn warmed_up(&self) -> bool {
        self.started.elapsed() > Duration::from_secs(1)
    }

    fn tick(&mut self, clock: &gdk::FrameClock) {
        let t = clock.frame_time();
        if let Some(prev) = self.last_frame_time
            && self.warmed_up()
        {
            self.intervals_us.push(t - prev);
        }
        self.last_frame_time = Some(t);
        self.fps = clock.fps();
        self.second.3 += 1;
    }

    fn uploaded(&mut self, frames: u64, wait: Duration, copy: Duration, total: Duration, dirty_px: usize) {
        self.second.0 += 1;
        self.second.1 += frames;
        self.second.2 += total.as_micros();
        if !self.warmed_up() {
            return;
        }
        self.uploads += 1;
        self.produced += frames;
        self.dirty_px += dirty_px as u64;
        self.wait_us.push(wait.as_micros());
        self.copy_us.push(copy.as_micros());
        self.upload_us.push(total.as_micros());
    }

    fn report_second(&mut self) {
        let (uploads, produced, upload_us, ticks) = std::mem::take(&mut self.second);
        println!(
            "t={:>4.1}s  ticks={ticks:>3}  clock_fps={:>5.1}  uploads={uploads:>3}  produced={produced:>3}  avg_upload={:.2} ms",
            self.started.elapsed().as_secs_f64(),
            self.fps,
            upload_us as f64 / uploads.max(1) as f64 / 1000.0,
        );
    }

    fn summary(&self, args: &Args) {
        let pct = |v: &[i64], p: f64| -> f64 {
            let mut v = v.to_vec();
            v.sort_unstable();
            v.get(((v.len() as f64 - 1.0) * p).round() as usize)
                .copied()
                .unwrap_or(0) as f64
                / 1000.0
        };
        let to_i = |v: &[u128]| v.iter().map(|&x| x as i64).collect::<Vec<_>>();
        // Measured window: everything after the 1 s warm-up.
        let secs = self.started.elapsed().as_secs_f64() - 1.0;
        let long = self.intervals_us.iter().filter(|&&i| i > 25_000).count();
        println!(
            "--- summary: mode={:?} scene={:?} size={}x{} measured {secs:.1}s (after 1 s warm-up)",
            args.mode,
            args.scene,
            width(),
            height()
        );
        println!(
            "frame clock: {:.1} ticks/s, interval p50={:.2} ms p99={:.2} ms max={:.2} ms, intervals >25 ms: {long}",
            self.intervals_us.len() as f64 / secs,
            pct(&self.intervals_us, 0.5),
            pct(&self.intervals_us, 0.99),
            pct(&self.intervals_us, 1.0),
        );
        println!(
            "uploads: {} ({:.1}/s) covering {} produced frames; avg dirty area {:.1}% of screen",
            self.uploads,
            self.uploads as f64 / secs,
            self.produced,
            100.0 * self.dirty_px as f64 / (self.uploads.max(1) as f64 * (width() * height()) as f64),
        );
        println!(
            "UI-thread cost per upload: lock wait p50={:.2} ms p99={:.2} ms; frame copy p50={:.2} ms p99={:.2} ms; \
             total p50={:.2} ms p99={:.2} ms",
            pct(&to_i(&self.wait_us), 0.5),
            pct(&to_i(&self.wait_us), 0.99),
            pct(&to_i(&self.copy_us), 0.5),
            pct(&to_i(&self.copy_us), 0.99),
            pct(&to_i(&self.upload_us), 0.5),
            pct(&to_i(&self.upload_us), 0.99),
        );
    }
}
