#!/usr/bin/env python3
"""Writes the Flatpak sources for every crates.io dependency in Cargo.lock.

Flatpak builds run offline: each crate is downloaded by flatpak-builder (checked against the
lockfile's checksum) into a vendor directory that Cargo is pointed at. Equivalent to
flatpak-builder-tools' flatpak-cargo-generator for a lockfile without git dependencies, and
needs nothing beyond Python 3.11.

Usage: scripts/flatpak-cargo-sources.py [Cargo.lock] [output.json]
"""

import json
import sys
import tomllib
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
CRATES_IO = "registry+https://github.com/rust-lang/crates.io-index"
VENDOR = "cargo/vendor"


def main() -> int:
    lock = Path(sys.argv[1]) if len(sys.argv) > 1 else ROOT / "Cargo.lock"
    out = Path(sys.argv[2]) if len(sys.argv) > 2 else ROOT / "packaging/flatpak/cargo-sources.json"
    packages = tomllib.loads(lock.read_text())["package"]

    sources = []
    for package in packages:
        source = package.get("source")
        if source is None:
            continue  # this workspace, or a crate patched in from vendor/
        if source != CRATES_IO:
            print(f"error: {package['name']} comes from {source}; only crates.io is supported", file=sys.stderr)
            return 1
        name, version, checksum = package["name"], package["version"], package["checksum"]
        dest = f"{VENDOR}/{name}-{version}"
        sources.append(
            {
                "type": "archive",
                "archive-type": "tar-gzip",
                "url": f"https://static.crates.io/crates/{name}/{name}-{version}.crate",
                "sha256": checksum,
                "dest": dest,
            }
        )
        # Cargo only accepts vendored crates with a checksum file; an empty file list skips
        # per-file checks (the archive itself was verified above).
        sources.append(
            {
                "type": "inline",
                "contents": json.dumps({"package": checksum, "files": {}}),
                "dest": dest,
                "dest-filename": ".cargo-checksum.json",
            }
        )

    sources.append(
        {
            "type": "inline",
            "contents": (
                '[source.vendored-sources]\n'
                f'directory = "{VENDOR}"\n'
                '\n'
                '[source.crates-io]\n'
                'replace-with = "vendored-sources"\n'
            ),
            "dest": "cargo",
            "dest-filename": "config.toml",
        }
    )
    out.parent.mkdir(parents=True, exist_ok=True)
    out.write_text(json.dumps(sources, indent=2) + "\n")
    print(f"{len(sources) // 2} crates -> {out.relative_to(ROOT) if out.is_relative_to(ROOT) else out}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
