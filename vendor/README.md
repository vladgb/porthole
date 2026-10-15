# Vendored crates

Patched copies of upstream crates, wired in through `[patch.crates-io]` in the workspace
`Cargo.toml`. Keep each patch minimal and drop it once upstream has the change.

## ironrdp-connector 0.10.0

Adds `Config::enable_gfx`, which sets `SUPPORT_DYN_VC_GFX_PROTOCOL` in the client's early
capability flags. Without it Windows never opens the Graphics Pipeline (EGFX) channel, so
H.264 and the other EGFX codecs can't be used. Upstream 0.10.0 has no way to set the flag.
Also drops one `#[expect(single_use_lifetimes)]` that is unfulfilled when built as a path dependency.
