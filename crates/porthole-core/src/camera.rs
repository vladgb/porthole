//! Webcams: video capture redirection (MS-RDPECAM).
//!
//! On the enumeration channel we negotiate the protocol version and announce each local camera
//! with the name of a dynamic channel of its own. The server opens that channel, asks for the
//! camera's streams and formats, starts a stream in one of them and then requests samples one at
//! a time. Each request is answered with the newest captured frame: older frames are dropped
//! rather than queued, so the remote picture never lags behind.

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};

use ironrdp::core::{Encode, EncodeResult, WriteCursor, impl_as_any};
use ironrdp::dvc::{DvcChannelListener, DvcClientProcessor, DvcEncode, DvcMessage, DvcProcessor};
use ironrdp::pdu::{PduResult, pdu_other_err};
use tokio::sync::mpsc;
use tracing::{debug, info, trace, warn};

pub(crate) const ENUMERATOR_CHANNEL: &str = "RDCamera_Device_Enumerator";
/// Version 2 adds camera properties (brightness and so on). Windows asks for them right after
/// activating a camera and drops the camera when the list is empty, so we stay on version 1.
const MAX_VERSION: u8 = 1;

const MSG_SUCCESS: u8 = 0x01;
const MSG_ERROR: u8 = 0x02;
const MSG_SELECT_VERSION_REQUEST: u8 = 0x03;
const MSG_SELECT_VERSION_RESPONSE: u8 = 0x04;
const MSG_DEVICE_ADDED: u8 = 0x05;
const MSG_ACTIVATE_DEVICE: u8 = 0x07;
const MSG_DEACTIVATE_DEVICE: u8 = 0x08;
const MSG_STREAM_LIST_REQUEST: u8 = 0x09;
const MSG_STREAM_LIST_RESPONSE: u8 = 0x0A;
const MSG_MEDIA_TYPE_LIST_REQUEST: u8 = 0x0B;
const MSG_MEDIA_TYPE_LIST_RESPONSE: u8 = 0x0C;
const MSG_CURRENT_MEDIA_TYPE_REQUEST: u8 = 0x0D;
const MSG_CURRENT_MEDIA_TYPE_RESPONSE: u8 = 0x0E;
const MSG_START_STREAMS: u8 = 0x0F;
const MSG_STOP_STREAMS: u8 = 0x10;
const MSG_SAMPLE_REQUEST: u8 = 0x11;
const MSG_SAMPLE_RESPONSE: u8 = 0x12;
const MSG_SAMPLE_ERROR: u8 = 0x13;
const MSG_PROPERTY_LIST_REQUEST: u8 = 0x14;
const MSG_PROPERTY_LIST_RESPONSE: u8 = 0x15;

const ERROR_INVALID_MESSAGE: u32 = 0x02;
const ERROR_NOT_INITIALIZED: u32 = 0x03;
const ERROR_INVALID_STREAM_NUMBER: u32 = 0x05;
const ERROR_INVALID_MEDIA_TYPE: u32 = 0x06;
const ERROR_ITEM_NOT_FOUND: u32 = 0x08;

const MEDIA_TYPE_SIZE: usize = 26;
const DECODING_REQUIRED: u8 = 0x01;

/// How a camera encodes its frames (the protocol's codes).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum VideoFormat {
    H264 = 1,
    Mjpg = 2,
    Yuy2 = 3,
    Nv12 = 4,
    I420 = 5,
    Rgb24 = 6,
    Rgb32 = 7,
}

impl VideoFormat {
    fn from_code(code: u8) -> Option<Self> {
        Some(match code {
            1 => Self::H264,
            2 => Self::Mjpg,
            3 => Self::Yuy2,
            4 => Self::Nv12,
            5 => Self::I420,
            6 => Self::Rgb24,
            7 => Self::Rgb32,
            _ => return None,
        })
    }

    fn compressed(self) -> bool {
        matches!(self, Self::H264 | Self::Mjpg)
    }
}

