//! Microphone: the AUDIO_INPUT dynamic channel (MS-RDPEAI).
//!
//! The server announces formats, then opens the microphone in one of the formats we accepted.
//! Captured 16-bit PCM is cut into packets of the requested size and handed to the session loop,
//! which sends each as an "incoming data" + "data" message pair.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use ironrdp::core::{Encode, EncodeResult, WriteCursor, impl_as_any};
use ironrdp::dvc::{DvcClientProcessor, DvcEncode, DvcMessage, DvcProcessor};
use ironrdp::pdu::{PduResult, pdu_other_err};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

pub(crate) const CHANNEL_NAME: &str = "AUDIO_INPUT";

const MSG_VERSION: u8 = 0x01;
const MSG_FORMATS: u8 = 0x02;
const MSG_OPEN: u8 = 0x03;
const MSG_OPEN_REPLY: u8 = 0x04;
const MSG_DATA_INCOMING: u8 = 0x05;
const MSG_DATA: u8 = 0x06;
const MSG_FORMAT_CHANGE: u8 = 0x07;
const WAVE_FORMAT_PCM: u16 = 0x0001;
const E_FAIL: u32 = 0x8000_4005;

/// What the server records in: 16-bit little-endian PCM.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CaptureFormat {
    pub sample_rate: u32,
    pub channels: u16,
}

/// Captures from the microphone. Called with the format and a sink for captured PCM; recording
/// stops when the returned value is dropped.
pub type Microphone =
    Arc<dyn Fn(CaptureFormat, Box<dyn FnMut(&[u8]) + Send>) -> Result<Box<dyn Send>, String> + Send + Sync>;

/// One AUDIO_FORMAT as sent on the wire, kept so we can echo it back.
#[derive(Debug, Clone)]
struct WireFormat {
    raw: Vec<u8>,
    format: Option<CaptureFormat>,
}

/// A message we send, already encoded.
struct Raw(Vec<u8>);

impl Encode for Raw {
    fn encode(&self, dst: &mut WriteCursor<'_>) -> EncodeResult<()> {
        dst.write_slice(&self.0);
        Ok(())
    }

    fn name(&self) -> &'static str {
        "AUDIN"
    }

    fn size(&self) -> usize {
        self.0.len()
    }
}

impl DvcEncode for Raw {}

/// Captured packets for the session loop to send, with the channel to send them on.
pub(crate) struct Packets {
    pub channel_id: Arc<AtomicU32>,
    pub receiver: mpsc::UnboundedReceiver<Vec<u8>>,
}

impl Packets {
    /// The two DVC messages carrying one packet.
    pub(crate) fn messages(packet: Vec<u8>) -> Vec<DvcMessage> {
        let mut data = Vec::with_capacity(packet.len() + 1);
        data.push(MSG_DATA);
        data.extend_from_slice(&packet);
        vec![Box::new(Raw(vec![MSG_DATA_INCOMING])), Box::new(Raw(data))]
    }
}

pub(crate) struct AudioInput {
    microphone: Microphone,
    packets: mpsc::UnboundedSender<Vec<u8>>,
    channel_id: Arc<AtomicU32>,
    /// The formats we accepted, in the order we sent them (the server refers to them by index).
    accepted: Vec<WireFormat>,
    frames_per_packet: u32,
    recording: Option<Box<dyn Send>>,
}

impl_as_any!(AudioInput);

impl AudioInput {
    pub(crate) fn new(microphone: Microphone) -> (Self, Packets) {
        let (sender, receiver) = mpsc::unbounded_channel();
        let channel_id = Arc::new(AtomicU32::new(0));
        let input = Self {
            microphone,
            packets: sender,
            channel_id: Arc::clone(&channel_id),
            accepted: Vec::new(),
            frames_per_packet: 0,
            recording: None,
        };
        (input, Packets { channel_id, receiver })
    }

