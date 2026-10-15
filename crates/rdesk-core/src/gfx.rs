//! Graphics Pipeline (MS-RDPEGFX): H.264 and the other EGFX codecs.
//!
//! `ironrdp-egfx` negotiates capabilities, tracks frames and decodes AVC420 (H.264, through
//! OpenH264 loaded at run time). Everything else lands here: the server draws into off-screen
//! surfaces (codec bitmaps, solid fills, copies, a bitmap cache) and maps surfaces onto the
//! desktop. Surfaces are kept as BGRX buffers; mapped surfaces are composited into a
//! desktop-sized image, which is handed to the shared [`Framebuffer`] at the end of each frame.

use std::collections::HashMap;

use ironrdp::graphics::clearcodec::ClearCodecDecoder;
use ironrdp::graphics::progressive::ProgressiveDecoder;
use ironrdp::graphics::rdp6::BitmapStreamDecoder;
use ironrdp::pdu::geometry::{ExclusiveRectangle, Rectangle as _};
use ironrdp_egfx::client::{BitmapUpdate, GraphicsPipelineHandler, Surface as SurfaceInfo};
use ironrdp_egfx::decode::H264Decoder as _;
use ironrdp_egfx::pdu::{
    Avc444BitmapStream, CapabilitiesV10Flags, CapabilitiesV107Flags, Encoding, MapSurfaceToScaledOutputPdu,
};
use ironrdp_egfx::pdu::{
    CacheToSurfacePdu, CapabilitiesV8Flags, CapabilitiesV81Flags, CapabilitySet, Codec1Type, EvictCacheEntryPdu,
    GfxPdu, SolidFillPdu, SurfaceToCachePdu, SurfaceToSurfacePdu, WireToSurface1Pdu, WireToSurface2Pdu,
};
use tracing::{debug, info, trace, warn};

use crate::event::SessionEvent;
use crate::framebuffer::{Framebuffer, Rect};
use crate::h264::SharedDecoder;

#[derive(Debug)]
struct Surface {
    width: u32,
    height: u32,
    /// BGRX, `width * 4` bytes per row.
    data: Vec<u8>,
    /// Desktop position, once mapped.
    origin: Option<(u32, u32)>,
}

/// A cached BGRX bitmap (SurfaceToCache / CacheToSurface).
#[derive(Debug)]
struct Cached {
    width: u32,
    height: u32,
    data: Vec<u8>,
}

pub(crate) struct GfxOutput {
    framebuffer: Framebuffer,
    events: async_channel::Sender<SessionEvent>,
    /// Shared with IronRDP's AVC420 path; used here for AVC444.
    h264: SharedDecoder,
    width: u32,
    height: u32,
    /// The composited desktop, BGRX.
    desktop: Vec<u8>,
    dirty: Vec<Rect>,
    surfaces: HashMap<u16, Surface>,
    cache: HashMap<u16, Cached>,
    clear: ClearCodecDecoder,
    planar: BitmapStreamDecoder,
    progressive: ProgressiveDecoder,
    scratch: Vec<u8>,
}

impl GfxOutput {
    pub(crate) fn new(
        framebuffer: Framebuffer,
        events: async_channel::Sender<SessionEvent>,
        h264: SharedDecoder,
    ) -> Self {
        Self {
            framebuffer,
            events,
            h264,
            width: 0,
            height: 0,
            desktop: Vec::new(),
            dirty: Vec::new(),
            surfaces: HashMap::new(),
            cache: HashMap::new(),
            clear: ClearCodecDecoder::new(),
            planar: BitmapStreamDecoder::default(),
            progressive: ProgressiveDecoder::new(),
            scratch: Vec::new(),
        }
    }

    /// Writes a BGRX block into a surface and composites it onto the desktop.
    fn blit(&mut self, surface_id: u16, x: u32, y: u32, width: u32, height: u32, bgrx: &[u8]) {
        let Some(surface) = self.surfaces.get_mut(&surface_id) else {
            trace!(surface_id, "Drawing on an unknown surface");
            return;
        };
        let w = width.min(surface.width.saturating_sub(x));
        let h = height.min(surface.height.saturating_sub(y));
        if w == 0 || h == 0 || bgrx.len() < (width * height * 4) as usize {
            return;
        }
        let (src_stride, dst_stride, len) = (width as usize * 4, surface.width as usize * 4, w as usize * 4);
        for row in 0..h as usize {
            let src = row * src_stride;
            let dst = (y as usize + row) * dst_stride + x as usize * 4;
            surface.data[dst..dst + len].copy_from_slice(&bgrx[src..src + len]);
        }
        self.composite(surface_id, x, y, w, h);
    }

