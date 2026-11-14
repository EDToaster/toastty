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

# The real binary is the bundle's executable. That makes the terminal
# process belong to the bundle, so the Dock shows the app's icon and name,
# and pinning the Dock tile pins the app (not a bare Unix executable).
#
# macOS treats a bundle as single-instance: relaunching it while it runs
# (Dock click, Spotlight, Finder) sends the running process a "reopen"
# Apple Event instead of starting a new one. toastty handles that event by
# starting a fresh instance with `open -n`, so every launch still opens a
# new window. See crates/toastty-window/src/reopen.rs.
#
# Older installs put a launcher shim (toastty-launch) in the bundle and
# the real binary outside it, in ~/Library/Application Support/<name>.
# The detached process then had no bundle, so the Dock showed the generic
# "exec" icon and a pinned tile pointed at the raw binary. Remove both.
legacy_support_dir="${SUPPORT_DIR:-${HOME}/Library/Application Support/${APP_NAME}}"

printf '==> installing %s\n' "${app_path}"
mkdir -p "${macos}" "${resources}"
rm -f "${macos}/toastty-launch"
if [ -f "${legacy_support_dir}/toastty" ]; then
    printf '==> removing old out-of-bundle binary %s\n' "${legacy_support_dir}/toastty"
    rm -f "${legacy_support_dir}/toastty"
    rmdir "${legacy_support_dir}" 2>/dev/null || true
fi
install -m 0755 "${bin_src}" "${macos}/toastty"

# App icon, pre-rendered from share/icons/toastty.svg by
# scripts/render_icons.py. Info.plist points at it via CFBundleIconFile.
install -m 0644 share/icons/toastty.icns "${resources}/toastty.icns"

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
    <key>CFBundleExecutable</key>       <string>toastty</string>
    <key>CFBundleIconFile</key>         <string>toastty</string>
    <key>LSMultipleInstancesProhibited</key><false/>
    <key>LSArchitecturePriority</key>
    <array>
        <string>arm64</string>
        <string>x86_64</string>
    </array>
    <key>CFBundlePackageType</key>      <string>APPL</string>
    <key>LSMinimumSystemVersion</key>   <string>11.0</string>
    <key>NSHighResolutionCapable</key>  <true/>
</dict>
</plist>
PLIST

# Ad-hoc sign the whole bundle so its signature covers Info.plist and the
# icon, not just the binary (the linker signs only the Mach-O). Then strip
# the quarantine xattr so Gatekeeper doesn't refuse the first launch.
codesign --force --sign - "${app_path}" >/dev/null 2>&1 || true
xattr -dr com.apple.quarantine "${app_path}" 2>/dev/null || true

# Nudge Spotlight / LaunchServices so the new bundle is indexed right away.
touch "${app_path}"
/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister \
    -f "${app_path}" >/dev/null 2>&1 || true

printf '==> done. launch via Spotlight: %s\n' "${APP_NAME}"