    /// Starts recording in the accepted format at `index`. Returns whether it worked.
    fn record(&mut self, index: u32) -> bool {
        self.recording = None;
        let Some(format) = self.accepted.get(index as usize).and_then(|f| f.format) else {
            warn!(index, "Server asked for a format we didn't accept");
            return false;
        };
        let packet_bytes = (self.frames_per_packet.max(1) as usize) * usize::from(format.channels) * 2;
        let packets = self.packets.clone();
        let pending = Arc::new(Mutex::new(Vec::with_capacity(packet_bytes * 2)));
        let sink = Box::new(move |pcm: &[u8]| {
            let mut pending = pending.lock().unwrap_or_else(|e| e.into_inner());
            pending.extend_from_slice(pcm);
            while pending.len() >= packet_bytes {
                let packet: Vec<u8> = pending.drain(..packet_bytes).collect();
                let _ = packets.send(packet);
            }
        });
        match (self.microphone)(format, sink) {
            Ok(recording) => {
                info!(?format, frames_per_packet = self.frames_per_packet, "Microphone on");
                self.recording = Some(recording);
                true
            }
            Err(e) => {
                warn!(error = %e, "Couldn't open the microphone");
                false
            }
        }
    }

    fn accept_formats(&mut self, payload: &[u8]) -> PduResult<Vec<DvcMessage>> {
        let count = read_u32(payload, 1)?;
        let mut offset = 9; // MessageId, NumFormats, cbSizeFormatsPacket
        self.accepted.clear();
        for _ in 0..count {
            let header = payload
                .get(offset..offset + 18)
                .ok_or_else(|| pdu_other_err!("AUDIN formats truncated"))?;
            let extra = usize::from(u16::from_le_bytes([header[16], header[17]]));
            let raw = payload
                .get(offset..offset + 18 + extra)
                .ok_or_else(|| pdu_other_err!("AUDIN format truncated"))?
                .to_vec();
            offset += 18 + extra;
            let tag = u16::from_le_bytes([raw[0], raw[1]]);
            let channels = u16::from_le_bytes([raw[2], raw[3]]);
            let sample_rate = u32::from_le_bytes([raw[4], raw[5], raw[6], raw[7]]);
            let bits = u16::from_le_bytes([raw[14], raw[15]]);
            if tag == WAVE_FORMAT_PCM
                && bits == 16
                && (1..=2).contains(&channels)
                && (8000..=48000).contains(&sample_rate)
            {
                self.accepted.push(WireFormat {
                    raw,
                    format: Some(CaptureFormat { sample_rate, channels }),
                });
            }
        }
        debug!(offered = count, accepted = self.accepted.len(), "Microphone formats");
        let mut reply = vec![MSG_FORMATS];
        reply.extend_from_slice(&(self.accepted.len() as u32).to_le_bytes());
        reply.extend_from_slice(&0u32.to_le_bytes()); // cbSizeFormatsPacket, set below
        for format in &self.accepted {
            reply.extend_from_slice(&format.raw);
        }
        let size = (reply.len() as u32).to_le_bytes();
        reply[5..9].copy_from_slice(&size);
        Ok(vec![Box::new(Raw(reply))])
    }
}

fn read_u32(payload: &[u8], offset: usize) -> PduResult<u32> {
    payload
        .get(offset..offset + 4)
        .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .ok_or_else(|| pdu_other_err!("AUDIN message truncated"))
}

fn u32_message(id: u8, value: u32) -> DvcMessage {
    let mut message = vec![id];
    message.extend_from_slice(&value.to_le_bytes());
    Box::new(Raw(message))
}

impl DvcProcessor for AudioInput {
    fn channel_name(&self) -> &str {
        CHANNEL_NAME
    }

    fn start(&mut self, channel_id: u32) -> PduResult<Vec<DvcMessage>> {
        self.channel_id.store(channel_id, Ordering::Relaxed);
        Ok(Vec::new())
    }

