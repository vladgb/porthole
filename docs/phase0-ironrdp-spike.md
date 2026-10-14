# Phase 0 — IronRDP spike findings (2026-10-14)

Spike: `spikes/rdp-screenshot` (IronRDP 0.17 meta crate, ironrdp-connector 0.10, ironrdp-session 0.11,
ironrdp-blocking 0.10, sspi 0.21, rustls/aws-lc).

```sh
docker build -t rdesk-xrdp tests/xrdp
docker run --rm -d -p 127.0.0.1:13389:3389 --name rdesk-xrdp rdesk-xrdp
RDESK_PASSWORD=rdesk cargo run -p rdp-screenshot -- --host 127.0.0.1 --port 13389 -u rdesk --no-nla -o out.png
```

## Results against xrdp (Ubuntu 24.04, xorgxrdp, TLS security)

| Check | Result |
|---|---|
| TCP → X.224 → TLS → licensing → capability exchange | ✅ connected in ~50 ms |
| Autologon with supplied credentials | ✅ (requires `autologon: true`; without it xrdp shows its login dialog) |
| Fast-path bitmap updates → RGBA framebuffer → PNG | ✅ desktop and xterm text render correctly |
| Server certificate details (subject, issuer, validity, SHA-256) | ✅ extracted; feeds the trust dialog |
| xrdp **login screen** bitmaps | ❌ rows sheared (stride mismatch in a legacy bitmap path) — reproduce, then file upstream |
| Faint dots on a 64 px grid in flat white areas | ⚠️ likely RemoteFX tile-edge artifacts — compare against FreeRDP |
| `unsupported fast-path update code` warning once per connect | ⚠️ harmless so far; identify the code |
| NLA / CredSSP (NTLM) | ⏳ not testable on xrdp — **needs the Windows 11 VM** |

## IronRDP 0.17 coverage (published crates.io set)

| Area | Status | Consequence for rdesk |
|---|---|---|
| Connector, TLS, CredSSP (NTLM/Kerberos via sspi) | Present | Use as-is. |
| Bitmap / planar / interleaved, RemoteFX (`session::rfx`) | Present | MVP graphics covered. |
| Bulk compression (MPPC/NCRUSH/XCRUSH, `ironrdp-bulk`) | Present | P2 item mostly free. |
| **EGFX / RDPGFX channel** | **Missing** — only building blocks in `ironrdp-graphics` (ZGFX, ClearCodec, progressive) | Implement the RDPGFX DVC client in `rdesk-gfx` (Phase 2). Biggest owned work item. |
| **H.264 (AVC420/444)** | **Missing** | `openh264`-based decoder in `rdesk-gfx`, as planned. |
| Display Control (dynamic resolution) | Present (`ironrdp-displaycontrol`) | P1 dynamic resize is cheap. |
| Clipboard | Protocol present; native backend is **Windows-only** | Write a GTK/Wayland backend in `rdesk-channels` (planned). |
| RDPDR (drives, smart card) | Protocol + noop backend only | Filesystem/smart-card backends are ours. |
| RDPSND (audio out) | Present, `cpal` backend in `ironrdp-rdpsnd-native` | Start with it; PipeWire backend later if needed. |
| AUDIN (mic), camera (RDPECAM), RAIL (RemoteApp) | Not in the published set | Own crates (Phase 3 / Phase 6). |
| RD Gateway | `ironrdp-mstsgu` **0.0.1**: WebSocket transport only, no HTTP/RPC transport | Wrap/extend in `rdesk-gateway`; HTTP transport is ours. |
| Server-side Deactivate-All / reactivation | Exposed (`ActiveStageOutput::DeactivateAll`, `activation_factory`) | Needed for resize/reconnect; handle in `rdesk-core`. |

## Decision

**Go with IronRDP.** The core connection path works and the crates are modular enough to add our own channels.
Re-check NLA, H.264 negotiation and gateway against Windows before Phase 1 closes. FreeRDP FFI stays the fallback
only if NLA against Windows fails in ways we can't fix upstream.

## Still open in Phase 0

- [ ] NLA (NTLM) + screenshot against Windows 11 Pro / Server 2022 VM.
- [ ] GTK4 60 fps texture spike (blocked on `libgtk-4-dev libadwaita-1-dev` being installed).
- [ ] Reproduce the login-screen shear in isolation; file upstream issue.
