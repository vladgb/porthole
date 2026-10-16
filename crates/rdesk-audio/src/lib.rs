//! Audio through PipeWire: the PC's sound (RDPSND) and the microphone (AUDIN capture).
//!
//! The server sends PCM in one of the formats we offer, in large packets (Windows: about 170 ms
//! each) at an uneven pace. Samples go into a jitter buffer that a PipeWire playback stream,
//! running on its own thread, drains in real time: playback starts once a packet and some margin
//! are queued, so the next packet arrives before the queue runs dry. The buffer is capped so audio
//! never lags far behind the picture: when it overflows, the oldest samples are dropped.

use std::borrow::Cow;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use ironrdp_rdpsnd::client::RdpsndClientHandler;
use ironrdp_rdpsnd::pdu::{AudioFormat, PitchPdu, VolumePdu, WaveFormat};
use pipewire as pw;
use pw::spa;
use tracing::{debug, warn};

const CHANNELS: u16 = 2;
const BYTES_PER_FRAME: usize = 4; // 16-bit stereo
/// The jitter buffer holds at least this much before playing...
const MIN_TARGET_MS: usize = 60;
/// ...and at most this much, whatever the packet size.
const MAX_TARGET_MS: usize = 500;
/// Beyond the target, this much more may queue up before the oldest samples are dropped.
const MAX_EXCESS_MS: usize = 250;

/// 16-bit stereo PCM, which every Windows version can send.
static FORMATS: [AudioFormat; 2] = [pcm(48_000), pcm(44_100)];

const fn pcm(rate: u32) -> AudioFormat {
    AudioFormat {
        format: WaveFormat::PCM,
        n_channels: CHANNELS,
        n_samples_per_sec: rate,
        n_avg_bytes_per_sec: rate * BYTES_PER_FRAME as u32,
        n_block_align: BYTES_PER_FRAME as u16,
        bits_per_sample: 16,
        data: None,
    }
}

/// Plays the session's audio on the default PipeWire output.
#[derive(Debug)]
pub struct PipeWireOutput {
    player: Option<Player>,
    /// Left and right gain (0–0xFFFF each), as the server last set them.
    volume: Arc<AtomicU32>,
}

impl Default for PipeWireOutput {
    fn default() -> Self {
        Self::new()
    }
}

impl PipeWireOutput {
    pub fn new() -> Self {
        Self {
            player: None,
            volume: Arc::new(AtomicU32::new(u32::MAX)),
        }
    }
}

impl Drop for PipeWireOutput {
    fn drop(&mut self) {
        self.close();
    }
}

impl RdpsndClientHandler for PipeWireOutput {
    fn get_formats(&self) -> &[AudioFormat] {
        &FORMATS
    }

    fn wave(&mut self, format_no: usize, _ts: u32, data: Cow<'_, [u8]>) {
        let Some(format) = FORMATS.get(format_no) else {
            warn!(format_no, "Audio in a format we didn't offer");
            return;
        };
        if self.player.as_ref().is_some_and(|p| p.rate != format.n_samples_per_sec) {
            self.close();
        }
        if self.player.is_none() {
            match Player::start(format.n_samples_per_sec, Arc::clone(&self.volume)) {
                Ok(player) => self.player = Some(player),
                Err(e) => {
                    warn!(error = %e, "Couldn't open the audio output");
                    return;
                }
            }
        }
        if let Some(player) = &self.player {
            player.push(&data);
        }
    }

    fn set_volume(&mut self, volume: VolumePdu) {
        let packed = (u32::from(volume.volume_right) << 16) | u32::from(volume.volume_left);
        self.volume.store(packed, Ordering::Relaxed);
    }

    fn set_pitch(&mut self, _pitch: PitchPdu) {}

    fn close(&mut self) {
        if let Some(player) = self.player.take() {
            player.stop();
        }
    }
}

/// A PipeWire playback stream on its own thread.
struct Player {
    rate: u32,
    queue: Arc<Mutex<Jitter>>,
    quit: pw::channel::Sender<()>,
    thread: Option<JoinHandle<()>>,
}

impl std::fmt::Debug for Player {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Player")
            .field("rate", &self.rate)
            .finish_non_exhaustive()
    }
}

impl Player {
    fn start(rate: u32, volume: Arc<AtomicU32>) -> Result<Self, pw::Error> {
        let queue = Arc::new(Mutex::new(Jitter::new(rate)));
        let (quit, quit_rx) = pw::channel::channel::<()>();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let thread = std::thread::Builder::new()
            .name("rdesk-audio".to_owned())
            .spawn({
                let queue = Arc::clone(&queue);
                move || {
                    if let Err(e) = run(rate, &queue, &volume, quit_rx, &ready_tx) {
                        let _ = ready_tx.send(Err(e));
                    }
                }
            })
            .map_err(|_| pw::Error::CreationFailed)?;
        ready_rx.recv().map_err(|_| pw::Error::CreationFailed)??;
        debug!(rate, "Audio output open");
        Ok(Self {
            rate,
            queue,
            quit,
            thread: Some(thread),
        })
    }

