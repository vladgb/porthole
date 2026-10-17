# Packaging

## Flatpak

`flatpak/io.github.vladgb.Porthole.yml` builds Porthole on the GNOME 51 runtime. The build
is offline: every crate is listed in `flatpak/cargo-sources.json`, which has to be regenerated
whenever `Cargo.lock` changes:

```sh
scripts/flatpak-cargo-sources.py
```

Build and install it for your user (needs `org.flatpak.Builder` from Flathub; the GNOME SDK
and its Rust and LLVM extensions are installed as needed):

```sh
flatpak install --user flathub org.flatpak.Builder
flatpak run org.flatpak.Builder --user --install --install-deps-from=flathub --force-clean \
    --state-dir=.flatpak-builder packaging/flatpak/build-dir \
    packaging/flatpak/io.github.vladgb.Porthole.yml
flatpak run io.github.vladgb.Porthole
```

### Flathub

`flathub/` holds what goes into the Flathub repository: the same manifest, but building the
tagged release from GitHub instead of this folder, and the crate list for that tag. For a new
release:

1. Add the release to `app/data/io.github.vladgb.Porthole.metainfo.xml`, point the screenshot
   links at the new tag, then tag and push it (`git tag -a vX.Y.Z && git push origin vX.Y.Z`).
2. In `flathub/io.github.vladgb.Porthole.yml`, update `tag` and `commit`
   (`git rev-parse vX.Y.Z^{commit}`), and copy the tag's crate list:
   `git show vX.Y.Z:packaging/flatpak/cargo-sources.json > packaging/flathub/cargo-sources.json`.
3. Check it builds and passes Flathub's linter:

   ```sh
   flatpak run org.flatpak.Builder --user --force-clean --repo=packaging/flathub/repo \
       packaging/flathub/build-dir packaging/flathub/io.github.vladgb.Porthole.yml
   flatpak run --command=flatpak-builder-lint org.flatpak.Builder repo packaging/flathub/repo
   ```

4. Copy both files into the `flathub/io.github.vladgb.Porthole` repository (or, for the first
   submission, into a pull request against `flathub/flathub`'s `new-pr` branch).

Notes on the sandbox:

- **H.264:** OpenH264 may only be distributed by Cisco, so the manifest declares it as
  *extra data*: Flatpak downloads Cisco's binary when Porthole is installed (x86_64 only for
  now). Without it Porthole falls back to the lossless picture mode.
- **Sound and microphone** use the PipeWire socket (`xdg-run/pipewire-0`).
- **Cameras** come through the camera portal, which asks the user for access the first time a
  PC that uses the camera connects.
- **Passwords** go to the secret portal; saved PCs and settings live in
  `~/.var/app/io.github.vladgb.Porthole/`, separate from a non-Flatpak install.
- **Shared folders** chosen in Porthole are granted through the document portal.