/// One way a camera can capture.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct VideoMode {
    pub format: VideoFormat,
    pub width: u32,
    pub height: u32,
    /// Frames per second as a fraction, e.g. 30/1.
    pub fps_num: u32,
    pub fps_den: u32,
}

impl VideoMode {
    fn encode(&self, out: &mut Vec<u8>) {
        out.push(self.format as u8);
        for value in [self.width, self.height, self.fps_num, self.fps_den, 1, 1] {
            out.extend_from_slice(&value.to_le_bytes());
        }
        out.push(if self.format.compressed() { DECODING_REQUIRED } else { 0 });
    }

    fn decode(bytes: &[u8]) -> Option<Self> {
        let bytes = bytes.get(..MEDIA_TYPE_SIZE)?;
        let u32_at = |i: usize| u32::from_le_bytes([bytes[i], bytes[i + 1], bytes[i + 2], bytes[i + 3]]);
        Some(Self {
            format: VideoFormat::from_code(bytes[0])?,
            width: u32_at(1),
            height: u32_at(5),
            fps_num: u32_at(9),
            fps_den: u32_at(13),
        })
    }
}

/// A local camera.
#[derive(Debug, Clone)]
pub struct CameraDevice {
    /// Identifies the camera to [`Cameras::start`].
    pub id: String,
    /// Shown on the PC.
    pub name: String,
    /// What it can capture, preferred first.
    pub modes: Vec<VideoMode>,
}

/// Receives captured frames.
pub type FrameSink = Box<dyn FnMut(&[u8]) + Send>;

/// Access to the local cameras.
pub trait Cameras: Send + Sync {
    /// The cameras present now. Called while connecting.
    fn devices(&self) -> Vec<CameraDevice>;

    /// Starts capturing; `sink` gets each frame in `mode`. Capture stops when the returned value
    /// is dropped.
    fn start(&self, device: &str, mode: VideoMode, sink: FrameSink) -> Result<Box<dyn Send>, String>;
}

/// The cameras the PC may use; `None` in the session config: no camera redirection.
pub type CameraBackend = Arc<dyn Cameras>;

/// A message we send, already encoded.
struct Raw(Vec<u8>);

impl Encode for Raw {
    fn encode(&self, dst: &mut WriteCursor<'_>) -> EncodeResult<()> {
        dst.write_slice(&self.0);
        Ok(())
    }

    fn name(&self) -> &'static str {
        "RDPECAM"
    }

    fn size(&self) -> usize {
        self.0.len()
    }
}

impl DvcEncode for Raw {}

fn message(version: u8, id: u8, body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(2 + body.len());
    out.push(version);
    out.push(id);
    out.extend_from_slice(body);
    out
}

fn reply(version: u8, id: u8, body: &[u8]) -> DvcMessage {
    Box::new(Raw(message(version, id, body)))
}

/// Frames for the session loop to send, each with the channel to send it on.
pub(crate) struct Samples {
    pub receiver: mpsc::UnboundedReceiver<(u32, Vec<u8>)>,
}

impl Samples {
    pub(crate) fn messages(message: Vec<u8>) -> Vec<DvcMessage> {
        vec![Box::new(Raw(message))]
    }
}

/// The enumeration channel and a listener per camera, to register with DRDYNVC.
pub(crate) struct Channels {
    pub enumerator: Enumerator,
    pub devices: Vec<DeviceListener>,
}

