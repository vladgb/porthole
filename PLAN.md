# Linux RDP Client — Project Plan

A native Linux Remote Desktop client written in Rust, modeled on Microsoft's
macOS client ("Windows App", formerly "Microsoft Remote Desktop"): a polished
connection center, saved PCs with thumbnails, account and gateway managers,
dynamic resolution, multi-monitor, device redirection, and Azure Virtual
Desktop / Windows 365 workspaces.

Working name: **`rdesk`** (rename freely).

---

## 1. Goals and non-goals

**Goals**
- Feel native on modern Linux desktops (GNOME first, KDE and wlroots usable).
- Wayland-first, X11 supported.
- Feature parity with the macOS client for everyday use (see §4).
- Pure Rust where practical; no FreeRDP dependency.
- Secure by default: NLA, TLS certificate verification, secrets in the system keyring.
- Distributable as Flatpak (primary), plus .deb/.rpm and AppImage.

**Non-goals (for v1)**
- Being an RDP *server*.
- USB device redirection (RemoteFX USB) — the Mac client doesn't have it either.
- VNC/SSH/other protocols (Remmina already covers multi-protocol).

---

## 2. Technology choices

| Concern | Choice | Why / alternatives |
|---|---|---|
| RDP protocol stack | **IronRDP** (`ironrdp-*` crates, Devolutions) | Pure Rust, actively maintained, modular (connector, session, graphics, channels). Alternative: FFI to FreeRDP 3 — more complete, but C, unsafe, and no maintained Rust bindings. |
| Authentication (NLA/CredSSP) | **`sspi`** (sspi-rs) | NTLM + Kerberos + CredSSP in Rust; used by IronRDP. |
| TLS | `rustls` (via `ironrdp-tls`) | Need custom certificate verifier for the trust dialog. |
| Async runtime | `tokio` | Network I/O, channels, timers. |
| UI toolkit | **GTK4 + libadwaita** via `gtk4-rs` / `libadwaita-rs`, structured with **`relm4`** | Closest to the Mac app's native look on Linux; mature Wayland support, shortcut inhibition, HiDPI, fractional scaling, portals. Alternatives: `iced`, `slint`, `egui` (less native, weaker a11y/IME). |
| Remote display rendering | GTK4 `GdkMemoryTexture` / `GdkTextureDownloader` for MVP → `gtk::GLArea` (or `GdkDmabufTexture`) for GPU path | Dirty-rect uploads to a GPU texture, scaled with linear filtering. |
| Video codecs | `openh264` crate (AVC420/444), optional VA-API via `gstreamer` or `ffmpeg-next` | Needed for the RDPGFX (EGFX) H.264 path used by modern Windows. |
| Audio out / in | `pipewire` crate (fallback `cpal`) | RDPSND (playback) + AUDIN (microphone). |
| Secrets | `oo7` (Secret Service / Flatpak portal) | GNOME Keyring, KWallet via Secret Service. |
| Config | `serde` + TOML in `$XDG_CONFIG_HOME/rdesk/` | Plus `.rdp` file import/export. |
| Smart cards | `pcsc` crate | Smart card redirection (RDPDR) + smart-card NLA later. |
| Camera | PipeWire camera portal | RDP camera redirection (MS-RDPECAM). |
| Logging | `tracing` + `tracing-subscriber` | Per-session log files for diagnostics. |
| Entra ID / AVD auth | `oauth2` + system browser via `ashpd` (OpenURI portal) | Workspaces subscription & AVD/W365 tokens. |
| Packaging | Flatpak (Flathub), `cargo-deb`, `cargo-generate-rpm` | |

> **Key risk:** IronRDP's coverage of some channels (RD Gateway HTTP/WebSocket
> transport, camera, AVD-specific features) is incomplete. Plan to contribute
> upstream or implement those pieces in our own crates against the `ironrdp-dvc` /
> `ironrdp-svc` traits. Do a feature spike in Phase 0 to confirm current status.

---

## 3. Architecture

### 3.1 Cargo workspace layout

