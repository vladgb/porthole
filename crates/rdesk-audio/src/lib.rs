//! Remote audio playback (RDPSND) through PipeWire.
//!
//! The server sends PCM in one of the formats we offer. Samples go into a short queue that a
//! PipeWire playback stream, running on its own thread, drains in real time. The queue is capped
//! so audio never lags far behind the picture: when it overflows, the oldest samples are dropped.

use std::borrow::Cow;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use ironrdp_rdpsnd::client::RdpsndClientHandler;
use ironrdp_rdpsnd::pdu::{AudioFormat, PitchPdu, VolumePdu, WaveFormat};
use pipewire as pw;
use pw::spa;
use tracing::{debug, warn};

const CHANNELS: u16 = 2;
const BYTES_PER_FRAME: usize = 4; // 16-bit stereo
/// At most this much audio waits to be played.
const MAX_QUEUED_MS: usize = 250;

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
    queue: Arc<Mutex<VecDeque<u8>>>,
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
        let queue = Arc::new(Mutex::new(VecDeque::new()));
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
        let max = self.rate as usize * BYTES_PER_FRAME * MAX_QUEUED_MS / 1000;
        let mut queue = self.queue.lock().unwrap_or_else(|e| e.into_inner());
        queue.extend(data);
        if queue.len() > max {
            // Drop whole frames from the front so channels stay aligned.
            let excess = (queue.len() - max).next_multiple_of(BYTES_PER_FRAME).min(queue.len());
            queue.drain(..excess);
        }
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
    queue: &Arc<Mutex<VecDeque<u8>>>,
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
        *pw::keys::MEDIA_ROLE => "Communication",
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
            let datas = buffer.datas_mut();
            let data = &mut datas[0];
            let Some(out) = data.data() else { return };
            let frames = out.len() / BYTES_PER_FRAME;
            let gain = volume.load(Ordering::Relaxed);
            let (left, right) = (gain & 0xFFFF, gain >> 16);
            let mut queue = queue.lock().unwrap_or_else(|e| e.into_inner());
            for (i, sample) in out[..frames * BYTES_PER_FRAME]
                .as_chunks_mut::<2>()
                .0
                .iter_mut()
                .enumerate()
            {
                let value = match (queue.pop_front(), queue.pop_front()) {
                    (Some(lo), Some(hi)) => i16::from_le_bytes([lo, hi]),
                    // Underrun: silence until more audio arrives.
                    _ => 0,
                };
                let gain = if i % 2 == 0 { left } else { right };
                let scaled = (i32::from(value) * gain as i32) >> 16;
                *sample = (scaled as i16).to_le_bytes();
            }
            drop(queue);
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
