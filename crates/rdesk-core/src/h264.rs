//! H.264 decoding with OpenH264, loaded at run time.
//!
//! OpenH264 isn't linked in: H.264 needs a patent licence, which Cisco's binaries carry and
//! distributions pass on through their own packages (or the Flatpak OpenH264 extension).

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

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

/// A new decoder (one per H.264 stream), if OpenH264 is installed. `None`: the session falls
/// back to other codecs.
pub(crate) fn decoder() -> Option<Box<dyn H264Decoder>> {
    // The library is looked for (and its hash checked) once.
    static LIBRARY: OnceLock<Option<PathBuf>> = OnceLock::new();
    if let Some(Some(path)) = LIBRARY.get() {
        return load(path).ok().and_then(|api| start(path, api));
    }
    for path in SEARCH_DIRS
        .iter()
        .flat_map(|dir| NAMES.iter().map(move |name| Path::new(dir).join(name)))
        .filter(|path| path.exists())
    {
        match load(&path) {
            Ok(api) => {
                if let Some(decoder) = start(&path, api) {
                    info!(library = %path.display(), "H.264 decoding available");
                    let _ = LIBRARY.set(Some(path));
                    return Some(decoder);
                }
            }
            Err(reason) => trace!(library = %path.display(), %reason, "OpenH264 not loaded"),
        }
    }
    info!("OpenH264 not found; H.264 is off");
    let _ = LIBRARY.set(None);
    None
}

fn start(path: &Path, api: OpenH264API) -> Option<Box<dyn H264Decoder>> {
    match Decoder::with_api_config(api, DecoderConfig::default()) {
        Ok(decoder) => Some(Box::new(OpenH264 {
            decoder,
            annex_b: Vec::new(),
        })),
        Err(e) => {
            warn!(library = %path.display(), error = %e, "OpenH264 decoder failed to start");
            None
        }
    }
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
        let (y_stride, uv_stride, _) = yuv.strides();
        yuv709_full_to_rgba(
            Planes {
                y: yuv.y(),
                u: yuv.u(),
                v: yuv.v(),
                y_stride,
                uv_stride,
            },
            width,
            height,
            &mut rgba,
        );
        Ok(DecodedFrame::new(rgba, width as u32, height as u32))
    }
}

struct Planes<'a> {
    y: &'a [u8],
    u: &'a [u8],
    v: &'a [u8],
    y_stride: usize,
    uv_stride: usize,
}

/// BT.709 full-range YUV 4:2:0 → RGBA, as RDP's H.264 is encoded (MS-RDPEGFX). OpenH264's own
/// conversion assumes BT.601 limited range, which crushes dark greys to black and oversaturates.
fn yuv709_full_to_rgba(planes: Planes<'_>, width: usize, height: usize, rgba: &mut [u8]) {
    // Coefficients in 16.16 fixed point.
    const RV: i32 = 103_206; // 1.5748
    const GU: i32 = 12_276; // 0.1873
    const GV: i32 = 30_679; // 0.4681
    const BU: i32 = 121_609; // 1.8556
    const HALF: i32 = 1 << 15;
    let clamp = |v: i32| (v >> 16).clamp(0, 255) as u8;
    for (row, out) in rgba.chunks_exact_mut(width * 4).take(height).enumerate() {
        let y_row = &planes.y[row * planes.y_stride..][..width];
        let u_row = &planes.u[(row / 2) * planes.uv_stride..];
        let v_row = &planes.v[(row / 2) * planes.uv_stride..];
        for (x, (pixel, &luma)) in out.as_chunks_mut::<4>().0.iter_mut().zip(y_row).enumerate() {
            let y = (i32::from(luma) << 16) + HALF;
            let u = i32::from(u_row[x / 2]) - 128;
            let v = i32::from(v_row[x / 2]) - 128;
            pixel[0] = clamp(y + RV * v);
            pixel[1] = clamp(y - GU * u - GV * v);
            pixel[2] = clamp(y + BU * u);
            pixel[3] = 0xFF;
        }
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
    fn converts_full_range_bt709() {
        // Windows Terminal's #0C0C0C background and a light blue, as Windows encodes them.
        let rgb_to_yuv = |r: f64, g: f64, b: f64| {
            let y = 0.2126 * r + 0.7152 * g + 0.0722 * b;
            let u = (b - y) / 1.8556 + 128.0;
            let v = (r - y) / 1.5748 + 128.0;
            (y.round() as u8, u.round() as u8, v.round() as u8)
        };
        for (r, g, b) in [(12, 12, 12), (142, 179, 206), (0, 0, 0), (255, 255, 255), (200, 30, 40)] {
            let (y, u, v) = rgb_to_yuv(f64::from(r), f64::from(g), f64::from(b));
            let mut rgba = [0; 8];
            let planes = Planes {
                y: &[y, y],
                u: &[u],
                v: &[v],
                y_stride: 2,
                uv_stride: 1,
            };
            yuv709_full_to_rgba(planes, 2, 1, &mut rgba);
            for (got, want) in rgba[..3].iter().zip([r, g, b]) {
                assert!(got.abs_diff(want) <= 2, "{:?} for {:?}", &rgba[..3], (r, g, b));
            }
            assert_eq!(rgba[3], 0xFF);
        }
    }

    #[test]
    fn converts_length_prefixed_nals() {
        let mut out = Vec::new();
        to_annex_b(&[0, 0, 0, 2, 0xAA, 0xBB, 0, 0, 0, 1, 0xCC, 0, 0, 0, 9, 1], &mut out);
        assert_eq!(out, [0, 0, 0, 1, 0xAA, 0xBB, 0, 0, 0, 1, 0xCC]);
    }
}
