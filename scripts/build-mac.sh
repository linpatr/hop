#!/bin/bash
# Builds dist/Hop.app (and bundles the Windows build, if present, so the Mac can hand it out).
set -euo pipefail
cd "$(dirname "$0")/.."
source "$HOME/.cargo/env" 2>/dev/null || true

TARGET_DIR=$(cargo metadata --format-version 1 --no-deps | python3 -c 'import sys,json;print(json.load(sys.stdin)["target_directory"])')
VERSION=$(grep -m1 '^version' Cargo.toml | cut -d'"' -f2)
cargo build --release

APP=dist/Hop.app
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp "$TARGET_DIR/release/hop" "$APP/Contents/MacOS/hop"
[ -f assets/AppIcon.icns ] && cp assets/AppIcon.icns "$APP/Contents/Resources/"

cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleIdentifier</key><string>com.patricklin.hop</string>
  <key>CFBundleName</key><string>Hop</string>
  <key>CFBundleDisplayName</key><string>Hop</string>
  <key>CFBundleExecutable</key><string>hop</string>
  <key>CFBundleIconFile</key><string>AppIcon</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>${VERSION}</string>
  <key>CFBundleVersion</key><string>${VERSION}</string>
  <key>LSMinimumSystemVersion</key><string>13.0</string>
  <key>LSApplicationCategoryType</key><string>public.app-category.utilities</string>
  <key>NSHighResolutionCapable</key><true/>
  <key>NSLocalNetworkUsageDescription</key><string>Hop finds and connects to your other computers on this network.</string>
</dict>
</plist>
PLIST

WIN_EXE="$TARGET_DIR/x86_64-pc-windows-gnu/release/hop.exe"
if [ -f "$WIN_EXE" ]; then
  mkdir -p "$APP/Contents/Resources/windows"
  cp "$WIN_EXE" "$APP/Contents/Resources/windows/Hop.exe"
  cp windows/install.ps1 "$APP/Contents/Resources/windows/install.ps1"
  echo "bundled Windows build ($(du -h "$WIN_EXE" | cut -f1))"
fi

IDENTITY="${HOP_SIGN_IDENTITY:-$(security find-identity -v -p codesigning | awk -F'"' '/Apple Development|Developer ID Application/{print $2; exit}')}"
codesign --force --deep --sign "${IDENTITY:--}" "$APP"
echo "built $APP (signed with: ${IDENTITY:-ad-hoc})"