    /// Copies a surface region onto the desktop, if the surface is mapped.
    fn composite(&mut self, surface_id: u16, x: u32, y: u32, width: u32, height: u32) {
        let Some(surface) = self.surfaces.get(&surface_id) else {
            return;
        };
        let Some((ox, oy)) = surface.origin else { return };
        let (dx, dy) = (ox + x, oy + y);
        let w = width.min(self.width.saturating_sub(dx));
        let h = height.min(self.height.saturating_sub(dy));
        if w == 0 || h == 0 {
            return;
        }
        let (src_stride, dst_stride, len) = (surface.width as usize * 4, self.width as usize * 4, w as usize * 4);
        for row in 0..h as usize {
            let src = (y as usize + row) * src_stride + x as usize * 4;
            let dst = (dy as usize + row) * dst_stride + dx as usize * 4;
            self.desktop[dst..dst + len].copy_from_slice(&surface.data[src..src + len]);
        }
        self.dirty.push(Rect {
            x: dx,
            y: dy,
            width: w,
            height: h,
        });
    }

    /// Reads a region of a surface, clipped to it (`None` when nothing remains).
    fn read(&self, surface_id: u16, rect: &ExclusiveRectangle) -> Option<Cached> {
        let surface = self.surfaces.get(&surface_id)?;
        let (x, y) = (u32::from(rect.left), u32::from(rect.top));
        let width = u32::from(rect.right).min(surface.width).checked_sub(x)?;
        let height = u32::from(rect.bottom).min(surface.height).checked_sub(y)?;
        if width == 0 || height == 0 {
            return None;
        }
        let mut data = Vec::with_capacity((width * height * 4) as usize);
        let stride = surface.width as usize * 4;
        for row in 0..height as usize {
            let start = (y as usize + row) * stride + x as usize * 4;
            data.extend_from_slice(&surface.data[start..start + width as usize * 4]);
        }
        Some(Cached { width, height, data })
    }

    fn decode_wire_to_surface1(&mut self, pdu: &WireToSurface1Pdu) {
        let rect = &pdu.destination_rectangle;
        let (x, y) = (u32::from(rect.left), u32::from(rect.top));
        let (width, height) = (rect.width(), rect.height());
        let mut bgrx = std::mem::take(&mut self.scratch);
        let decoded = match pdu.codec_id {
            Codec1Type::ClearCodec => match self.clear.decode(&pdu.bitmap_data, width, height) {
                Ok(bgra) => {
                    bgrx = bgra;
                    true
                }
                Err(e) => {
                    warn!(error = %e, "ClearCodec decode failed");
                    false
                }
            },
            Codec1Type::Avc444 | Codec1Type::Avc444v2 => match self.decode_avc444(pdu, &mut bgrx) {
                Ok(decoded) => decoded,
                Err(e) => {
                    warn!(error = %e, "AVC444 decode failed");
                    false
                }
            },
            Codec1Type::Planar => {
                let mut rgb = Vec::new();
                match self.planar.decode_bitmap_stream_to_rgb24(
                    &pdu.bitmap_data,
                    &mut rgb,
                    usize::from(width),
                    usize::from(height),
                ) {
                    Ok(()) => {
                        bgrx.clear();
                        bgrx.extend(rgb.as_chunks::<3>().0.iter().flat_map(|&[r, g, b]| [b, g, r, 0xFF]));
                        true
                    }
                    Err(e) => {
                        warn!(error = %e, "Planar decode failed");
                        false
                    }
                }
            }
            other => {
                debug!(codec = ?other, "Unsupported EGFX codec");
                false
            }
        };
        if decoded {
            self.blit(pdu.surface_id, x, y, u32::from(width), u32::from(height), &bgrx);
        }
        self.scratch = bgrx;
    }

