<p align="center">
  <img src="app/data/icons/hicolor/scalable/apps/io.github.vladgb.Porthole.svg" width="128" alt="">
</p>

# Porthole

A native Remote Desktop (RDP) client for the Linux desktop, built with GTK 4 and libadwaita.
Connect to Windows PCs and use them as if you were sitting in front of them.

> **Status:** early, but used daily. Tested against Windows 11. Expect rough edges, and
> please report what you find.

## Features

- **Saved PCs** in a grid or list with thumbnails of the last session, groups, search and
  sorting. Passwords are kept in the desktop keyring.
- **Sign-in** with Network Level Authentication (NTLM, or Kerberos for domain accounts) and a
  clear prompt for untrusted certificates.
- **Picture:** H.264 through the Graphics Pipeline when OpenH264 is installed, or a lossless
  mode for sharp text and exact colours (per PC). The desktop follows the window size and
  HiDPI scale.
- **Full screen** on one monitor or across all of them, with keyboard shortcuts sent to the
  PC and a small corner button to leave.
- **Clipboard:** text, images and files, both ways.
- **Sound** from the PC, and your **microphone** and **webcam** for calls on the PC.
- **Shared folders:** local folders appear on the PC as drives.
- Automatic reconnection after a dropped network connection.

Planned: printers, smart cards, opening `.rdp` files and `rdp://` links, and packages
(Flatpak first). RD Gateway may follow later.

## Building

You need Rust 1.88 or newer and these development packages:

| | Ubuntu / Debian | Fedora |
|---|---|---|
| GTK 4.16+ and libadwaita 1.5+ | `libgtk-4-dev libadwaita-1-dev` | `gtk4-devel libadwaita-devel` |
| PipeWire (sound, microphone, camera) | `libpipewire-0.3-dev` | `pipewire-devel` |
| Build tools | `clang pkg-config` | `clang pkgconf-pkg-config` |

```sh
cargo build --release -p porthole
target/release/porthole
```

For H.264, install Cisco's OpenH264 (`libopenh264-8` on Ubuntu, `openh264` from the
fedora-cisco-openh264 repository on Fedora). It's loaded at run time; without it Porthole uses
the lossless mode.

To get Porthole in your app grid and dock with its name and icon, install the launcher for
your user (undo with `--uninstall`):

```sh
scripts/install-local.sh
```

## Usage

Type a PC name or IP address and press Connect, or add the PC to keep it. From a terminal:

```sh
porthole pc.example.com -u 'CONTOSO\alice'
```

Set `PORTHOLE_LOG=debug` to see what's going on, for example when reporting a problem.

## Project layout

| Path | What it is |
|---|---|
| `app/` | The GTK application |
| `crates/porthole-core/` | Session engine: connection, sign-in, graphics, input and channels (no UI) |
| `crates/porthole-store/` | Saved PCs, trusted certificates, settings and keyring |
| `crates/porthole-input/` | Keyboard layouts and scancodes |
| `crates/porthole-audio/`, `crates/porthole-camera/` | Sound, microphone and webcam through PipeWire |
| `cli/` | `porthole-cli`, a headless client for tests and debugging |
| `vendor/` | Patched IronRDP crates (see [`vendor/README.md`](vendor/README.md)) |

## Acknowledgements

Porthole is built on [IronRDP](https://github.com/Devolutions/IronRDP). FreeRDP's sources were
a valuable reference for how Windows behaves in practice.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.
