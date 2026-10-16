use std::sync::{Arc, Mutex, MutexGuard};

/// More separate rectangles than this are merged into full-frame damage.
const MAX_DAMAGE_RECTS: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

/// The remote desktop image shared between the session task and the UI.
///
/// Pixels are 32-bit BGRX (bytes B, G, R, X), rows `width * 4` bytes apart, so the UI can hand
/// the bytes straight to a `B8g8r8x8` memory texture. The session task decodes into its own
/// image and only copies changed rectangles in here, so the lock is held for memcpy-sized
/// moments, never during decoding.
#[derive(Clone, Default)]
pub struct Framebuffer(Arc<Mutex<Inner>>);

#[derive(Default)]
struct Inner {
    width: u32,
    height: u32,
    data: Vec<u8>,
    damage: Vec<Rect>,
    full_damage: bool,
    /// A `FrameReady` event was sent and the UI hasn't taken the damage yet.
    notified: bool,
    /// Counts resizes (including to the same size, as after a graphics reset).
    generation: u64,
}

/// A view of the framebuffer and everything that changed since the last [`Framebuffer::take_damage`].
pub struct Damage<'a> {
    pub width: u32,
    pub height: u32,
    pub stride: usize,
    pub data: &'a [u8],
    /// The UI must repaint everything (first frame, resize, or too many rectangles).
    pub full: bool,
    /// Changed areas; empty when `full` is set.
    pub rects: &'a [Rect],
}

impl Framebuffer {
    fn lock(&self) -> MutexGuard<'_, Inner> {
        // A panic while holding the lock can only come from a bug in our copy code; the data is
        // still a valid image, so keep going.
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn size(&self) -> (u32, u32) {
        let inner = self.lock();
        (inner.width, inner.height)
    }

    /// Calls `f` with the current image and damage, then clears the damage. Keep `f` short:
    /// the session task waits for the lock to publish new pixels.
    pub fn take_damage<R>(&self, f: impl FnOnce(Damage<'_>) -> R) -> R {
        let mut inner = self.lock();
        let inner = &mut *inner;
        let result = f(Damage {
            width: inner.width,
            height: inner.height,
            stride: inner.width as usize * 4,
            data: &inner.data,
            full: inner.full_damage,
            rects: if inner.full_damage { &[] } else { &inner.damage },
        });
        inner.damage.clear();
        inner.full_damage = false;
        inner.notified = false;
        result
    }

    /// Changes whenever the desktop is (re)sized.
    pub(crate) fn generation(&self) -> u64 {
        self.lock().generation
    }

    pub(crate) fn resize(&self, width: u32, height: u32) {
        let mut inner = self.lock();
        inner.generation += 1;
        inner.width = width;
        inner.height = height;
        inner.data = vec![0; width as usize * height as usize * 4];
        inner.damage.clear();
        inner.full_damage = true;
    }

    /// Copies `rects` from `src` (same size and format) and records them as damage.
    /// Returns true if the UI should be notified.
    pub(crate) fn publish(&self, src: &[u8], rects: &[Rect]) -> bool {
        let mut inner = self.lock();
        let stride = inner.width as usize * 4;
        if src.len() != inner.data.len() {
            return false;
        }
        for r in rects {
            let Some(r) = clip(*r, inner.width, inner.height) else {
                continue;
            };
            let start = r.x as usize * 4;
            let len = r.width as usize * 4;
            for row in r.y as usize..(r.y + r.height) as usize {
                let offset = row * stride + start;
                inner.data[offset..offset + len].copy_from_slice(&src[offset..offset + len]);
            }
            if !inner.full_damage {
                if inner.damage.len() >= MAX_DAMAGE_RECTS {
                    inner.full_damage = true;
                    inner.damage.clear();
                } else {
                    inner.damage.push(r);
                }
            }
        }
        let notify = !inner.notified && (inner.full_damage || !inner.damage.is_empty());
        inner.notified |= notify;
        notify
    }

    /// Records full damage without changing pixels (after a resize). Returns true if the UI should be notified.
    pub(crate) fn mark_full_damage(&self) -> bool {
        let mut inner = self.lock();
        inner.full_damage = true;
        inner.damage.clear();
        let notify = !inner.notified;
        inner.notified = true;
        notify
    }
}

fn clip(r: Rect, width: u32, height: u32) -> Option<Rect> {
    let x2 = (r.x + r.width).min(width);
    let y2 = (r.y + r.height).min(height);
    (r.x < x2 && r.y < y2).then(|| Rect {
        x: r.x,
        y: r.y,
        width: x2 - r.x,
        height: y2 - r.y,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: u32, y: u32, width: u32, height: u32) -> Rect {
        Rect { x, y, width, height }
    }

    #[test]
    fn copies_only_damaged_pixels_and_notifies_once() {
        let fb = Framebuffer::default();
        fb.resize(4, 3);
        fb.take_damage(|_| ());
        let src = vec![0xAB; 4 * 3 * 4];

        assert!(fb.publish(&src, &[rect(1, 1, 2, 1)]));
        assert!(
            !fb.publish(&src, &[rect(0, 0, 1, 1)]),
            "second update before take must not notify again"
        );

        fb.take_damage(|d| {
            assert!(!d.full);
            assert_eq!(d.rects, &[rect(1, 1, 2, 1), rect(0, 0, 1, 1)]);
            let px = |x: usize, y: usize| d.data[y * d.stride + x * 4];
            assert_eq!(px(0, 0), 0xAB);
            assert_eq!(px(1, 1), 0xAB);
            assert_eq!(px(2, 1), 0xAB);
            assert_eq!(px(3, 1), 0);
            assert_eq!(px(0, 2), 0);
        });
        assert!(fb.publish(&src, &[rect(0, 0, 1, 1)]), "notifies again after take");
    }

    #[test]
    fn clips_rects_and_collapses_many_into_full() {
        let fb = Framebuffer::default();
        fb.resize(10, 10);
        fb.take_damage(|_| ());
        let src = vec![1; 10 * 10 * 4];
        fb.publish(&src, &[rect(8, 8, 5, 5), rect(20, 20, 1, 1)]);
        fb.take_damage(|d| assert_eq!(d.rects, &[rect(8, 8, 2, 2)]));

        let many: Vec<_> = (0..=MAX_DAMAGE_RECTS as u32).map(|i| rect(i % 10, 0, 1, 1)).collect();
        fb.publish(&src, &many);
        fb.take_damage(|d| assert!(d.full && d.rects.is_empty()));
    }

    #[test]
    fn ignores_source_of_wrong_size() {
        let fb = Framebuffer::default();
        fb.resize(2, 2);
        assert!(!fb.publish(&[0; 4], &[rect(0, 0, 1, 1)]));
    }
}