    /// AVC444 sends a regular 4:2:0 picture (the main view) plus extra chroma in a second stream.
    /// Showing the main view gives AVC420 quality; chroma-only updates are skipped.
    fn decode_avc444(&mut self, pdu: &WireToSurface1Pdu, bgrx: &mut Vec<u8>) -> Result<bool, String> {
        let stream = ironrdp::core::decode::<Avc444BitmapStream<'_>>(&pdu.bitmap_data).map_err(|e| e.to_string())?;
        if stream.encoding == Encoding::CHROMA {
            return Ok(false);
        }
        let frame = self.h264.decode(stream.stream1.data).map_err(|e| e.to_string())?;
        let rect = &pdu.destination_rectangle;
        let (width, height) = (usize::from(rect.width()), usize::from(rect.height()));
        let frame_width = frame.width() as usize;
        if frame_width < width || (frame.height() as usize) < height {
            return Err("decoded picture smaller than its rectangle".to_owned());
        }
        bgrx.clear();
        for row in frame.data().chunks_exact(frame_width * 4).take(height) {
            bgrx.extend(
                row[..width * 4]
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .flat_map(|&[r, g, b, _]| [b, g, r, 0xFF]),
            );
        }
        Ok(true)
    }

    fn decode_progressive(&mut self, pdu: &WireToSurface2Pdu) {
        let Some(surface) = self.surfaces.get(&pdu.surface_id) else {
            return;
        };
        let (width, height) = (surface.width as u16, surface.height as u16);
        let tiles = match self
            .progressive
            .decode_bitmap(pdu.codec_context_id, width, height, &pdu.bitmap_data)
        {
            Ok(tiles) => tiles,
            Err(e) => {
                warn!(error = %e, "Progressive decode failed");
                return;
            }
        };
        let mut bgrx = std::mem::take(&mut self.scratch);
        for tile in tiles {
            rgba_to_bgrx(&tile.pixels, &mut bgrx);
            let (x, y) = (u32::from(tile.x_idx) * 64, u32::from(tile.y_idx) * 64);
            self.blit(pdu.surface_id, x, y, 64, 64, &bgrx);
        }
        self.scratch = bgrx;
    }
}

fn rgba_to_bgrx(rgba: &[u8], out: &mut Vec<u8>) {
    out.clear();
    out.extend(rgba.as_chunks::<4>().0.iter().flat_map(|&[r, g, b, _]| [b, g, r, 0xFF]));
}

impl GraphicsPipelineHandler for GfxOutput {
    fn capabilities(&self) -> Vec<CapabilitySet> {
        // Windows only uses H.264 with version 10+ clients, and with "thin client" set it uses it
        // for screen content too instead of the progressive codec. AVC444 is shown from its main
        // view.
        vec![
            CapabilitySet::V10_7 {
                flags: CapabilitiesV107Flags::SMALL_CACHE
                    | CapabilitiesV107Flags::AVC_THIN_CLIENT
                    | CapabilitiesV107Flags::SCALEDMAP_DISABLE,
            },
            CapabilitySet::V10 {
                flags: CapabilitiesV10Flags::SMALL_CACHE,
            },
            CapabilitySet::V8_1 {
                flags: CapabilitiesV81Flags::AVC420_ENABLED | CapabilitiesV81Flags::SMALL_CACHE,
            },
            CapabilitySet::V8 {
                flags: CapabilitiesV8Flags::SMALL_CACHE,
            },
        ]
    }

    fn on_capabilities_confirmed(&mut self, caps: &CapabilitySet) {
        info!(?caps, "Graphics Pipeline active");
    }

    fn on_reset_graphics(&mut self, width: u32, height: u32) {
        debug!(width, height, "Graphics reset");
        self.width = width;
        self.height = height;
        self.desktop = vec![0; (width * height * 4) as usize];
        self.dirty.clear();
        self.framebuffer.resize(width, height);
        let (Ok(width), Ok(height)) = (u16::try_from(width), u16::try_from(height)) else {
            return;
        };
        let _ = self.events.try_send(SessionEvent::DesktopResized { width, height });
    }