    fn push(&self, data: &[u8]) {
        let mut queue = self.queue.lock().unwrap_or_else(|e| e.into_inner());
        queue.push(data, Instant::now());
    }

    fn stop(mut self) {
        let _ = self.quit.send(());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn run(
    rate: u32,
    queue: &Arc<Mutex<Jitter>>,
    volume: &Arc<AtomicU32>,
    quit: pw::channel::Receiver<()>,
    ready: &std::sync::mpsc::Sender<Result<(), pw::Error>>,
) -> Result<(), pw::Error> {
    pw::init();
    let mainloop = pw::main_loop::MainLoopRc::new(None)?;
    let context = pw::context::ContextRc::new(&mainloop, None)?;
    let core = context.connect_rc(None)?;
    // About 20 ms per buffer: low latency without stressing the scheduler.
    let latency = format!("{}/{rate}", rate / 50);
    let mut properties = pw::properties::properties! {
        *pw::keys::MEDIA_TYPE => "Audio",
        *pw::keys::MEDIA_CATEGORY => "Playback",
        *pw::keys::NODE_LATENCY => latency,
        *pw::keys::APP_NAME => "Remote Desktop",
    };
    // Plays to a specific output (node name or serial) instead of the default one.
    if let Ok(target) = std::env::var("RDESK_AUDIO_TARGET") {
        properties.insert("target.object", target);
    }
    let stream = pw::stream::StreamBox::new(&core, "Remote Desktop", properties)?;

    let queue = Arc::clone(queue);
    let volume = Arc::clone(volume);
    let _listener = stream
        .add_local_listener_with_user_data(())
        .process(move |stream, _| {
            let Some(mut buffer) = stream.dequeue_buffer() else {
                return;
            };
            // Fill only what this cycle needs: the buffer itself holds far more, and filling it
            // all would play long stretches of silence whenever the queue is short.
            let requested = usize::try_from(buffer.requested()).unwrap_or(usize::MAX);
            let datas = buffer.datas_mut();
            let data = &mut datas[0];
            let Some(out) = data.data() else { return };
            let wanted = if requested == 0 { rate as usize / 50 } else { requested };
            let frames = (out.len() / BYTES_PER_FRAME).min(wanted);
            let out = &mut out[..frames * BYTES_PER_FRAME];
            queue
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .pull(out, Instant::now());
            let gain = volume.load(Ordering::Relaxed);
            let (left, right) = (gain & 0xFFFF, gain >> 16);
            for (i, sample) in out.as_chunks_mut::<2>().0.iter_mut().enumerate() {
                let gain = if i % 2 == 0 { left } else { right };
                let scaled = (i32::from(i16::from_le_bytes(*sample)) * gain as i32) >> 16;
                *sample = (scaled as i16).to_le_bytes();
            }
            let chunk = data.chunk_mut();
            *chunk.offset_mut() = 0;
            *chunk.stride_mut() = BYTES_PER_FRAME as _;
            *chunk.size_mut() = (frames * BYTES_PER_FRAME) as _;
        })
        .register()?;

    let mut info = spa::param::audio::AudioInfoRaw::new();
    info.set_format(spa::param::audio::AudioFormat::S16LE);
    info.set_rate(rate);
    info.set_channels(u32::from(CHANNELS));
    let mut position = [0; spa::param::audio::MAX_CHANNELS];
    position[0] = spa::sys::SPA_AUDIO_CHANNEL_FL;
    position[1] = spa::sys::SPA_AUDIO_CHANNEL_FR;
    info.set_position(position);
    let format = spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &spa::pod::Value::Object(spa::pod::Object {
            type_: spa::sys::SPA_TYPE_OBJECT_Format,
            id: spa::sys::SPA_PARAM_EnumFormat,
            properties: info.into(),
        }),
    )
    .map_err(|_| pw::Error::CreationFailed)?
    .0
    .into_inner();
    let mut params = [spa::pod::Pod::from_bytes(&format).ok_or(pw::Error::CreationFailed)?];
    stream.connect(
        spa::utils::Direction::Output,
        None,
        pw::stream::StreamFlags::AUTOCONNECT
            | pw::stream::StreamFlags::MAP_BUFFERS
            | pw::stream::StreamFlags::RT_PROCESS,
        &mut params,
    )?;

    let _quit = quit.attach(mainloop.loop_(), {
        let mainloop = mainloop.clone();
        move |()| mainloop.quit()
    });
    let _ = ready.send(Ok(()));
    mainloop.run();
    Ok(())
}

/// Smooths out the server's uneven delivery (see the module docs). Works in bytes of 16-bit
/// stereo frames.
struct Jitter {
    samples: VecDeque<u8>,
    /// Bytes per millisecond.
    per_ms: usize,
    /// Draining; false while (re)filling up to the target.
    playing: bool,
    last_push: Option<Instant>,
    /// The largest packet so far: the queue must outlast the wait for the next one.
    largest_packet: usize,
}

impl Jitter {
    fn new(rate: u32) -> Self {
        Self {
            samples: VecDeque::new(),
            per_ms: rate as usize * BYTES_PER_FRAME / 1000,
            playing: false,
            last_push: None,
            largest_packet: 0,
        }
    }

