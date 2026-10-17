//! Local cameras through PipeWire, for webcam redirection.
//!
//! Cameras are PipeWire `Video/Source` nodes; their modes come from the formats each node can
//! produce. Capture runs a PipeWire stream on its own thread in the mode the PC picked, handing
//! each frame over as is: JPEG data for MJPEG, tightly packed rows for raw formats.

use std::cell::RefCell;
use std::collections::HashSet;
use std::os::fd::OwnedFd;
use std::rc::Rc;
use std::sync::Mutex;
use std::thread::JoinHandle;

use pipewire as pw;
use porthole_core::{CameraDevice, Cameras, FrameSink, VideoFormat, VideoMode};
use pw::spa;
use pw::spa::pod::{ChoiceValue, Object, Property, Value};
use pw::spa::utils::{ChoiceEnum, Fraction, Id, Rectangle};
use tracing::{debug, warn};

/// Frame rates above this aren't offered (the PC gets one frame per request anyway).
const MAX_FPS: f64 = 30.0;
/// Larger frames aren't offered.
const MAX_SIZE: (u32, u32) = (1920, 1080);
/// Offered for cameras that report a size range instead of fixed sizes.
const COMMON_SIZES: [(u32, u32); 4] = [(1920, 1080), (1280, 720), (640, 480), (320, 240)];

/// The cameras PipeWire knows about. `PORTHOLE_CAMERA_TARGET` (a node name) limits them to one.
///
/// In a sandbox (Flatpak) cameras are only reachable through the camera portal: listing them
/// asks the user for access, and the PipeWire connection the portal hands out is used from then
/// on.
#[derive(Debug, Default)]
pub struct PipeWireCameras {
    remote: Mutex<Option<OwnedFd>>,
}

impl PipeWireCameras {
    /// A connection to the cameras' PipeWire: `None` for the default one, outside a sandbox.
    /// May ask the portal, so only from a blocking context (listing runs in one).
    fn remote(&self) -> Result<Option<OwnedFd>, String> {
        if !ashpd::is_sandboxed() {
            return Ok(None);
        }
        let mut remote = self.remote.lock().unwrap_or_else(|e| e.into_inner());
        if remote.is_none() {
            let runtime = tokio::runtime::Handle::try_current().map_err(|e| e.to_string())?;
            *remote = Some(runtime.block_on(camera_portal())?);
        }
        let fd = remote.as_ref().map(OwnedFd::try_clone).transpose();
        fd.map_err(|e| e.to_string())
    }
}

/// Asks for camera access and opens the portal's PipeWire connection.
async fn camera_portal() -> Result<OwnedFd, String> {
    let camera = ashpd::desktop::camera::Camera::new()
        .await
        .map_err(|e| format!("no camera portal: {e}"))?;
    camera
        .request_access(Default::default())
        .await
        .and_then(|request| request.response())
        .map_err(|e| format!("camera access wasn't granted: {e}"))?;
    camera
        .open_pipe_wire_remote(Default::default())
        .await
        .map_err(|e| format!("the camera portal didn't open PipeWire: {e}"))
}

impl Cameras for PipeWireCameras {
    fn devices(&self) -> Vec<CameraDevice> {
        let only = std::env::var("PORTHOLE_CAMERA_TARGET").ok();
        let devices = self
            .remote()
            .and_then(|remote| enumerate(only.as_deref(), remote).map_err(|e| e.to_string()));
        match devices {
            Ok(devices) => devices,
            Err(e) => {
                warn!(error = %e, "Couldn't list the cameras");
                Vec::new()
            }
        }
    }

    fn start(&self, device: &str, mode: VideoMode, sink: FrameSink) -> Result<Box<dyn Send>, String> {
        // Called from the session task: reuse the portal connection made while listing.
        let remote = if ashpd::is_sandboxed() {
            let remote = self.remote.lock().unwrap_or_else(|e| e.into_inner());
            let fd = remote.as_ref().ok_or("camera access wasn't granted")?;
            Some(fd.try_clone().map_err(|e| e.to_string())?)
        } else {
            None
        };
        Capture::start(device.to_owned(), mode, sink, remote)
            .map(|capture| Box::new(capture) as Box<dyn Send>)
            .map_err(|e| e.to_string())
    }
}