    fn on_surface_created(&mut self, surface: &SurfaceInfo) {
        let (width, height) = (u32::from(surface.width), u32::from(surface.height));
        self.surfaces.insert(
            surface.id,
            Surface {
                width,
                height,
                data: vec![0; (width * height * 4) as usize],
                origin: None,
            },
        );
    }

    fn on_surface_deleted(&mut self, surface_id: u16) {
        self.surfaces.remove(&surface_id);
    }

    fn on_surface_mapped(&mut self, surface_id: u16, origin_x: u32, origin_y: u32) {
        let Some(surface) = self.surfaces.get_mut(&surface_id) else {
            return;
        };
        surface.origin = Some((origin_x, origin_y));
        let (width, height) = (surface.width, surface.height);
        self.composite(surface_id, 0, 0, width, height);
    }

    fn on_map_surface_to_scaled_output(&mut self, pdu: &MapSurfaceToScaledOutputPdu) {
        let Some(surface) = self.surfaces.get(&pdu.surface_id) else {
            return;
        };
        if (surface.width, surface.height) != (pdu.target_width, pdu.target_height) {
            warn!(
                surface_id = pdu.surface_id,
                "Scaled surface mapping isn't supported; showing it unscaled"
            );
        }
        self.on_surface_mapped(pdu.surface_id, pdu.output_origin_x, pdu.output_origin_y);
    }

    fn on_bitmap_updated(&mut self, update: &BitmapUpdate) {
        let rect = &update.destination_rectangle;
        let mut bgrx = std::mem::take(&mut self.scratch);
        rgba_to_bgrx(&update.data, &mut bgrx);
        self.blit(
            update.surface_id,
            u32::from(rect.left),
            u32::from(rect.top),
            u32::from(update.width),
            u32::from(update.height),
            &bgrx,
        );
        self.scratch = bgrx;
    }

    fn on_frame_complete(&mut self, _frame_id: u32) {
        if self.dirty.is_empty() {
            return;
        }
        if self.framebuffer.publish(&self.desktop, &self.dirty) {
            let _ = self.events.try_send(SessionEvent::FrameReady);
        }
        self.dirty.clear();
    }

    fn on_solid_fill(&mut self, pdu: &SolidFillPdu) {
        let c = &pdu.fill_pixel;
        let pixel = [c.b, c.g, c.r, 0xFF];
        for rect in &pdu.rectangles {
            let (width, height) = (u32::from(rect.width()), u32::from(rect.height()));
            let fill: Vec<u8> = pixel.repeat((width * height) as usize);
            self.blit(
                pdu.surface_id,
                u32::from(rect.left),
                u32::from(rect.top),
                width,
                height,
                &fill,
            );
        }
    }

    fn on_surface_to_surface(&mut self, pdu: &SurfaceToSurfacePdu) {
        // Read first: source and destination may be the same surface and overlap.
        let Some(block) = self.read(pdu.source_surface_id, &pdu.source_rectangle) else {
            return;
        };
        for point in &pdu.destination_points {
            let (x, y) = (u32::from(point.x), u32::from(point.y));
            self.blit(pdu.destination_surface_id, x, y, block.width, block.height, &block.data);
        }
    }

    fn on_surface_to_cache(&mut self, pdu: &SurfaceToCachePdu) {
        if let Some(block) = self.read(pdu.surface_id, &pdu.source_rectangle) {
            self.cache.insert(pdu.cache_slot, block);
        }
    }

    fn on_cache_to_surface(&mut self, pdu: &CacheToSurfacePdu) {
        let Some(block) = self.cache.remove(&pdu.cache_slot) else {
            trace!(slot = pdu.cache_slot, "Unknown cache slot");
            return;
        };
        for point in &pdu.destination_points {
            let (x, y) = (u32::from(point.x), u32::from(point.y));
            self.blit(pdu.surface_id, x, y, block.width, block.height, &block.data);
        }
        self.cache.insert(pdu.cache_slot, block);
    }

    fn on_evict_cache_entry(&mut self, pdu: &EvictCacheEntryPdu) {
        self.cache.remove(&pdu.cache_slot);
    }

