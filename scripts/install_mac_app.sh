#!/usr/bin/env bash
# Build toastty in release mode and install it as a minimal .app bundle
# so it's launchable from Spotlight / Launchpad.
#
# Usage:
#   ./scripts/install_mac_app.sh
#
# Env overrides:
#   APP_DIR=/Applications        install location for the .app
#   APP_NAME=Toastty             bundle display name (also names the .app)
#   BUNDLE_ID=com.instacart.toastty
#   SKIP_BUILD=1                 reuse the existing target/release/toastty

set -eu

if [ "$(uname -s)" != "Darwin" ]; then
    printf 'this script only makes sense on macOS\n' >&2
    exit 1
fi

repo_root=$(cd "$(dirname "$0")/.." && pwd)
cd "${repo_root}"

APP_DIR="${APP_DIR:-/Applications}"
APP_NAME="${APP_NAME:-Toastty}"
BUNDLE_ID="${BUNDLE_ID:-com.instacart.toastty}"

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

app_path="${APP_DIR}/${APP_NAME}.app"
contents="${app_path}/Contents"
macos="${contents}/MacOS"
resources="${contents}/Resources"

# The real terminal binary lives OUTSIDE the .app bundle (see the shim comment
# below for why). SUPPORT_DIR can override the location; it is resolved now,
# at install time, and baked into the shim as an absolute path.
support_dir="${SUPPORT_DIR:-${HOME}/Library/Application Support/${APP_NAME}}"
support_bin="${support_dir}/toastty"

printf '==> installing launcher to %s\n' "${app_path}"
printf '==> installing binary to   %s\n' "${support_bin}"
mkdir -p "${macos}" "${resources}" "${support_dir}"
# Drop any stale in-bundle binary from an older install: a window-owning
# binary inside the bundle is exactly what breaks repeat launches.
rm -f "${macos}/toastty"
install -m 0755 "${bin_src}" "${support_bin}"

# Launcher shim (the bundle's CFBundleExecutable).
#
# Goal: a brand-new terminal window on every Spotlight / Launchpad / Finder
# launch. macOS normally treats an .app as single-instance -- launching it
# again just reactivates the process it already associates with the bundle.
#
# Crucially, macOS decides which bundle a GUI process belongs to by walking
# UP that process's executable path to the enclosing .app -- NOT by matching
# CFBundleExecutable. So ANY window-owning binary inside Contents/MacOS/
# registers as this bundle's running instance, and the next launch just
# reactivates it. (An earlier version of this script kept the real `toastty`
# in the bundle and relied on the shim exiting; it didn't work, because the
# detached child -- though named differently from CFBundleExecutable -- still
# lived inside the bundle and so claimed it.)
#
# Fix: the real binary lives OUTSIDE any bundle (${support_bin}). This shim,
# the only executable in the bundle, spawns that external binary fully
# detached and exits immediately. Because the shim is gone a beat later and
# the terminal process is not inside any .app, LaunchServices never considers
# the bundle "running": every launch re-runs the shim and spawns another
# independent, non-bundle-associated terminal + window.
#
# We intentionally do NOT forward "$@": a Spotlight launch carries no useful
# user args, and legacy LaunchServices args (e.g. -psn_0_...) would trip
# toastty's CLI parser. Launch the binary from a real terminal for flags.
#
# NOTE: unquoted heredoc -- ${support_bin} is expanded now, at install time,
# and baked into the shim as an absolute path.
cat > "${macos}/toastty-launch" <<LAUNCH
#!/bin/sh
# Detached background spawn: the child is reparented to launchd when this
# shim exits, keeps its inherited WindowServer connection, and creates its
# own window. The binary lives outside the bundle, so each launch => an
# independent, non-bundle-associated process + window.
"${support_bin}" >/dev/null 2>&1 &
exit 0
LAUNCH
chmod 0755 "${macos}/toastty-launch"

cat > "${contents}/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key>             <string>${APP_NAME}</string>
    <key>CFBundleDisplayName</key>      <string>${APP_NAME}</string>
    <key>CFBundleIdentifier</key>       <string>${BUNDLE_ID}</string>
    <key>CFBundleVersion</key>          <string>${version}</string>
    <key>CFBundleShortVersionString</key><string>${version}</string>
    <key>CFBundleExecutable</key>       <string>toastty-launch</string>
    <key>LSMultipleInstancesProhibited</key><false/>
    <key>CFBundlePackageType</key>      <string>APPL</string>
    <key>LSMinimumSystemVersion</key>   <string>11.0</string>
    <key>NSHighResolutionCapable</key>  <true/>
</dict>
</plist>
PLIST

# Strip the quarantine xattr so Gatekeeper doesn't refuse to launch the
# unsigned binary on first run (covers both the bundle and the external binary).
xattr -dr com.apple.quarantine "${app_path}" "${support_bin}" 2>/dev/null || true

# Nudge Spotlight / LaunchServices so the new bundle is indexed right away.
touch "${app_path}"
/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister \
    -f "${app_path}" >/dev/null 2>&1 || true

printf '==> done. launch via Spotlight: %s\n' "${APP_NAME}"