/// Connects to `remote`, or to the default PipeWire.
fn connect(context: &pw::context::ContextRc, remote: Option<OwnedFd>) -> Result<pw::core::CoreRc, pw::Error> {
    match remote {
        Some(fd) => context.connect_fd_rc(fd, None),
        None => context.connect_rc(None),
    }
}

/// A camera node found while listing.
struct Found {
    name: String,
    description: String,
    modes: Vec<VideoMode>,
}

fn enumerate(only: Option<&str>, remote: Option<OwnedFd>) -> Result<Vec<CameraDevice>, pw::Error> {
    pw::init();
    let mainloop = pw::main_loop::MainLoopRc::new(None)?;
    let context = pw::context::ContextRc::new(&mainloop, None)?;
    let core = connect(&context, remote)?;
    let registry = core.get_registry_rc()?;

    let found: Rc<RefCell<Vec<Found>>> = Rc::default();
    // Node proxies and their listeners must live until the formats have arrived.
    let bound: Rc<RefCell<Vec<(pw::node::Node, pw::node::NodeListener)>>> = Rc::default();
    let _registry_listener = registry
        .add_listener_local()
        .global({
            let registry = registry.downgrade();
            let found = Rc::clone(&found);
            let bound = Rc::clone(&bound);
            let only = only.map(str::to_owned);
            move |global| {
                let Some(props) = global.props else { return };
                if global.type_ != pw::types::ObjectType::Node || props.get("media.class") != Some("Video/Source") {
                    return;
                }
                let name = props.get("node.name").unwrap_or_default().to_owned();
                if only.as_deref().is_some_and(|only| only != name) {
                    return;
                }
                let description = props
                    .get("node.description")
                    .or_else(|| props.get("node.nick"))
                    .unwrap_or(&name)
                    .to_owned();
                let Some(registry) = registry.upgrade() else { return };
                let Ok(node) = registry.bind::<pw::node::Node, _>(global) else {
                    return;
                };
                let index = {
                    let mut found = found.borrow_mut();
                    found.push(Found {
                        name,
                        description,
                        modes: Vec::new(),
                    });
                    found.len() - 1
                };
                let listener = node
                    .add_listener_local()
                    .param({
                        let found = Rc::clone(&found);
                        move |_, _, _, _, param| {
                            if let Some(param) = param {
                                found.borrow_mut()[index].modes.extend(modes(param.as_bytes()));
                            }
                        }
                    })
                    .register();
                node.enum_params(0, Some(spa::param::ParamType::EnumFormat), 0, u32::MAX);
                bound.borrow_mut().push((node, listener));
            }
        })
        .register();

    // Two round trips: the first lists the nodes (and asks for their formats), the second
    // collects the formats.
    let round_trips = Rc::new(RefCell::new(0));
    let pending = Rc::new(RefCell::new(core.sync(0)?));
    let _core_listener = core
        .add_listener_local()
        .done({
            let core = core.clone();
            let mainloop = mainloop.clone();
            let pending = Rc::clone(&pending);
            let round_trips = Rc::clone(&round_trips);
            move |id, seq| {
                if id != pw::core::PW_ID_CORE || seq != *pending.borrow() {
                    return;
                }
                *round_trips.borrow_mut() += 1;
                match core.sync(0) {
                    Ok(next) if *round_trips.borrow() < 2 => *pending.borrow_mut() = next,
                    _ => mainloop.quit(),
                }
            }
        })
        .register();
    mainloop.run();
    drop(bound);

    let found = found.take();
    let mut names: HashSet<String> = HashSet::new();
    Ok(found
        .into_iter()
        .filter_map(|camera| {
            let modes = preferred(camera.modes);
            if modes.is_empty() {
                debug!(camera = %camera.description, "Camera has no usable formats");
                return None;
            }
            // Two cameras of the same model need telling apart on the PC.
            let mut name = camera.description.clone();
            let mut n = 1;
            while !names.insert(name.clone()) {
                n += 1;
                name = format!("{} ({n})", camera.description);
            }
            debug!(camera = %name, node = %camera.name, modes = modes.len(), "Camera");
            Some(CameraDevice {
                id: camera.name,
                name,
                modes,
            })
        })
        .collect())
}