    fn on_wire_to_surface2(&mut self, pdu: &WireToSurface2Pdu) {
        self.decode_progressive(pdu);
    }

    fn on_unhandled_pdu(&mut self, pdu: &GfxPdu) {
        match pdu {
            GfxPdu::WireToSurface1(pdu) => self.decode_wire_to_surface1(pdu),
            other => trace!(pdu = ?std::mem::discriminant(other), "Unhandled EGFX PDU"),
        }
    }

    fn on_close(&mut self) {
        debug!("Graphics Pipeline closed");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ironrdp_egfx::decode::{DecodedFrame, DecoderError, DecoderResult, H264Decoder};
    use ironrdp_egfx::pdu::{Color, Point};

    struct NoH264;

    impl H264Decoder for NoH264 {
        fn decode(&mut self, _data: &[u8]) -> DecoderResult<DecodedFrame> {
            Err(DecoderError::msg("not in tests"))
        }
    }

    fn output() -> GfxOutput {
        let (events, _) = async_channel::unbounded();
        let mut gfx = GfxOutput::new(Framebuffer::default(), events, SharedDecoder::new(Box::new(NoH264)));
        gfx.on_reset_graphics(8, 4);
        gfx.surfaces.insert(
            1,
            Surface {
                width: 4,
                height: 4,
                data: vec![0; 64],
                origin: None,
            },
        );
        gfx.on_surface_mapped(1, 4, 0);
        gfx
    }

    fn pixel(gfx: &GfxOutput, x: usize, y: usize) -> [u8; 4] {
        let i = (y * gfx.width as usize + x) * 4;
        gfx.desktop[i..i + 4].try_into().unwrap()
    }

    fn rect(left: u16, top: u16, right: u16, bottom: u16) -> ExclusiveRectangle {
        ExclusiveRectangle {
            left,
            top,
            right,
            bottom,
        }
    }

    #[test]
    fn fills_copy_and_cache_land_on_the_desktop() {
        let mut gfx = output();
        gfx.on_solid_fill(&SolidFillPdu {
            surface_id: 1,
            fill_pixel: Color {
                b: 1,
                g: 2,
                r: 3,
                xa: 0,
            },
            rectangles: vec![rect(0, 0, 2, 2)],
        });
        // Surface (1, 1) is desktop (5, 1).
        assert_eq!(pixel(&gfx, 5, 1), [1, 2, 3, 0xFF]);
        assert_eq!(pixel(&gfx, 6, 1), [0, 0, 0, 0]);

        gfx.on_surface_to_surface(&SurfaceToSurfacePdu {
            source_surface_id: 1,
            destination_surface_id: 1,
            source_rectangle: rect(0, 0, 2, 2),
            destination_points: vec![Point { x: 2, y: 2 }],
        });
        assert_eq!(pixel(&gfx, 7, 3), [1, 2, 3, 0xFF]);

        gfx.on_surface_to_cache(&SurfaceToCachePdu {
            surface_id: 1,
            cache_key: 0,
            cache_slot: 7,
            source_rectangle: rect(0, 0, 1, 1),
        });
        gfx.on_cache_to_surface(&CacheToSurfacePdu {
            cache_slot: 7,
            surface_id: 1,
            destination_points: vec![Point { x: 3, y: 0 }],
        });
        assert_eq!(pixel(&gfx, 7, 0), [1, 2, 3, 0xFF]);
        assert!(!gfx.dirty.is_empty());
    }

    #[test]
    fn drawing_is_clipped_to_the_surface() {
        let mut gfx = output();
        gfx.on_solid_fill(&SolidFillPdu {
            surface_id: 1,
            fill_pixel: Color {
                b: 9,
                g: 9,
                r: 9,
                xa: 0,
            },
            rectangles: vec![rect(3, 3, 10, 10)],
        });
        assert_eq!(pixel(&gfx, 7, 3), [9, 9, 9, 0xFF]);
        // Unmapped and unknown surfaces are ignored.
        gfx.on_solid_fill(&SolidFillPdu {
            surface_id: 2,
            fill_pixel: Color {
                b: 1,
                g: 1,
                r: 1,
                xa: 0,
            },
            rectangles: vec![rect(0, 0, 1, 1)],
        });
    }
}