```
linux-rdp-client/
├── Cargo.toml                  # workspace
├── crates/
│   ├── rdesk-core/             # session engine: connect, run loop, events (no UI)
│   ├── rdesk-gfx/              # framebuffer, codecs (RemoteFX, H.264/EGFX), surfaces
│   ├── rdesk-input/            # keyboard scancode tables, layouts, mouse/scroll mapping
│   ├── rdesk-channels/         # clipboard, drives, audio, printers, smartcard, camera, display-control
│   ├── rdesk-store/            # profiles, groups, accounts, gateways, .rdp parser, keyring
│   ├── rdesk-workspaces/       # AVD / W365 / RD Web feed discovery, Entra ID auth
│   └── rdesk-gateway/          # RD Gateway (HTTP / WebSocket / UDP) transport
├── app/                        # GTK4/libadwaita application (binary `rdesk`)
│   ├── src/connection_center/  # PCs / Workspaces grid, search, groups
│   ├── src/session_window/     # remote display widget, fullscreen, toolbar
│   ├── src/dialogs/            # add/edit PC, accounts, gateways, certificate, prefs
│   └── data/                   # .desktop, icons, metainfo, gschema
├── cli/                        # optional `rdesk-cli` (headless/debug, .rdp launch)
└── packaging/                  # flatpak manifest, deb/rpm metadata
```

### 3.2 Runtime model

```
 GTK main thread (UI)                     Tokio runtime (per app, multi-thread)
 ───────────────────                      ───────────────────────────────────
 SessionWindow ──InputEvent──────────────▶ SessionTask (one per connection)
   RemoteDisplay widget                      ├─ TCP/TLS/gateway transport
   ◀──FrameUpdate(dirty rects)───────────   ├─ IronRDP ActiveStage
   ◀──SessionEvent(resize, cert, cliprdr…)─ ├─ static channels (cliprdr, rdpdr, rdpsnd)
                                             └─ dynamic channels (egfx, disp, audin, cam)
```

- Communication via `async-channel` (works on both sides: tokio + glib main context).
- Framebuffer is a shared `Arc<Mutex<…>>` (or triple buffer) updated by the session
  task; UI is notified with a coalesced "damage" message and uploads only dirty rects
  once per frame clock tick.
- Each session is fully isolated: a crash/panic in one session task is caught and
  surfaced as a disconnect, not an app crash.
- `rdesk-core` exposes a UI-agnostic API (`Session::connect(config) -> (handle, events)`),
  so the CLI and tests can drive it without GTK.

### 3.3 Data model (rdesk-store)

- `PcProfile` — id, hostname[:port], friendly name, group, account ref, gateway ref,
  display/devices/audio/folders settings, thumbnail path, last-connected.
- `UserAccount` — username, domain, friendly name; password stored in keyring by id.
- `Gateway` — host, port, account ref ("use PC credentials" option), bypass local.
- `Workspace` — feed URL, account, cached resources (desktops/apps + icons).
- `Group` — name, ordering.
- Settings stored as TOML; secrets never on disk in plaintext.

---

## 4. Complete feature list

Priority: **P0** = MVP, **P1** = v1.0 parity, **P2** = post-1.0.

### 4.1 Connection Center (main window)
- [P0] List of saved PCs with friendly name, hostname, and connection status.
- [P0] Add / edit / duplicate / delete PC.
- [P0] Quick connect bar (type a hostname, press Enter).
- [P1] Grid view with **live session thumbnails** (captured on disconnect) and list view toggle.
- [P1] Groups (collapsible sections), drag-and-drop to reorder/move between groups.
- [P1] Search / filter across PCs and workspace resources.
- [P1] Sort by name / last connected.
- [P1] "PCs" and "Workspaces" tabs (sidebar like the Mac app).
- [P1] Context menu: Connect, Edit, Duplicate, Export, Delete.
- [P1] Multi-select + bulk delete/export.
- [P1] Light/dark mode following system (libadwaita).
- [P2] Favorites / pinned PCs.

### 4.2 PC settings (Add/Edit PC dialog)
**General**
- [P0] PC name (hostname or IP, optional `:port`).
- [P0] User account: "Ask when required" or pick/add a saved account.
- [P1] Friendly name, group.
- [P1] Gateway: none / pick / add.
- [P1] Reconnect if the connection is dropped.
- [P1] Connect to an admin session (`/admin` / console).
- [P1] Swap mouse buttons.

**Display**
- [P0] Resolution: Default (fit window) / presets / custom.
- [P0] Start session in full screen.
- [P1] **Update the session resolution on resize** (dynamic resolution, Display Control channel).
- [P1] **Use all monitors** (multi-monitor layout) / choose specific monitors.
- [P1] HiDPI: "Optimize for high-resolution displays" (send desktop scale factor + device scale).
- [P1] Fit session to window vs. 1:1 with scrollbars.
- [P1] Color depth (16 / 24 / 32 bit).
- [P2] Graphics mode override (auto / AVC444 / AVC420 / RemoteFX / bitmap-only).