    fn process(&mut self, _channel_id: u32, payload: &[u8]) -> PduResult<Vec<DvcMessage>> {
        match payload.first().copied() {
            Some(MSG_VERSION) => Ok(vec![u32_message(MSG_VERSION, 1)]),
            Some(MSG_FORMATS) => self.accept_formats(payload),
            Some(MSG_OPEN) => {
                self.frames_per_packet = read_u32(payload, 1)?;
                let index = read_u32(payload, 5)?;
                let ok = self.record(index);
                let mut replies = Vec::new();
                if ok {
                    replies.push(u32_message(MSG_FORMAT_CHANGE, index));
                }
                replies.push(u32_message(MSG_OPEN_REPLY, if ok { 0 } else { E_FAIL }));
                Ok(replies)
            }
            Some(MSG_FORMAT_CHANGE) => {
                let index = read_u32(payload, 1)?;
                self.record(index);
                Ok(vec![u32_message(MSG_FORMAT_CHANGE, index)])
            }
            other => {
                debug!(message = ?other, "Unexpected AUDIN message");
                Ok(Vec::new())
            }
        }
    }

    fn close(&mut self, _channel_id: u32) {
        if self.recording.take().is_some() {
            info!("Microphone off");
        }
    }
}

impl DvcClientProcessor for AudioInput {}

#[cfg(test)]
mod tests {
    use super::*;

    fn format(tag: u16, channels: u16, rate: u32, bits: u16) -> Vec<u8> {
        let block = channels * bits / 8;
        let mut f = Vec::new();
        f.extend_from_slice(&tag.to_le_bytes());
        f.extend_from_slice(&channels.to_le_bytes());
        f.extend_from_slice(&rate.to_le_bytes());
        f.extend_from_slice(&(rate * u32::from(block)).to_le_bytes());
        f.extend_from_slice(&block.to_le_bytes());
        f.extend_from_slice(&bits.to_le_bytes());
        f.extend_from_slice(&0u16.to_le_bytes());
        f
    }

    fn encoded(message: &DvcMessage) -> Vec<u8> {
        let mut buf = vec![0; message.size()];
        message.encode(&mut WriteCursor::new(&mut buf)).unwrap();
        buf
    }

    #[test]
    fn negotiates_and_records_in_packets() {
        let captured = Arc::new(Mutex::new(None));
        let opened = Arc::clone(&captured);
        let microphone: Microphone = Arc::new(move |format, mut sink| {
            *opened.lock().unwrap() = Some(format);
            sink(&[1; 10]); // 2.5 stereo frames: not a full packet yet
            sink(&[2; 10]);
            Ok(Box::new(()))
        });
        let (mut input, mut packets) = AudioInput::new(microphone);
        input.start(7).unwrap();
        assert_eq!(packets.channel_id.load(Ordering::Relaxed), 7);

        let version = input.process(7, &[MSG_VERSION, 1, 0, 0, 0]).unwrap();
        assert_eq!(encoded(&version[0]), [MSG_VERSION, 1, 0, 0, 0]);

        let mut formats = vec![MSG_FORMATS, 3, 0, 0, 0, 0, 0, 0, 0];
        formats.extend(format(0x55, 2, 44_100, 16)); // MP3: refused
        formats.extend(format(WAVE_FORMAT_PCM, 2, 44_100, 16));
        formats.extend(format(WAVE_FORMAT_PCM, 1, 22_050, 8)); // 8-bit: refused
        let reply = encoded(&input.process(7, &formats).unwrap()[0]);
        assert_eq!(&reply[..5], [MSG_FORMATS, 1, 0, 0, 0]);
        assert_eq!(reply.len(), 9 + 18);

        // Open: 4 frames per packet, accepted format 0.
        let replies = input.process(7, &[MSG_OPEN, 4, 0, 0, 0, 0, 0, 0, 0]).unwrap();
        assert_eq!(encoded(&replies[0]), [MSG_FORMAT_CHANGE, 0, 0, 0, 0]);
        assert_eq!(encoded(&replies[1]), [MSG_OPEN_REPLY, 0, 0, 0, 0]);
        assert_eq!(
            *captured.lock().unwrap(),
            Some(CaptureFormat {
                sample_rate: 44_100,
                channels: 2
            })
        );
        // 20 bytes captured, 16-byte packets (4 frames × 2 channels × 2 bytes).
        let packet = packets.receiver.try_recv().unwrap();
        assert_eq!(packet.len(), 16);
        assert!(packets.receiver.try_recv().is_err());
    }
}
