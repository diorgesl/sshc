#!/usr/bin/env bash
# Monta dist/sshc.app (bundle macOS) a partir do build release.
set -euo pipefail
cd "$(dirname "$0")/.."

APP=dist/sshc.app
VERSION=$(grep -m1 '^version' Cargo.toml | cut -d'"' -f2)
BIN=target/release/sshc-gui

echo "== build release =="
cargo build --release -p sshc-gui

echo "== ícone =="
[ -f assets/icon.icns ] || python3 scripts/make_icon.py assets/icon.icns

echo "== bundle =="
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp "$BIN" "$APP/Contents/MacOS/sshc"
cp assets/icon.icns "$APP/Contents/Resources/icon.icns"

cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>sshc</string>
  <key>CFBundleDisplayName</key><string>sshc</string>
  <key>CFBundleIdentifier</key><string>dev.sshc.client</string>
  <key>CFBundleExecutable</key><string>sshc</string>
  <key>CFBundleIconFile</key><string>icon</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>${VERSION}</string>
  <key>CFBundleVersion</key><string>1</string>
  <key>LSMinimumSystemVersion</key><string>11.0</string>
  <key>LSApplicationCategoryType</key><string>public.app-category.developer-tools</string>
  <key>NSHighResolutionCapable</key><true/>
</dict>
</plist>
PLIST

printf 'APPL????' > "$APP/Contents/PkgInfo"

echo "== assinatura ad-hoc =="
codesign --force --sign - "$APP" >/dev/null 2>&1 || echo "aviso: não consegui assinar (segue sem assinatura)"

echo "== DMG =="
DMG=dist/sshc-macos-arm64.dmg
DMG_STAGE=dist/dmg
rm -rf "$DMG_STAGE" "$DMG"
mkdir -p "$DMG_STAGE"
# ditto preserva a assinatura e os metadados do bundle
ditto "$APP" "$DMG_STAGE/sshc.app"
ln -s /Applications "$DMG_STAGE/Applications"
hdiutil create -volname "sshc" -srcfolder "$DMG_STAGE" -ov -format UDZO "$DMG" >/dev/null
rm -rf "$DMG_STAGE"

echo "pronto: $APP e $DMG"