**Devices & Audio**
- [P0] Clipboard redirection (on/off).
- [P1] Play sound: on this computer / on remote PC / never.
- [P1] Microphone redirection.
- [P1] Printer redirection (via CUPS, using the generic/XPS or "Microsoft Print to PDF"-style driver).
- [P1] Smart card redirection.
- [P2] Camera redirection.
- [P2] Serial/port redirection (low priority).

**Folders**
- [P1] Redirect local folders (pick folders, set name, read-only toggle).
- [P2] Redirect home folder by default.

**Advanced / power user**
- [P1] Custom `.rdp` properties passthrough for unsupported settings.
- [P2] Load balance info / routing token, alternate shell, working directory.
- [P2] Per-connection keyboard layout override.

### 4.3 Session window
- [P0] Render remote desktop with smooth scaling.
- [P0] Keyboard + mouse input (buttons 1–5, wheel).
- [P0] Windowed and fullscreen modes; toggle with shortcut.
- [P0] Disconnect / close session; disconnect reason shown in human terms.
- [P1] Multiple simultaneous sessions, each in its own window (optionally tabs).
- [P1] Session toolbar / header bar in fullscreen (auto-hide, like the Mac app's top bar).
- [P1] **Smooth / high-resolution scrolling** (touchpad → fine-grained wheel deltas, horizontal wheel).
- [P1] Touchpad gestures: pinch zoom (local zoom of the view), two-finger scroll.
- [P1] Cursor: remote cursor shapes (pointer updates), large/colour/alpha cursors.
- [P1] Local zoom / pan of the session view.
- [P1] Auto-reconnect with overlay ("Reconnecting… attempt 2/20").
- [P1] Session info panel: resolution, codec, RTT, bandwidth, frame rate.
- [P1] Screenshot of session to file/clipboard.
- [P2] Touch-screen input (RDP multitouch / MS-RDPEI) on Linux touch devices.
- [P2] Pen input.

### 4.4 Keyboard
- [P0] Scancode-based input (evdev keycodes → RDP scancodes), with extended keys.
- [P0] Keyboard layout negotiation (send client layout id).
- [P1] **System shortcut inhibition** in session (Alt+Tab, Super, etc. go to the remote),
      via GTK `inhibit_system_shortcuts` (Wayland keyboard-shortcuts-inhibit protocol).
- [P1] Configurable key mapping (e.g. Super → Windows key, Ctrl+Alt+End → Ctrl+Alt+Del).
- [P1] "Send keys" menu: Ctrl+Alt+Del, Win, Win+L, Alt+Tab, PrintScreen, etc.
- [P1] Unicode input mode (for IME / compose / non-matching layouts) alongside scancode mode.
- [P1] Lock-key sync (Caps/Num/Scroll lock state on focus).
- [P2] Keyboard shortcut to release grab (e.g. Ctrl+Alt) with on-screen hint.

### 4.5 Clipboard
- [P0] Bidirectional text (CF_UNICODETEXT ↔ UTF-8).
- [P1] Images (DIB/PNG ↔ image/png).
- [P1] HTML / rich text.
- [P1] **File copy/paste** in both directions (FileGroupDescriptorW + file contents streaming,
      `text/uri-list` / portal file transfer on Linux).
- [P2] Clipboard size limits and per-direction policy.

### 4.6 Audio
- [P1] Audio playback (RDPSND + Audio Output Virtual Channel), PCM + AAC/Opus if offered.
- [P1] Microphone input (AUDIN dynamic channel).
- [P1] A/V sync with EGFX video.
- [P2] Per-session volume control in the toolbar.

### 4.7 Graphics / performance
- [P0] Bitmap updates (uncompressed / interleaved / planar RLE).
- [P0] Fast-path output and input.
- [P1] RemoteFX codec.
- [P1] **Graphics Pipeline (RDPGFX / EGFX)** with progressive codec, ClearCodec, planar, AVC420.
- [P1] AVC444 (full-chroma H.264) for crisp text.
- [P1] Hardware decode via VA-API (optional), software fallback.
- [P1] GPU-backed presentation with frame-clock-paced uploads.
- [P1] Network auto-detect & connection-type hints (bandwidth/RTT measurement).
- [P2] UDP transport (MS-RDPEUDP / multitransport) for lower latency.
- [P2] Bulk compression (MPPC / NCRUSH / XCRUSH) where negotiated.

### 4.8 Security & authentication
- [P0] TLS with **certificate verification dialog** (show issuer, fingerprint, validity,
      "Always trust this certificate for this PC"); store trusted fingerprints per host.
- [P0] NLA / CredSSP with NTLM.
- [P1] Kerberos (system krb5 ccache, KDC proxy over gateway).
- [P1] Credentials stored in system keyring (Secret Service); never in plaintext config.
- [P1] Restricted Admin mode / Remote Credential Guard (best effort).
- [P1] Server redirection (Session Broker load balancing / reconnection to correct host).
- [P1] Prompt-for-credentials flow when saved ones fail; "remember" checkbox.
- [P2] Smart card / Windows Hello for Business authentication (PKINIT).
- [P2] Microsoft Entra ID (AAD) RDP authentication (RDSTLS / Entra SSO to Entra-joined PCs).

### 4.9 Gateways
- [P1] Gateways manager (add/edit/delete, credentials or "use PC credentials").
- [P1] RD Gateway over HTTP transport (MS-TSGU, RPC-over-HTTP fallback is P2).
- [P1] RD Gateway WebSocket transport.
- [P2] UDP gateway transport.
- [P2] Gateway messaging (consent/admin messages shown to the user).

### 4.10 Workspaces (Azure Virtual Desktop, Windows 365, RD Web)
- [P2] Subscribe to a workspace by URL or email discovery.
- [P2] Entra ID sign-in via system browser (OAuth2 auth code + PKCE), MFA/Conditional Access.
- [P2] Feed download: desktops and RemoteApps with icons, grouped by workspace.
- [P2] Auto-refresh feeds; manual refresh; unsubscribe.
- [P2] AVD connection flow (gateway + RDP tokens, short-path/UDP when available).
- [P2] Windows 365 Cloud PC actions (restart, restore point) — if APIs allow.

### 4.11 RemoteApp
- [P2] Launch RemoteApp programs (RAIL channel).
- [P2] Each remote window as a separate local window (title, icon, z-order, move/resize).
- [P2] Remote app windows in taskbar/dock, Alt+Tab-able locally.
- [P2] Language bar / IME integration in RemoteApp.

### 4.12 Account & profile management
- [P1] User Accounts manager (add/edit/delete; username, password, friendly name).
- [P1] Import `.rdp` files (drag-and-drop onto the window, or "Open with").
- [P1] Export PC(s) to `.rdp`.
- [P1] Launch directly from `.rdp` file (`rdesk file.rdp`, MIME association).
- [P1] URI scheme handler: `rdp://` and `ms-rd:` (`ms-rd:subscribe?url=…`).
- [P2] Backup/restore all settings (bookmarks + accounts without passwords).
- [P2] Import from Remmina / KRDC profiles.

### 4.13 App preferences
- [P1] Default display/devices settings for new PCs.
- [P1] Grid/list default, thumbnail on/off.
- [P1] Keyboard mapping mode (scancode vs Unicode), shortcut configuration.
- [P1] Logging level and "Open logs folder".
- [P2] Telemetry: none (explicitly).
- [P2] Hardware acceleration toggle.

### 4.14 Desktop integration
- [P1] `.desktop` file, icons, AppStream metainfo.
- [P1] Single-instance app (D-Bus activation); opening a `.rdp` focuses the existing app.
- [P1] Desktop notifications (session disconnected, reconnected).
- [P1] Accessibility: keyboard navigation of the connection center, screen-reader labels.
- [P1] Localization (gettext / fluent).
- [P2] GNOME Shell search provider (search PCs from the overview).
- [P2] Background "running sessions" indicator.

### 4.15 Command line
- [P1] `rdesk <host|file.rdp|uri>` opens a session directly.
- [P2] `rdesk-cli` for headless connect tests, screenshot, `.rdp` validation, debugging.

---

## 5. Roadmap

Rough sizing assumes one experienced Rust developer; parallelize where possible.

### Phase 0 — Spike & foundations (1–2 weeks)
- Install toolchain (`rustup`, stable), set up Cargo workspace, CI (fmt, clippy, test).
- Spike: minimal binary using IronRDP that connects with NLA to a Windows 10/11 VM and
  writes the first frame to a PNG. Confirm EGFX/H.264, cliprdr, rdpdr, rdpsnd, gateway status.
- Spike: GTK4 window that displays a 1920×1080 texture updated at 60 fps from another thread.
- Decide go/no-go on IronRDP vs FreeRDP FFI based on spike results.
- Test lab: Windows 11 Pro VM, Windows Server 2022 with RDS + RD Gateway, xrdp box.

### Phase 1 — MVP (4–6 weeks) → all **P0** items
- `rdesk-core`: connect, TLS, NLA (NTLM), bitmap/fast-path, input, disconnect handling.
- `rdesk-input`: evdev → scancode table, mouse/wheel.
- `app`: connection center (list), add/edit PC, quick connect, session window, fullscreen.
- Certificate trust dialog + trusted-fingerprint store.
- Text clipboard.
- TOML config store.
- **Exit criteria:** daily-drivable for connecting to a Windows PC on a LAN.

### Phase 2 — Display quality & UX (4–6 weeks)
- EGFX pipeline with AVC420, then AVC444; RemoteFX; GPU presentation.
- Dynamic resolution + HiDPI scale factor; multi-monitor.
- Shortcut inhibition, send-keys menu, key mapping, lock-key sync.
- Smooth scrolling, cursor shapes, auto-reconnect overlay, session info panel.
- Grid view with thumbnails, groups, search, sort.

### Phase 3 — Redirection & accounts (4–6 weeks)
- Audio out (RDPSND) + microphone (AUDIN) via PipeWire.
- Clipboard images/HTML/files.
- Drive redirection (RDPDR filesystem), printers, smart cards.
- Accounts manager + keyring; Kerberos.
- `.rdp` import/export, URI handler, CLI launch.

### Phase 4 — Gateways & enterprise (3–5 weeks)
- RD Gateway (HTTP + WebSocket), gateways manager.
- Server redirection / session broker.
- Restricted Admin / Remote Credential Guard.

### Phase 5 — v1.0 release (2–3 weeks)
- Packaging: Flatpak (Flathub), .deb, .rpm, AppImage.
- Accessibility pass, localization infra, docs, metainfo screenshots.
- Fuzzing of PDU parsers we own (`cargo-fuzz`), security review.

### Phase 6 — Post-1.0 (P2)
- Workspaces: AVD / Windows 365 / RD Web feeds with Entra ID auth.
- RemoteApp (RAIL) with integrated windows.
- UDP transport, camera redirection, touch/pen, VA-API.

---

## 6. Testing strategy

- **Unit tests:** scancode tables, `.rdp` parser round-trips, config migrations, clipboard format conversion.
- **Property/fuzz tests:** `.rdp` parser, any PDU encoders/decoders we add (`proptest`, `cargo-fuzz`).
- **Integration tests:** `rdesk-core` against an `xrdp` container in CI (connect, frame, input, clipboard);
  Windows VM matrix run nightly/manually (Win 10, Win 11, Server 2019/2022/2025, with/without NLA, via gateway).
- **UI tests:** screenshot smoke tests under headless Weston/Mutter.
- **Performance budget:** <16 ms frame present at 4K/60 on integrated GPU; input-to-send latency <5 ms;
  memory <300 MB per 4K session.

---

## 7. Risks & mitigations

| Risk | Mitigation |
|---|---|
| IronRDP missing features (gateway, camera, AVD bits) | Phase 0 spike; contribute upstream; implement in own crates; FreeRDP FFI as fallback plan. |
| Wayland limits (global shortcuts, absolute window placement for RemoteApp/multi-mon) | Use shortcut-inhibit protocol; fullscreen-per-monitor for multi-mon; RemoteApp as P2 with best-effort placement. |
| H.264 licensing/patents in distro packages | Use `openh264` (Cisco binary, downloadable) or system codecs; Flatpak codecs extension. |
| AVD/Entra protocols poorly documented | Defer to post-1.0; follow MS-RDP* open specs and FreeRDP's implementation as reference. |
| Keyboard layout edge cases | Scancode mode by default, Unicode fallback, per-PC layout override. |

---

## 8. Immediate next steps

Status 2026-10-14: steps 1–4 done; spike verified against xrdp and Windows 11 Pro with NLA (see `docs/phase0-ironrdp-spike.md`). GTK4 texture spike done (see `docs/phase0-gtk-texture-spike.md`): memory textures hold 60 fps at 1080p; 4K needs the Phase 2 GL/dmabuf path.

Phase 1 progress (2026-10-15): `rdesk-core` session engine, `rdesk-input` key table, `rdesk-cli screenshot`, and the app's quick connect → sign-in → certificate prompt → session window (rendering, keyboard, mouse, wheel, remote cursor, full screen, readable disconnect reasons) work against Windows 11 with NLA. `rdesk-store` keeps saved PCs (add/edit/remove, per-PC NLA switch) and "always trust" certificate fingerprints per host:port as TOML in `$XDG_CONFIG_HOME/rdesk/`. Still to do: text clipboard, keyboard layout id from the local layout.

1. Install Rust: `curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh`.
2. Install system deps (Ubuntu): `sudo apt install libgtk-4-dev libadwaita-1-dev libpipewire-0.3-dev libpcsclite-dev pkg-config clang`.
3. Create the Cargo workspace skeleton from §3.1.
4. Build the Phase 0 IronRDP connect-and-screenshot spike against a test Windows VM.