/// The channels for `devices`, and the frames they will produce.
pub(crate) fn channels(backend: &CameraBackend, devices: Vec<CameraDevice>) -> (Channels, Samples) {
    let version = Arc::new(AtomicU8::new(MAX_VERSION));
    let (sender, receiver) = mpsc::unbounded_channel();
    let mut announced = Vec::new();
    let mut channels = Vec::new();
    for (index, device) in devices.into_iter().enumerate() {
        let channel = format!("RDCamera_Device_{index}");
        announced.push((device.name.clone(), channel.clone()));
        channels.push(DeviceListener {
            channel,
            device: Arc::new(Mutex::new(Device {
                device,
                backend: Arc::clone(backend),
                version: Arc::clone(&version),
                stream: Arc::default(),
                samples: sender.clone(),
                current: None,
                capture: None,
                open: 0,
            })),
        });
    }
    let enumerator = Enumerator { version, announced };
    (
        Channels {
            enumerator,
            devices: channels,
        },
        Samples { receiver },
    )
}

pub(crate) struct Enumerator {
    version: Arc<AtomicU8>,
    /// (display name, channel name) per camera.
    announced: Vec<(String, String)>,
}

impl_as_any!(Enumerator);

impl DvcProcessor for Enumerator {
    fn channel_name(&self) -> &str {
        ENUMERATOR_CHANNEL
    }

    fn start(&mut self, _channel_id: u32) -> PduResult<Vec<DvcMessage>> {
        Ok(vec![reply(MAX_VERSION, MSG_SELECT_VERSION_REQUEST, &[])])
    }

    fn process(&mut self, _channel_id: u32, payload: &[u8]) -> PduResult<Vec<DvcMessage>> {
        match payload {
            [version, MSG_SELECT_VERSION_RESPONSE, ..] => {
                let version = (*version).clamp(1, MAX_VERSION);
                self.version.store(version, Ordering::Relaxed);
                info!(version, cameras = self.announced.len(), "Camera redirection ready");
                Ok(self
                    .announced
                    .iter()
                    .map(|(name, channel)| {
                        let mut body: Vec<u8> = name.encode_utf16().chain([0]).flat_map(u16::to_le_bytes).collect();
                        body.extend(channel.bytes().filter(u8::is_ascii));
                        body.push(0);
                        reply(version, MSG_DEVICE_ADDED, &body)
                    })
                    .collect())
            }
            _ => {
                debug!(message = ?payload.get(..2), "Unexpected camera enumeration message");
                Ok(Vec::new())
            }
        }
    }
}

impl DvcClientProcessor for Enumerator {}

/// Shared between a camera and its capture thread.
#[derive(Default)]
struct Stream {
    /// The channel the server asks for samples on.
    channel_id: u32,
    running: bool,
    /// Samples the server asked for that weren't available yet.
    requested: u32,
    /// The newest frame, waiting for a request.
    latest: Option<Vec<u8>>,
}

/// Opens a channel to a camera whenever the server asks: Windows uses several at once (one for
/// control, another for the samples), all with the camera's channel name.
pub(crate) struct DeviceListener {
    channel: String,
    device: Arc<Mutex<Device>>,
}

impl DvcChannelListener for DeviceListener {
    fn channel_name(&self) -> &str {
        &self.channel
    }

    fn create(&mut self, _channel_id: u32) -> Option<Box<dyn DvcProcessor>> {
        Some(Box::new(DeviceConnection {
            channel: self.channel.clone(),
            device: Arc::clone(&self.device),
        }))
    }
}

/// One of the channels to a camera.
struct DeviceConnection {
    channel: String,
    device: Arc<Mutex<Device>>,
}

impl_as_any!(DeviceConnection);

impl DeviceConnection {
    fn device(&self) -> std::sync::MutexGuard<'_, Device> {
        self.device.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl DvcProcessor for DeviceConnection {
    fn channel_name(&self) -> &str {
        &self.channel
    }

    fn start(&mut self, _channel_id: u32) -> PduResult<Vec<DvcMessage>> {
        self.device().open += 1;
        Ok(Vec::new())
    }

    fn process(&mut self, channel_id: u32, payload: &[u8]) -> PduResult<Vec<DvcMessage>> {
        self.device().handle(channel_id, payload)
    }

    fn close(&mut self, _channel_id: u32) {
        let mut device = self.device();
        device.open = device.open.saturating_sub(1);
        if device.open == 0 {
            device.stop();
        }
    }
}

impl DvcClientProcessor for DeviceConnection {}

/// A camera as the server sees it, shared by its channels.
struct Device {
    device: CameraDevice,
    backend: CameraBackend,
    version: Arc<AtomicU8>,
    stream: Arc<Mutex<Stream>>,
    samples: mpsc::UnboundedSender<(u32, Vec<u8>)>,
    current: Option<VideoMode>,
    capture: Option<Box<dyn Send>>,
    /// Channels open to it.
    open: usize,
}

impl Device {
    fn version(&self) -> u8 {
        self.version.load(Ordering::Relaxed)
    }