    /// How much to queue before playing: a packet and a quarter, within limits.
    fn target(&self) -> usize {
        (self.largest_packet + self.largest_packet / 4)
            .clamp(MIN_TARGET_MS * self.per_ms, MAX_TARGET_MS * self.per_ms)
            .next_multiple_of(BYTES_PER_FRAME)
    }

    fn push(&mut self, data: &[u8], now: Instant) {
        self.largest_packet = self.largest_packet.max(data.len());
        self.last_push = Some(now);
        self.samples.extend(data);
        let max = self.target() + MAX_EXCESS_MS * self.per_ms;
        if self.samples.len() > max {
            // Drop whole frames from the front so channels stay aligned.
            let excess = (self.samples.len() - max)
                .next_multiple_of(BYTES_PER_FRAME)
                .min(self.samples.len());
            self.samples.drain(..excess);
        }
    }

    /// Fills `out` with what's due, silence where nothing is.
    fn pull(&mut self, out: &mut [u8], now: Instant) {
        if !self.playing && !self.samples.is_empty() {
            let target = self.target();
            // A sound's last bit never reaches the target: play it once nothing more is coming.
            let idle = self
                .last_push
                .is_some_and(|t| now.duration_since(t) >= Duration::from_millis((target / self.per_ms.max(1)) as u64));
            self.playing = self.samples.len() >= target || idle;
        }
        let mut filled = 0;
        if self.playing {
            filled = out.len().min(self.samples.len());
            for (dst, src) in out[..filled].iter_mut().zip(self.samples.drain(..filled)) {
                *dst = src;
            }
            if filled < out.len() {
                // Ran dry: build the margin up again before resuming.
                self.playing = false;
            }
        }
        out[filled..].fill(0);
    }
}

/// Receives captured interleaved 16-bit little-endian samples.
pub type CaptureSink = Box<dyn FnMut(&[u8]) + Send>;

/// Records 16-bit PCM from the default PipeWire input until dropped.
pub struct Capture {
    quit: pw::channel::Sender<()>,
    thread: Option<JoinHandle<()>>,
}

impl std::fmt::Debug for Capture {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Capture").finish_non_exhaustive()
    }
}

