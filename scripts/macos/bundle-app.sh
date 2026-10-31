#!/usr/bin/env bash
# Build ljosbru-decode.app so macOS attributes Screen Recording and
# Accessibility permissions to ljosbru-decode instead of the terminal app.
#
# Set CODESIGN_IDENTITY to a stable signing identity (see
# `security find-identity -v -p codesigning`) to keep granted permissions
# across rebuilds. The default ad-hoc signature changes on every build, so
# macOS asks again after each rebuild.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
identity="${CODESIGN_IDENTITY:--}"
version="$(sed -n 's/^version = "\(.*\)"$/\1/p' "$repo_root/Cargo.toml" | head -n 1)"

cargo build --release --features decode --bin ljosbru-decode --manifest-path "$repo_root/Cargo.toml"

app="$repo_root/target/release/ljosbru-decode.app"
rm -rf "$app"
mkdir -p "$app/Contents/MacOS"
cp "$repo_root/target/release/ljosbru-decode" "$app/Contents/MacOS/ljosbru-decode"

cat >"$app/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleExecutable</key>
  <string>ljosbru-decode</string>
  <key>CFBundleIdentifier</key>
  <string>io.github.pavelzw.ljosbru-decode</string>
  <key>CFBundleName</key>
  <string>ljosbru-decode</string>
  <key>CFBundlePackageType</key>
  <string>APPL</string>
  <key>CFBundleShortVersionString</key>
  <string>${version}</string>
  <key>CFBundleVersion</key>
  <string>${version}</string>
  <key>LSUIElement</key>
  <true/>
</dict>
</plist>
EOF

codesign --force --sign "$identity" --identifier io.github.pavelzw.ljosbru-decode "$app"
codesign --verify --strict "$app"

echo "Built $app (signed with: $identity)"
echo "Run it with: $repo_root/scripts/macos/ljosbru-decode-app [ljosbru-decode args...]"
