# Phase 0 — GTK4 texture presentation spike (2026-10-14)

Spike: `spikes/gtk-texture`. A producer thread (standing in for the RDP session task) draws into a shared BGRX
framebuffer at 60 Hz and records dirty rectangles; the UI uploads on every frame-clock tick via `GdkMemoryTexture`.

- `--mode full`: new texture every frame.
- `--mode dirty`: `GdkMemoryTextureBuilder` with `update_texture` + `update_region` (GTK ≥ 4.16), so only damage
  is re-uploaded to the GPU.
- `--scene desktop`: moving 320×240 window + typing + full repaint every 3 s (≈2–5% damage per frame).
- `--scene video`: every pixel changes every frame.

```sh
cargo run --release -p gtk-texture -- --mode dirty --scene desktop --size 3840x2160 --seconds 8
```

Machine: Lenovo Yoga Pro 7 14APH8 (Ryzen 7 7840HS, Radeon 780M iGPU), Ubuntu 26.04, GNOME Wayland, GTK 4.22.4,
90 Hz panel, 1280×720 window (texture scaled down). Numbers exclude a 1 s warm-up; 8 s runs.

## Results

**1080p — comfortable in every mode (Vulkan renderer, the GTK default):**

| Mode / scene | Textures/s (producer 60) | UI-thread cost p50 / p99 | Frame intervals > 25 ms |
|---|---|---|---|
| full / desktop | 58.4 | 0.69 / 2.44 ms | 1 |
| dirty / desktop | 57.7 | 0.71 / 2.86 ms | 0 |
| full / video | 57.7 | 0.77 / 3.80 ms | 0 |
| dirty / video | 58.4 | 0.77 / 3.89 ms | 0 |

The ~3% gap to 60 is producer frames coalesced into the next tick (not lost content). The GL renderer (`ngl`)
also holds 1080p (dirty/desktop: 59.6/s).

**4K — at the edge:**

| Renderer | full / desktop | dirty / desktop |
|---|---|---|
| Vulkan (default), 4 runs each | 51–59 fps | 38–57 fps |
| GL (`ngl`), 1 run | 57 fps, copy p50 6.4 ms | **27 fps**, copy p50 17 ms |

## Findings

1. **The UI-thread cost is the full-frame memcpy.** A `GdkMemoryTexture` keeps a reference to its bytes, so every
   frame needs a fresh full copy (8 MB at 1080p, 33 MB at 4K) even when `update_region` limits the GPU upload.
   ~0.7 ms at 1080p, ~3 ms at 4K.
2. **`update_region` doesn't help at 4K on this iGPU,** and with the GL renderer it collapses to 27 fps (the
   memcpy itself slows to 17 ms — the driver is competing for shared memory bandwidth).
3. **Lock contention is small** as long as the session task doesn't hold the framebuffer lock while decoding a
   whole frame (video scene p99 lock wait: 2.5 ms at 1080p, 6 ms at 4K). The real session task should decode into
   its own buffer and publish damage, not draw under the lock.
4. Frame pacing is good once the window is mapped; the first second contains the only long frames.

## Decision

- **MVP (Phase 1): `GdkMemoryTexture` + `update_region`.** Good enough up to 1440p, which covers most sessions.
- **Phase 2 GPU path is required for 4K / HiDPI:** keep a persistent GL texture, upload only dirty rects with
  `glTexSubImage2D` (or import decoded H.264 frames as dmabufs), and hand it to GTK via `GdkGLTextureBuilder`
  (also supports `update_region`) or `GdkDmabufTextureBuilder`. That removes the per-frame full copy entirely.
- Re-run this spike on a discrete GPU and on Intel graphics before committing to the 4K budget in PLAN.md §6.