impl Capture {
    pub fn start(rate: u32, channels: u16, sink: CaptureSink) -> Result<Self, pw::Error> {
        let (quit, quit_rx) = pw::channel::channel::<()>();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let thread = std::thread::Builder::new()
            .name("rdesk-microphone".to_owned())
            .spawn(move || {
                if let Err(e) = run_capture(rate, channels, sink, quit_rx, &ready_tx) {
                    let _ = ready_tx.send(Err(e));
                }
            })
            .map_err(|_| pw::Error::CreationFailed)?;
        ready_rx.recv().map_err(|_| pw::Error::CreationFailed)??;
        debug!(rate, channels, "Microphone open");
        Ok(Self {
            quit,
            thread: Some(thread),
        })
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        let _ = self.quit.send(());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn run_capture(
    rate: u32,
    channels: u16,
    mut sink: CaptureSink,
    quit: pw::channel::Receiver<()>,
    ready: &std::sync::mpsc::Sender<Result<(), pw::Error>>,
) -> Result<(), pw::Error> {
    pw::init();
    let mainloop = pw::main_loop::MainLoopRc::new(None)?;
    let context = pw::context::ContextRc::new(&mainloop, None)?;
    let core = context.connect_rc(None)?;
    let mut properties = pw::properties::properties! {
        *pw::keys::MEDIA_TYPE => "Audio",
        *pw::keys::MEDIA_CATEGORY => "Capture",
        *pw::keys::MEDIA_ROLE => "Communication",
        *pw::keys::NODE_LATENCY => format!("{}/{rate}", rate / 50),
        *pw::keys::APP_NAME => "Remote Desktop",
    };
    // Records from a specific input (node name or serial) instead of the default one. A sink's
    // name records what plays on it.
    if let Ok(target) = std::env::var("RDESK_MIC_TARGET") {
        properties.insert("target.object", target);
        properties.insert("stream.capture.sink", "true");
    }
    let stream = pw::stream::StreamBox::new(&core, "Remote Desktop microphone", properties)?;
    let _listener = stream
        .add_local_listener_with_user_data(())
        .process(move |stream, _| {
            let Some(mut buffer) = stream.dequeue_buffer() else {
                return;
            };
            let datas = buffer.datas_mut();
            let data = &mut datas[0];
            let (offset, size) = {
                let chunk = data.chunk();
                (chunk.offset() as usize, chunk.size() as usize)
            };
            if let Some(samples) = data.data()
                && let Some(captured) = samples.get(offset..offset + size)
            {
                sink(captured);
            }
        })
        .register()?;

    let mut info = spa::param::audio::AudioInfoRaw::new();
    info.set_format(spa::param::audio::AudioFormat::S16LE);
    info.set_rate(rate);
    info.set_channels(u32::from(channels));
    let mut position = [0; spa::param::audio::MAX_CHANNELS];
    if channels == 1 {
        position[0] = spa::sys::SPA_AUDIO_CHANNEL_MONO;
    } else {
        position[0] = spa::sys::SPA_AUDIO_CHANNEL_FL;
        position[1] = spa::sys::SPA_AUDIO_CHANNEL_FR;
    }
    info.set_position(position);
    let format = spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &spa::pod::Value::Object(spa::pod::Object {
            type_: spa::sys::SPA_TYPE_OBJECT_Format,
            id: spa::sys::SPA_PARAM_EnumFormat,
            properties: info.into(),
        }),
    )
    .map_err(|_| pw::Error::CreationFailed)?
    .0
    .into_inner();
    let mut params = [spa::pod::Pod::from_bytes(&format).ok_or(pw::Error::CreationFailed)?];
    stream.connect(
        spa::utils::Direction::Input,
        None,
        pw::stream::StreamFlags::AUTOCONNECT
            | pw::stream::StreamFlags::MAP_BUFFERS
            | pw::stream::StreamFlags::RT_PROCESS,
        &mut params,
    )?;

    let _quit = quit.attach(mainloop.loop_(), {
        let mainloop = mainloop.clone();
        move |()| mainloop.quit()
    });
    let _ = ready.send(Ok(()));
    mainloop.run();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: u32 = 48_000;
    const PER_MS: usize = 48 * BYTES_PER_FRAME;

    fn packet(ms: usize, value: u8) -> Vec<u8> {
        vec![value; ms * PER_MS]
    }

    #[test]
    fn waits_for_a_packet_and_margin_then_plays_without_gaps() {
        let start = Instant::now();
        let at = |ms: u64| start + Duration::from_millis(ms);
        let mut jitter = Jitter::new(RATE);
        let mut out = vec![0xFF; 10 * PER_MS];

        jitter.push(&packet(170, 1), at(0));
        jitter.pull(&mut out, at(5));
        assert!(out.iter().all(|&b| b == 0), "one packet is below the target: silence");

        jitter.push(&packet(170, 2), at(180));
        // 340 ms queued, target 212 ms: plays, and 10 ms pulls every 10 ms never run dry before
        // the next packet arrives 180 ms later.
        for step in 0..18 {
            jitter.pull(&mut out, at(181 + step * 10));
            assert!(out.iter().all(|&b| b != 0), "gap at step {step}");
        }
        jitter.push(&packet(170, 3), at(360));
        for step in 0..18 {
            jitter.pull(&mut out, at(361 + step * 10));
            assert!(out.iter().all(|&b| b != 0), "gap after the third packet at step {step}");
        }
    }

    #[test]
    fn plays_a_short_tail_once_nothing_more_comes() {
        let start = Instant::now();
        let mut jitter = Jitter::new(RATE);
        let mut out = vec![0; 10 * PER_MS];
        jitter.push(&packet(20, 7), start);
        jitter.pull(&mut out, start + Duration::from_millis(10));
        assert!(out.iter().all(|&b| b == 0));
        jitter.pull(&mut out, start + Duration::from_millis(100));
        assert!(out.iter().all(|&b| b == 7));
    }

    #[test]
    fn drops_the_oldest_audio_when_far_behind() {
        let start = Instant::now();
        let mut jitter = Jitter::new(RATE);
        for i in 0..20 {
            jitter.push(&packet(100, i), start);
        }
        assert!(jitter.samples.len() <= jitter.target() + MAX_EXCESS_MS * PER_MS);
        assert_eq!(jitter.samples.len() % BYTES_PER_FRAME, 0);
        assert_eq!(jitter.samples.back(), Some(&19));
    }
}
