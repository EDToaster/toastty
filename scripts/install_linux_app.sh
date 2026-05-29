#!/usr/bin/env bash
# Build toastty in release mode and install it as a freedesktop .desktop
# entry so it's launchable from krunner / the application menu.
#
# Usage:
#   ./scripts/install_linux_app.sh
#
# Env overrides:
#   BIN_DIR=$HOME/.local/bin                    install location for the binary
#   APP_DIR=$HOME/.local/share/applications     install location for the .desktop
#   APP_NAME=Toastty                            display name shown in krunner
#   SKIP_BUILD=1                                reuse the existing release binary

set -eu

if [ "$(uname -s)" != "Linux" ]; then
    printf 'this script only makes sense on Linux\n' >&2
    exit 1
fi

repo_root=$(cd "$(dirname "$0")/.." && pwd)
cd "${repo_root}"

BIN_DIR="${BIN_DIR:-${HOME}/.local/bin}"
APP_DIR="${APP_DIR:-${HOME}/.local/share/applications}"
APP_NAME="${APP_NAME:-Toastty}"

version=$(awk -F\" '/^version = / { print $2; exit }' Cargo.toml)
version="${version:-0.1.0}"

if [ -z "${SKIP_BUILD:-}" ]; then
    printf '==> building release binary\n'
    cargo build --release -p toastty
fi

# Ask cargo where it actually put things — respects CARGO_TARGET_DIR,
# .cargo/config.toml [build] target-dir, etc.
target_dir=$(cargo metadata --format-version=1 --no-deps \
    | python3 -c 'import sys,json;print(json.load(sys.stdin)["target_directory"])')
bin_src="${target_dir}/release/toastty"
if [ ! -x "${bin_src}" ]; then
    printf 'release binary not found at %s\n' "${bin_src}" >&2
    exit 1
fi

bin_dst="${BIN_DIR}/toastty"
desktop_file="${APP_DIR}/toastty.desktop"

printf '==> installing binary to %s\n' "${bin_dst}"
mkdir -p "${BIN_DIR}"
install -m 0755 "${bin_src}" "${bin_dst}"

printf '==> writing desktop entry to %s\n' "${desktop_file}"
mkdir -p "${APP_DIR}"
cat > "${desktop_file}" <<DESKTOP
[Desktop Entry]
Type=Application
Version=1.0
Name=${APP_NAME}
GenericName=Terminal
Comment=A fast terminal emulator
Exec=${bin_dst}
Icon=utilities-terminal
Categories=System;TerminalEmulator;
Keywords=terminal;shell;console;
StartupNotify=true
Terminal=false
DESKTOP

# Refresh the desktop database so krunner / the menu pick up the new entry
# right away instead of after the next cache rebuild.
if command -v update-desktop-database >/dev/null 2>&1; then
    update-desktop-database "${APP_DIR}" >/dev/null 2>&1 || true
fi

printf '==> done. launch via krunner (Alt+Space): %s\n' "${APP_NAME}"

case ":${PATH}:" in
    *":${BIN_DIR}:"*) ;;
    *) printf 'note: %s is not on your PATH; add it to run `toastty` from a shell\n' "${BIN_DIR}" >&2 ;;
esac