/// The modes one EnumFormat param describes.
fn modes(pod: &[u8]) -> Vec<VideoMode> {
    let Ok((_, Value::Object(object))) = spa::pod::deserialize::PodDeserializer::deserialize_any_from(pod) else {
        return Vec::new();
    };
    let property = |key: u32| object.properties.iter().find(|p| p.key == key).map(|p| &p.value);
    if property(spa::sys::SPA_FORMAT_mediaType) != Some(&Value::Id(Id(spa::sys::SPA_MEDIA_TYPE_video))) {
        return Vec::new();
    }
    let formats: Vec<VideoFormat> = match property(spa::sys::SPA_FORMAT_mediaSubtype) {
        Some(Value::Id(Id(spa::sys::SPA_MEDIA_SUBTYPE_mjpg))) => vec![VideoFormat::Mjpg],
        Some(Value::Id(Id(spa::sys::SPA_MEDIA_SUBTYPE_raw))) => ids(property(spa::sys::SPA_FORMAT_VIDEO_format))
            .into_iter()
            .filter_map(raw_format)
            .collect(),
        _ => Vec::new(),
    };
    let sizes = sizes(property(spa::sys::SPA_FORMAT_VIDEO_size));
    let Some(rate) = best_rate(property(spa::sys::SPA_FORMAT_VIDEO_framerate)) else {
        return Vec::new();
    };
    formats
        .iter()
        .flat_map(|&format| {
            sizes.iter().map(move |&(width, height)| VideoMode {
                format,
                width,
                height,
                fps_num: rate.num,
                fps_den: rate.denom,
            })
        })
        .collect()
}

fn raw_format(id: u32) -> Option<VideoFormat> {
    match id {
        spa::sys::SPA_VIDEO_FORMAT_YUY2 => Some(VideoFormat::Yuy2),
        spa::sys::SPA_VIDEO_FORMAT_NV12 => Some(VideoFormat::Nv12),
        _ => None,
    }
}

fn raw_id(format: VideoFormat) -> Option<u32> {
    match format {
        VideoFormat::Yuy2 => Some(spa::sys::SPA_VIDEO_FORMAT_YUY2),
        VideoFormat::Nv12 => Some(spa::sys::SPA_VIDEO_FORMAT_NV12),
        _ => None,
    }
}

fn ids(value: Option<&Value>) -> Vec<u32> {
    match value {
        Some(Value::Id(Id(id))) => vec![*id],
        Some(Value::Choice(ChoiceValue::Id(choice))) => match &choice.1 {
            ChoiceEnum::None(Id(id)) => vec![*id],
            ChoiceEnum::Enum { default, alternatives } => {
                std::iter::once(default).chain(alternatives).map(|id| id.0).collect()
            }
            _ => Vec::new(),
        },
        _ => Vec::new(),
    }
}

fn sizes(value: Option<&Value>) -> Vec<(u32, u32)> {
    let all: Vec<(u32, u32)> = match value {
        Some(Value::Rectangle(r)) => vec![(r.width, r.height)],
        Some(Value::Choice(ChoiceValue::Rectangle(choice))) => match &choice.1 {
            ChoiceEnum::None(r) => vec![(r.width, r.height)],
            ChoiceEnum::Enum { default, alternatives } => std::iter::once(default)
                .chain(alternatives)
                .map(|r| (r.width, r.height))
                .collect(),
            ChoiceEnum::Range { min, max, .. } | ChoiceEnum::Step { min, max, .. } => COMMON_SIZES
                .into_iter()
                .filter(|&(w, h)| (min.width..=max.width).contains(&w) && (min.height..=max.height).contains(&h))
                .collect(),
            ChoiceEnum::Flags { .. } => Vec::new(),
        },
        _ => Vec::new(),
    };
    all.into_iter()
        .filter(|&(w, h)| w > 0 && h > 0 && w <= MAX_SIZE.0 && h <= MAX_SIZE.1)
        .collect()
}

