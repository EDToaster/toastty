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

printf '==> installing to %s\n' "${app_path}"
mkdir -p "${macos}" "${resources}"
install -m 0755 "${bin_src}" "${macos}/toastty"

# Launcher shim (see CFBundleExecutable below).
#
# macOS treats an .app bundle as single-instance: launching it again from
# Spotlight / Launchpad / Finder reactivates the running process instead of
# starting a second one. To get a fresh terminal on every launch, the
# bundle's main executable is this shim, which spawns the real `toastty`
# fully detached and exits immediately. Because the shim (the designated
# CFBundleExecutable) is no longer running a beat later, LaunchServices has
# nothing to reactivate and the next launch runs the shim again -> another
# terminal. The detached instances run `toastty`, a *different* executable
# name than the CFBundleExecutable, so they aren't mistaken for the app's
# main process either.
#
# We intentionally do NOT forward "$@": a Spotlight launch carries no useful
# user args, and legacy LaunchServices args (e.g. -psn_0_...) would trip
# toastty's CLI parser. Launch the binary from a real terminal for flags.
cat > "${macos}/toastty-launch" <<'LAUNCH'
#!/bin/sh
dir=$(dirname "$0")
# Detached background spawn: the child is reparented to launchd when this
# shim exits, keeps its inherited WindowServer connection, and creates its
# own window. Each launch => an independent process + window.
"${dir}/toastty" >/dev/null 2>&1 &
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
# unsigned binary on first run.
xattr -dr com.apple.quarantine "${app_path}" 2>/dev/null || true

# Nudge Spotlight / LaunchServices so the new bundle is indexed right away.
touch "${app_path}"
/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister \
    -f "${app_path}" >/dev/null 2>&1 || true

printf '==> done. launch via Spotlight: %s\n' "${APP_NAME}"
