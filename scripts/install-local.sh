#!/bin/sh
# Installs the launcher and icons for the current user, pointing at a binary built in this
# checkout, so GNOME shows Porthole with its name and icon (app grid, dock, Alt+Tab).
# Undo with: scripts/install-local.sh --uninstall
set -eu

APP_ID=io.github.vladgb.Porthole
ROOT=$(cd "$(dirname "$0")/.." && pwd)
DATA=${XDG_DATA_HOME:-$HOME/.local/share}
DESKTOP=$DATA/applications/$APP_ID.desktop
ICON=$DATA/icons/hicolor/scalable/apps/$APP_ID.svg
SYMBOLIC=$DATA/icons/hicolor/symbolic/apps/$APP_ID-symbolic.svg

refresh() {
    update-desktop-database -q "$DATA/applications" 2>/dev/null || true
    gtk-update-icon-cache -q -t -f "$DATA/icons/hicolor" 2>/dev/null || true
}

if [ "${1:-}" = "--uninstall" ]; then
    rm -f "$DESKTOP" "$ICON" "$SYMBOLIC"
    refresh
    echo "Removed the Porthole launcher and icons."
    exit 0
fi

BINARY=$ROOT/target/release/porthole
if [ ! -x "$BINARY" ]; then
    echo "Build it first: cargo build --release -p porthole" >&2
    exit 1
fi

mkdir -p "$(dirname "$DESKTOP")" "$(dirname "$ICON")" "$(dirname "$SYMBOLIC")"
# .rdp files and rdp:// links aren't handled yet, so don't claim them.
sed -e "s|^Exec=porthole|Exec=$BINARY|" -e '/^MimeType=/d' \
    "$ROOT/app/data/$APP_ID.desktop" > "$DESKTOP"
cp "$ROOT/app/data/icons/hicolor/scalable/apps/$APP_ID.svg" "$ICON"
cp "$ROOT/app/data/icons/hicolor/symbolic/apps/$APP_ID-symbolic.svg" "$SYMBOLIC"
refresh
echo "Installed the Porthole launcher ($DESKTOP)."