/// The fastest rate up to [`MAX_FPS`] (or the slowest offered if all are faster).
fn best_rate(value: Option<&Value>) -> Option<Fraction> {
    let fps = |f: &Fraction| f64::from(f.num) / f64::from(f.denom.max(1));
    let rates: Vec<Fraction> = match value {
        Some(Value::Fraction(f)) => vec![*f],
        Some(Value::Choice(ChoiceValue::Fraction(choice))) => match &choice.1 {
            ChoiceEnum::None(f) => vec![*f],
            ChoiceEnum::Enum { default, alternatives } => {
                std::iter::once(default).chain(alternatives).copied().collect()
            }
            ChoiceEnum::Range { min, max, .. } | ChoiceEnum::Step { min, max, .. } => {
                if fps(max) <= MAX_FPS {
                    vec![*max]
                } else if fps(min) <= MAX_FPS {
                    vec![Fraction { num: 30, denom: 1 }]
                } else {
                    vec![*min]
                }
            }
            ChoiceEnum::Flags { .. } => Vec::new(),
        },
        // No rate given: assume the usual.
        None => vec![Fraction { num: 30, denom: 1 }],
        _ => Vec::new(),
    };
    let rates: Vec<Fraction> = rates.into_iter().filter(|f| f.num > 0 && f.denom > 0).collect();
    rates
        .iter()
        .filter(|f| fps(f) <= MAX_FPS + 0.01)
        .max_by(|a, b| fps(a).total_cmp(&fps(b)))
        .or_else(|| rates.iter().min_by(|a, b| fps(a).total_cmp(&fps(b))))
        .copied()
}

/// Unique modes, MJPEG first (far less data than raw frames), then larger sizes first.
fn preferred(modes: Vec<VideoMode>) -> Vec<VideoMode> {
    let mut seen = HashSet::new();
    let mut modes: Vec<VideoMode> = modes
        .into_iter()
        .filter(|m| seen.insert((m.format, m.width, m.height)))
        .collect();
    let rank = |f: VideoFormat| match f {
        VideoFormat::Mjpg => 0,
        VideoFormat::Nv12 => 1,
        _ => 2,
    };
    modes.sort_by_key(|m| (rank(m.format), std::cmp::Reverse(m.width * m.height)));
    modes
}

/// Captures from one camera on its own thread until dropped.
struct Capture {
    quit: pw::channel::Sender<()>,
    thread: Option<JoinHandle<()>>,
}

