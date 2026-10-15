//! H.264 decoding with OpenH264, loaded at run time.
//!
//! OpenH264 isn't linked in: H.264 needs a patent licence, which Cisco's binaries carry and
//! distributions pass on through their own packages (or the Flatpak OpenH264 extension).

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use ironrdp_egfx::decode::{DecodedFrame, DecoderError, DecoderResult, H264Decoder};
use openh264::OpenH264API;
use openh264::decoder::{Decoder, DecoderConfig};
use openh264::formats::YUVSource as _;
use tracing::{info, trace, warn};

/// The OpenH264 version the bindings are written against.
const ABI_VERSION_PREFIX: &str = "libopenh264.so.2.6.";

const SEARCH_DIRS: &[&str] = &[
    "/usr/lib/x86_64-linux-gnu",
    "/usr/lib64",
    "/usr/lib",
    "/app/lib/openh264/extra", // Flatpak OpenH264 extension
    "/usr/lib/extensions/openh264/extra",
];
const NAMES: &[&str] = &["libopenh264.so.8", "libopenh264.so.7", "libopenh264.so"];

/// One H.264 stream decoder shared by IronRDP's AVC420 path and our AVC444 one: both carry the
/// same encoder's pictures, so they must go through the same decoder.
#[derive(Clone)]
pub(crate) struct SharedDecoder(Arc<Mutex<Box<dyn H264Decoder>>>);

impl SharedDecoder {
    pub(crate) fn new(decoder: Box<dyn H264Decoder>) -> Self {
        Self(Arc::new(Mutex::new(decoder)))
    }
}

impl H264Decoder for SharedDecoder {
    fn decode(&mut self, data: &[u8]) -> DecoderResult<DecodedFrame> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).decode(data)
    }

    fn reset(&mut self) {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).reset();
    }
}

/// Loads OpenH264 if it's installed. `None`: the session falls back to other codecs.
pub(crate) fn decoder() -> Option<Box<dyn H264Decoder>> {
    for path in SEARCH_DIRS
        .iter()
        .flat_map(|dir| NAMES.iter().map(move |name| Path::new(dir).join(name)))
        .filter(|path| path.exists())
    {
        match load(&path) {
            Ok(api) => match Decoder::with_api_config(api, DecoderConfig::default()) {
                Ok(decoder) => {
                    info!(library = %path.display(), "H.264 decoding available");
                    return Some(Box::new(OpenH264 {
                        decoder,
                        annex_b: Vec::new(),
                    }));
                }
                Err(e) => warn!(library = %path.display(), error = %e, "OpenH264 decoder failed to start"),
            },
            Err(reason) => trace!(library = %path.display(), %reason, "OpenH264 not loaded"),
        }
    }
    info!("OpenH264 not found; H.264 is off");
    None
}

fn load(path: &Path) -> Result<OpenH264API, String> {
    // Cisco's own binaries are recognised by hash.
    if let Ok(api) = OpenH264API::from_blob_path(path) {
        return Ok(api);
    }
    // Distribution builds have other hashes; accept them only at the exact version the bindings
    // match, so the function signatures are the ones we call.
    let real: PathBuf = std::fs::canonicalize(path).map_err(|e| e.to_string())?;
    let name = real.file_name().and_then(|n| n.to_str()).unwrap_or_default();
    if !name.starts_with(ABI_VERSION_PREFIX) {
        return Err(format!("{name} is not OpenH264 2.6"));
    }
    load_unchecked(&real)
}

#[allow(unsafe_code)]
fn load_unchecked(path: &Path) -> Result<OpenH264API, String> {
    // SAFETY: the library is OpenH264 2.6.x (checked by its file name above), the ABI these
    // bindings are generated for, so the symbols resolve to functions with matching signatures.
    unsafe { OpenH264API::from_blob_path_unchecked(path) }.map_err(|e| e.to_string())
}

struct OpenH264 {
    decoder: Decoder,
    annex_b: Vec<u8>,
}

impl H264Decoder for OpenH264 {
    fn decode(&mut self, data: &[u8]) -> DecoderResult<DecodedFrame> {
        // Windows sends Annex B (start codes); length-prefixed AVC is converted.
        let annex_b = if data.starts_with(&[0, 0, 0, 1]) || data.starts_with(&[0, 0, 1]) {
            data
        } else {
            to_annex_b(data, &mut self.annex_b);
            &self.annex_b
        };
        let yuv = self
            .decoder
            .decode(annex_b)
            .map_err(|e| DecoderError::new("OpenH264 decode failed", e))?
            .ok_or_else(|| DecoderError::msg("OpenH264 returned no picture"))?;
        let (width, height) = yuv.dimensions();
        let mut rgba = vec![0; width * height * 4];
        yuv.write_rgba8(&mut rgba);
        Ok(DecodedFrame::new(rgba, width as u32, height as u32))
    }
}

/// AVC NAL units with 4-byte big-endian lengths → Annex B start codes, which OpenH264 wants.
fn to_annex_b(data: &[u8], out: &mut Vec<u8>) {
    out.clear();
    let mut rest = data;
    while let Some((len, tail)) = rest.split_first_chunk::<4>() {
        let len = u32::from_be_bytes(*len) as usize;
        let Some(nal) = tail.get(..len) else { break };
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(nal);
        rest = &tail[len..];
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_length_prefixed_nals() {
        let mut out = Vec::new();
        to_annex_b(&[0, 0, 0, 2, 0xAA, 0xBB, 0, 0, 0, 1, 0xCC, 0, 0, 0, 9, 1], &mut out);
        assert_eq!(out, [0, 0, 0, 1, 0xAA, 0xBB, 0, 0, 0, 1, 0xCC]);
    }
}