    fn success(&self) -> Vec<DvcMessage> {
        vec![reply(self.version(), MSG_SUCCESS, &[])]
    }

    fn error(&self, code: u32) -> Vec<DvcMessage> {
        vec![reply(self.version(), MSG_ERROR, &code.to_le_bytes())]
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Stream> {
        self.stream.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn start_streams(&mut self, body: &[u8]) -> Vec<DvcMessage> {
        // One stream (index 0) and its media type.
        let Some((&index, media_type)) = body.split_first() else {
            return self.error(ERROR_INVALID_MESSAGE);
        };
        if index != 0 {
            return self.error(ERROR_INVALID_STREAM_NUMBER);
        }
        let Some(requested) = VideoMode::decode(media_type) else {
            return self.error(ERROR_INVALID_MEDIA_TYPE);
        };
        // Match on what identifies the mode; the server may round the frame rate.
        let Some(mode) = self
            .device
            .modes
            .iter()
            .find(|m| m.format == requested.format && m.width == requested.width && m.height == requested.height)
            .copied()
        else {
            warn!(?requested, "Server asked for a camera mode we didn't offer");
            return self.error(ERROR_INVALID_MEDIA_TYPE);
        };
        self.stop();
        let stream = Arc::clone(&self.stream);
        let samples = self.samples.clone();
        let version = Arc::clone(&self.version);
        let sink = Box::new(move |frame: &[u8]| {
            let mut stream = stream.lock().unwrap_or_else(|e| e.into_inner());
            if !stream.running {
                return;
            }
            if stream.requested > 0 {
                stream.requested -= 1;
                let _ = samples.send((stream.channel_id, sample(version.load(Ordering::Relaxed), frame)));
            } else {
                stream.latest = Some(frame.to_vec());
            }
        });
        self.lock().running = true;
        match self.backend.start(&self.device.id, mode, sink) {
            Ok(capture) => {
                info!(camera = %self.device.name, ?mode, "Camera on");
                self.capture = Some(capture);
                self.current = Some(mode);
                self.success()
            }
            Err(e) => {
                warn!(camera = %self.device.name, error = %e, "Couldn't start the camera");
                self.lock().running = false;
                self.error(ERROR_NOT_INITIALIZED)
            }
        }
    }

    fn stop(&mut self) {
        {
            let mut stream = self.lock();
            stream.running = false;
            stream.requested = 0;
            stream.latest = None;
        }
        if self.capture.take().is_some() {
            info!(camera = %self.device.name, "Camera off");
        }
    }

    fn sample_request(&mut self, channel_id: u32) -> Vec<DvcMessage> {
        let mut stream = self.lock();
        stream.channel_id = channel_id;
        if !stream.running {
            drop(stream);
            return vec![reply(
                self.version(),
                MSG_SAMPLE_ERROR,
                &[&[0u8][..], &ERROR_NOT_INITIALIZED.to_le_bytes()].concat(),
            )];
        }
        match stream.latest.take() {
            Some(frame) => vec![Box::new(Raw(sample(self.version(), &frame)))],
            None => {
                stream.requested += 1;
                Vec::new()
            }
        }
    }
}

/// A Sample Response for stream 0.
fn sample(version: u8, frame: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(3 + frame.len());
    out.extend_from_slice(&[version, MSG_SAMPLE_RESPONSE, 0]);
    out.extend_from_slice(frame);
    out
}

impl Device {
    fn handle(&mut self, channel_id: u32, payload: &[u8]) -> PduResult<Vec<DvcMessage>> {
        let [_version, id, body @ ..] = payload else {
            return Err(pdu_other_err!("camera message too short"));
        };
        trace!(camera = %self.device.name, message = id, len = body.len(), "Camera request");
        let version = self.version();
        Ok(match *id {
            MSG_ACTIVATE_DEVICE => self.success(),
            MSG_DEACTIVATE_DEVICE => {
                self.stop();
                self.success()
            }
            MSG_STREAM_LIST_REQUEST => {
                // One color capture stream, selected, not shared.
                let stream = [0x01, 0x00, 0x01, 0x01, 0x00];
                vec![reply(version, MSG_STREAM_LIST_RESPONSE, &stream)]
            }
            MSG_MEDIA_TYPE_LIST_REQUEST if body.first() == Some(&0) => {
                let mut list = Vec::with_capacity(self.device.modes.len() * MEDIA_TYPE_SIZE);
                for mode in &self.device.modes {
                    mode.encode(&mut list);
                }
                debug!(camera = %self.device.name, modes = self.device.modes.len(), "Camera modes asked for");
                vec![reply(version, MSG_MEDIA_TYPE_LIST_RESPONSE, &list)]
            }
            MSG_CURRENT_MEDIA_TYPE_REQUEST if body.first() == Some(&0) => {
                match self.current.or_else(|| self.device.modes.first().copied()) {
                    Some(mode) => {
                        let mut out = Vec::with_capacity(MEDIA_TYPE_SIZE);
                        mode.encode(&mut out);
                        vec![reply(version, MSG_CURRENT_MEDIA_TYPE_RESPONSE, &out)]
                    }
                    None => self.error(ERROR_ITEM_NOT_FOUND),
                }
            }
            MSG_MEDIA_TYPE_LIST_REQUEST | MSG_CURRENT_MEDIA_TYPE_REQUEST => self.error(ERROR_INVALID_STREAM_NUMBER),
            MSG_START_STREAMS => self.start_streams(body),
            MSG_STOP_STREAMS => {
                self.stop();
                self.success()
            }
            MSG_SAMPLE_REQUEST => self.sample_request(channel_id),
            // No adjustable properties (brightness and so on).
            MSG_PROPERTY_LIST_REQUEST => vec![reply(version, MSG_PROPERTY_LIST_RESPONSE, &[])],
            other => {
                debug!(message = other, "Unsupported camera request");
                self.error(ERROR_ITEM_NOT_FOUND)
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encoded(messages: &[DvcMessage]) -> Vec<Vec<u8>> {
        messages
            .iter()
            .map(|m| {
                let mut buf = vec![0; m.size()];
                m.encode(&mut WriteCursor::new(&mut buf)).unwrap();
                buf
            })
            .collect()
    }

    /// Hands out frames pushed through the sink it keeps.
    #[derive(Default)]
    struct FakeCameras {
        sink: Mutex<Option<FrameSink>>,
        started: Mutex<Option<VideoMode>>,
    }

    impl Cameras for FakeCameras {
        fn devices(&self) -> Vec<CameraDevice> {
            Vec::new()
        }

        fn start(&self, _device: &str, mode: VideoMode, sink: FrameSink) -> Result<Box<dyn Send>, String> {
            *self.sink.lock().unwrap() = Some(sink);
            *self.started.lock().unwrap() = Some(mode);
            Ok(Box::new(()))
        }
    }

    const MJPG_720: VideoMode = VideoMode {
        format: VideoFormat::Mjpg,
        width: 1280,
        height: 720,
        fps_num: 30,
        fps_den: 1,
    };

    fn camera() -> CameraDevice {
        CameraDevice {
            id: "cam".into(),
            name: "Cam".into(),
            modes: vec![MJPG_720],
        }
    }

    #[test]
    fn negotiates_announces_and_streams_newest_frames() {
        let fake = Arc::new(FakeCameras::default());
        let backend: CameraBackend = fake.clone();
        let (mut channels, mut samples) = channels(&backend, vec![camera()]);
        let enumerator = &mut channels.enumerator;

        assert_eq!(
            encoded(&enumerator.start(5).unwrap()),
            [vec![1, MSG_SELECT_VERSION_REQUEST]]
        );
        let added = encoded(&enumerator.process(5, &[1, MSG_SELECT_VERSION_RESPONSE]).unwrap());
        let mut expected = vec![1, MSG_DEVICE_ADDED, b'C', 0, b'a', 0, b'm', 0, 0, 0];
        expected.extend_from_slice(b"RDCamera_Device_0\0");
        assert_eq!(added, [expected]);

        // Windows opens a control channel and a sample channel to the same camera.
        let listener = &mut channels.devices[0];
        assert_eq!(listener.channel_name(), "RDCamera_Device_0");
        let mut control = listener.create(8).unwrap();
        let mut camera = listener.create(9).unwrap();
        control.start(8).unwrap();
        camera.start(9).unwrap();

        let list = encoded(&control.process(8, &[1, MSG_MEDIA_TYPE_LIST_REQUEST, 0]).unwrap());
        assert_eq!(list[0].len(), 2 + MEDIA_TYPE_SIZE);
        assert_eq!(VideoMode::decode(&list[0][2..]), Some(MJPG_720));
        assert_eq!(list[0][2 + 25], DECODING_REQUIRED);

        let mut start = vec![1, MSG_START_STREAMS, 0];
        start.extend_from_slice(&list[0][2..]);
        assert_eq!(encoded(&control.process(8, &start).unwrap()), [vec![1, MSG_SUCCESS]]);
        assert_eq!(*fake.started.lock().unwrap(), Some(MJPG_720));

        let push = |frame: &[u8]| (fake.sink.lock().unwrap().as_mut().unwrap())(frame);
        // A request before any frame is answered by the next frame, on the requesting channel.
        assert!(camera.process(9, &[1, MSG_SAMPLE_REQUEST, 0]).unwrap().is_empty());
        push(b"one");
        assert_eq!(
            samples.receiver.try_recv().unwrap(),
            (9, vec![1, MSG_SAMPLE_RESPONSE, 0, b'o', b'n', b'e'])
        );
        // Without a request frames wait, and only the newest is kept.
        push(b"two");
        push(b"three");
        assert!(samples.receiver.try_recv().is_err());
        let answer = encoded(&camera.process(9, &[1, MSG_SAMPLE_REQUEST, 0]).unwrap());
        assert_eq!(answer, [[&[1, MSG_SAMPLE_RESPONSE, 0][..], b"three"].concat()]);

        // Closing one channel keeps the camera on; closing the last turns it off.
        control.close(8);
        push(b"four");
        assert_eq!(camera.process(9, &[1, MSG_SAMPLE_REQUEST, 0]).unwrap().len(), 1);
        camera.close(9);
        push(b"late");
        assert!(samples.receiver.try_recv().is_err());
    }

    #[test]
    fn rejects_modes_it_did_not_offer() {
        let backend: CameraBackend = Arc::new(FakeCameras::default());
        let (mut channels, _samples) = channels(&backend, vec![camera()]);
        let mut camera = channels.devices[0].create(1).unwrap();
        let mut start = vec![1, MSG_START_STREAMS, 0];
        VideoMode {
            width: 640,
            height: 480,
            ..MJPG_720
        }
        .encode(&mut start);
        let reply = encoded(&camera.process(1, &start).unwrap());
        assert_eq!(
            reply,
            [[&[1, MSG_ERROR][..], &ERROR_INVALID_MEDIA_TYPE.to_le_bytes()].concat()]
        );
    }
}
