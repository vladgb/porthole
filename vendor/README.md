# Vendored crates

Patched copies of upstream crates, wired in through `[patch.crates-io]` in the workspace
`Cargo.toml`. Keep each patch minimal and drop it once upstream has the change.

## ironrdp-connector 0.10.0

Adds `Config::enable_gfx`, which sets `SUPPORT_DYN_VC_GFX_PROTOCOL` in the client's early
capability flags. Without it Windows never opens the Graphics Pipeline (EGFX) channel, so
H.264 and the other EGFX codecs can't be used. Upstream 0.10.0 has no way to set the flag.

Adds `Config::enable_audio_capture`, which sets `INFO_AUDIOCAPTURE` in the Client Info PDU.
Without it Windows never opens the AUDIO_INPUT channel, so the microphone can't be redirected.

Adds `Config::enable_rail`, which asks for a RemoteApp session: it sets `INFO_RAIL` instead of
`INFO_MAXIMIZESHELL` in the Client Info PDU, and `INFO_HIDEF_RAIL_SUPPORTED` too when
`enable_gfx` is set (enhanced RemoteApp: one Graphics Pipeline surface per window). It adds the
Remote Programs and Window List capability sets to the Confirm Active PDU, and announces the
`rail` channel with `INITIALIZED | ENCRYPT_RDP | COMPRESS_RDP | SHOW_PROTOCOL` (as mstsc and
FreeRDP do): with no options Windows opens the channel and handshakes but silently ignores the
request to start the program. Upstream 0.10.0 can't set any of these.

Also drops one `#[expect(single_use_lifetimes)]` that is unfulfilled when built as a path dependency.

## ironrdp-rdpsnd 0.9.0

Fixes audio format negotiation. Upstream builds the client's format list from a `HashSet`
intersection, so the order is random, yet the server names a wave's format by its index in that
list, and the handler is called with that index as if it indexed the handler's own list. With
two formats (48 kHz and 44.1 kHz) Windows' 44.1 kHz audio was often played as 48 kHz: too fast,
with a gap whenever the buffer ran dry. The patch keeps the handler's order, remembers the list
sent, and translates the server's index to the handler's before calling `wave`.

## ironrdp-pdu 0.9.0

Fixes ClearCodec's short V-bar cache miss (MS-RDPEGFX 2.2.4.1.1.2.1.1.3): upstream reads
`shortVBarYOn` and `shortVBarYOff` from each other's bits, so Windows' ClearCodec data fails to
decode ("shortVBarYOff < shortVBarYOn", then V-bar cache misses) and parts of the picture stay
empty. The fields are, from the low bit: `shortVBarYOn` (8 bits), `shortVBarYOff` (6 bits), as in
FreeRDP. It matters for RemoteApp without H.264, where Windows uses ClearCodec for windows.