impl Capture {
    fn start(target: String, mode: VideoMode, sink: FrameSink, remote: Option<OwnedFd>) -> Result<Self, pw::Error> {
        let (quit, quit_rx) = pw::channel::channel::<()>();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let thread = std::thread::Builder::new()
            .name("porthole-camera".to_owned())
            .spawn(move || {
                if let Err(e) = run(&target, mode, sink, remote, quit_rx, &ready_tx) {
                    let _ = ready_tx.send(Err(e));
                }
            })
            .map_err(|_| pw::Error::CreationFailed)?;
        ready_rx.recv().map_err(|_| pw::Error::CreationFailed)??;
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

fn serialize(object: Object) -> Result<Vec<u8>, pw::Error> {
    Ok(
        spa::pod::serialize::PodSerializer::serialize(std::io::Cursor::new(Vec::new()), &Value::Object(object))
            .map_err(|_| pw::Error::CreationFailed)?
            .0
            .into_inner(),
    )
}

/// Bytes per row and number of rows of a tightly packed frame.
fn packed_rows(mode: VideoMode) -> Option<(usize, usize)> {
    let (width, height) = (mode.width as usize, mode.height as usize);
    match mode.format {
        VideoFormat::Yuy2 => Some((width * 2, height)),
        // Y rows, then half as many interleaved UV rows of the same length.
        VideoFormat::Nv12 => Some((width, height + height / 2)),
        _ => None,
    }
}

/// Whether a JPEG's chroma is subsampled vertically too (4:2:0). Webcams send 4:2:2, the only
/// kind Windows' MJPEG decoder shows correctly.
fn jpeg_is_420(jpeg: &[u8]) -> Option<bool> {
    let mut i = 2; // after SOI
    while i + 4 <= jpeg.len() {
        if jpeg[i] != 0xFF {
            return None;
        }
        let marker = jpeg[i + 1];
        let len = usize::from(u16::from_be_bytes([jpeg[i + 2], jpeg[i + 3]]));
        // SOF0..SOF2: precision, height, width, components, then id + sampling per component.
        if (0xC0..=0xC2).contains(&marker) {
            let luma_sampling = *jpeg.get(i + 4 + 7)?;
            return Some(luma_sampling & 0x0F == 2);
        }
        i += 2 + len;
    }
    None
}

/// Copies `rows` rows of `row_bytes` that are `stride` apart into one packed frame.
fn pack(data: &[u8], stride: usize, row_bytes: usize, rows: usize, out: &mut Vec<u8>) -> bool {
    out.clear();
    if stride < row_bytes || data.len() < stride * (rows - 1) + row_bytes {
        return false;
    }
    if stride == row_bytes {
        out.extend_from_slice(&data[..row_bytes * rows]);
    } else {
        for row in 0..rows {
            out.extend_from_slice(&data[row * stride..row * stride + row_bytes]);
        }
    }
    true
}

fn run(
    target: &str,
    mode: VideoMode,
    mut sink: FrameSink,
    remote: Option<OwnedFd>,
    quit: pw::channel::Receiver<()>,
    ready: &std::sync::mpsc::Sender<Result<(), pw::Error>>,
) -> Result<(), pw::Error> {
    pw::init();
    let mainloop = pw::main_loop::MainLoopRc::new(None)?;
    let context = pw::context::ContextRc::new(&mainloop, None)?;
    let core = connect(&context, remote)?;
    let mut properties = pw::properties::properties! {
        *pw::keys::MEDIA_TYPE => "Video",
        *pw::keys::MEDIA_CATEGORY => "Capture",
        *pw::keys::MEDIA_ROLE => "Camera",
        *pw::keys::APP_NAME => "Porthole",
    };
    properties.insert("target.object", target);
    let stream = pw::stream::StreamBox::new(&core, "Porthole camera", properties)?;

    let target_name = target.to_owned();
    let packed = packed_rows(mode);
    let mut frame = Vec::new();
    let mut checked = mode.format != VideoFormat::Mjpg;
    // Plain memory: cameras otherwise hand out DMA-BUFs, which aren't mapped for reading.
    let buffers = serialize(Object {
        type_: spa::sys::SPA_TYPE_OBJECT_ParamBuffers,
        id: spa::sys::SPA_PARAM_Buffers,
        properties: vec![Property::new(
            spa::sys::SPA_PARAM_BUFFERS_dataType,
            Value::Int((1 << spa::sys::SPA_DATA_MemPtr) | (1 << spa::sys::SPA_DATA_MemFd)),
        )],
    })?;
    let mut frames = 0u64;
    let mut unmapped = false;
    let _listener = stream
        .add_local_listener_with_user_data(())
        .state_changed({
            let target = target.to_owned();
            move |_, _, old, new| match new {
                pw::stream::StreamState::Error(e) => warn!(camera = %target, error = %e, "Camera stream failed"),
                new => debug!(camera = %target, ?old, ?new, "Camera stream"),
            }
        })
        .param_changed(move |stream, _, id, param| {
            if id == spa::sys::SPA_PARAM_Format
                && param.is_some()
                && let Some(buffers) = spa::pod::Pod::from_bytes(&buffers)
                && let Err(e) = stream.update_params(&mut [buffers])
            {
                warn!(error = %e, "Couldn't ask the camera for mappable buffers");
            }
        })
        .process(move |stream, _| {
            let Some(mut buffer) = stream.dequeue_buffer() else {
                return;
            };
            let datas = buffer.datas_mut();
            let Some(data) = datas.first_mut() else { return };
            if data.data().is_none() {
                if !unmapped {
                    unmapped = true;
                    warn!(kind = ?data.type_(), "Camera frames aren't readable");
                }
                return;
            }
            frames += 1;
            if frames == 1 {
                debug!("First camera frame");
            }
            let (offset, size, stride) = {
                let chunk = data.chunk();
                (
                    chunk.offset() as usize,
                    chunk.size() as usize,
                    chunk.stride().max(0) as usize,
                )
            };
            let Some(bytes) = data.data().and_then(|d| d.get(offset..offset + size)) else {
                return;
            };
            match packed {
                None => {
                    if !checked {
                        checked = true;
                        if jpeg_is_420(bytes) == Some(true) {
                            warn!(
                                camera = %target_name,
                                "The camera sends 4:2:0 JPEG, which Windows shows with wrong colours"
                            );
                        }
                    }
                    sink(bytes);
                }
                Some((row_bytes, rows)) => {
                    let stride = if stride == 0 { row_bytes } else { stride };
                    if pack(bytes, stride, row_bytes, rows, &mut frame) {
                        sink(&frame);
                    }
                }
            }
        })
        .register()?;

    let mut properties = vec![
        Property::new(
            spa::sys::SPA_FORMAT_mediaType,
            Value::Id(Id(spa::sys::SPA_MEDIA_TYPE_video)),
        ),
        Property::new(
            spa::sys::SPA_FORMAT_mediaSubtype,
            Value::Id(Id(if mode.format == VideoFormat::Mjpg {
                spa::sys::SPA_MEDIA_SUBTYPE_mjpg
            } else {
                spa::sys::SPA_MEDIA_SUBTYPE_raw
            })),
        ),
        Property::new(
            spa::sys::SPA_FORMAT_VIDEO_size,
            Value::Rectangle(Rectangle {
                width: mode.width,
                height: mode.height,
            }),
        ),
        Property::new(
            spa::sys::SPA_FORMAT_VIDEO_framerate,
            Value::Fraction(Fraction {
                num: mode.fps_num,
                denom: mode.fps_den,
            }),
        ),
    ];
    if let Some(format) = raw_id(mode.format) {
        properties.push(Property::new(spa::sys::SPA_FORMAT_VIDEO_format, Value::Id(Id(format))));
    }
    let format = serialize(Object {
        type_: spa::sys::SPA_TYPE_OBJECT_Format,
        id: spa::sys::SPA_PARAM_EnumFormat,
        properties,
    })?;
    let mut params = [spa::pod::Pod::from_bytes(&format).ok_or(pw::Error::CreationFailed)?];
    stream.connect(
        spa::utils::Direction::Input,
        None,
        pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
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
    use spa::pod::serialize::PodSerializer;
    use spa::utils::{Choice, ChoiceFlags};

    fn pod(properties: Vec<Property>) -> Vec<u8> {
        PodSerializer::serialize(
            std::io::Cursor::new(Vec::new()),
            &Value::Object(Object {
                type_: spa::sys::SPA_TYPE_OBJECT_Format,
                id: spa::sys::SPA_PARAM_EnumFormat,
                properties,
            }),
        )
        .unwrap()
        .0
        .into_inner()
    }

    #[test]
    fn reads_modes_from_enum_formats() {
        // As a V4L2 camera reports MJPEG at one size with several rates.
        let mjpg = pod(vec![
            Property::new(
                spa::sys::SPA_FORMAT_mediaType,
                Value::Id(Id(spa::sys::SPA_MEDIA_TYPE_video)),
            ),
            Property::new(
                spa::sys::SPA_FORMAT_mediaSubtype,
                Value::Id(Id(spa::sys::SPA_MEDIA_SUBTYPE_mjpg)),
            ),
            Property::new(
                spa::sys::SPA_FORMAT_VIDEO_size,
                Value::Rectangle(Rectangle {
                    width: 1280,
                    height: 720,
                }),
            ),
            Property::new(
                spa::sys::SPA_FORMAT_VIDEO_framerate,
                Value::Choice(ChoiceValue::Fraction(Choice(
                    ChoiceFlags::empty(),
                    ChoiceEnum::Enum {
                        default: Fraction { num: 60, denom: 1 },
                        alternatives: vec![
                            Fraction { num: 60, denom: 1 },
                            Fraction { num: 30, denom: 1 },
                            Fraction { num: 15, denom: 1 },
                        ],
                    },
                ))),
            ),
        ]);
        assert_eq!(
            modes(&mjpg),
            [VideoMode {
                format: VideoFormat::Mjpg,
                width: 1280,
                height: 720,
                fps_num: 30,
                fps_den: 1
            }]
        );

        // Raw with a format choice: only the formats the protocol carries are kept.
        let raw = pod(vec![
            Property::new(
                spa::sys::SPA_FORMAT_mediaType,
                Value::Id(Id(spa::sys::SPA_MEDIA_TYPE_video)),
            ),
            Property::new(
                spa::sys::SPA_FORMAT_mediaSubtype,
                Value::Id(Id(spa::sys::SPA_MEDIA_SUBTYPE_raw)),
            ),
            Property::new(
                spa::sys::SPA_FORMAT_VIDEO_format,
                Value::Choice(ChoiceValue::Id(Choice(
                    ChoiceFlags::empty(),
                    ChoiceEnum::Enum {
                        default: Id(spa::sys::SPA_VIDEO_FORMAT_YUY2),
                        alternatives: vec![Id(spa::sys::SPA_VIDEO_FORMAT_YUY2), Id(spa::sys::SPA_VIDEO_FORMAT_I420)],
                    },
                ))),
            ),
            Property::new(
                spa::sys::SPA_FORMAT_VIDEO_size,
                Value::Rectangle(Rectangle {
                    width: 640,
                    height: 480,
                }),
            ),
            Property::new(
                spa::sys::SPA_FORMAT_VIDEO_framerate,
                Value::Fraction(Fraction { num: 30, denom: 1 }),
            ),
        ]);
        let found = modes(&raw);
        assert_eq!(
            found.len(),
            2,
            "YUY2 twice (default and alternative), no I420: {found:?}"
        );
        assert!(found.iter().all(|m| m.format == VideoFormat::Yuy2 && m.width == 640));

        let ordered = preferred([found, modes(&mjpg)].concat());
        assert_eq!(ordered.len(), 2);
        assert_eq!(ordered[0].format, VideoFormat::Mjpg);
    }

    #[test]
    fn tells_420_jpeg_apart() {
        // SOI, then SOF0 for 16x16 with three components; luma sampling 0x22 (4:2:0) or 0x21 (4:2:2).
        let jpeg = |luma: u8| {
            vec![
                0xFF, 0xD8, 0xFF, 0xC0, 0, 17, 8, 0, 16, 0, 16, 3, 1, luma, 0, 2, 0x11, 1, 3, 0x11, 1,
            ]
        };
        assert_eq!(jpeg_is_420(&jpeg(0x22)), Some(true));
        assert_eq!(jpeg_is_420(&jpeg(0x21)), Some(false));
        assert_eq!(jpeg_is_420(&[0xFF, 0xD8]), None);
    }

    #[test]
    fn packs_strided_rows() {
        let mut out = Vec::new();
        // Two rows of 3 bytes, 4 apart.
        assert!(pack(&[1, 2, 3, 0, 4, 5, 6], 4, 3, 2, &mut out));
        assert_eq!(out, [1, 2, 3, 4, 5, 6]);
        assert!(!pack(&[1, 2, 3], 4, 3, 2, &mut out), "too short");
    }
}
