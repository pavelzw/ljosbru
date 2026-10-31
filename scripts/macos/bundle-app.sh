#!/usr/bin/env bash
# Build ljosbru.app (the decode GUI) so macOS attributes Screen Recording and
# Accessibility permissions to ljosbru instead of the terminal app.
#
# Set CODESIGN_IDENTITY to a stable signing identity (see
# `security find-identity -v -p codesigning`) to keep granted permissions
# across rebuilds. The default ad-hoc signature changes on every build, so
# macOS asks again after each rebuild.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
identity="${CODESIGN_IDENTITY:--}"
version="$(sed -n 's/^version = "\(.*\)"$/\1/p' "$repo_root/Cargo.toml" | head -n 1)"

cargo build --release --features gui --bin ljosbru-gui --manifest-path "$repo_root/Cargo.toml"

app="$repo_root/target/release/ljosbru.app"
rm -rf "$app"
mkdir -p "$app/Contents/MacOS"
cp "$repo_root/target/release/ljosbru-gui" "$app/Contents/MacOS/ljosbru-gui"

cat >"$app/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleExecutable</key>
  <string>ljosbru-gui</string>
  <key>CFBundleIdentifier</key>
  <string>io.github.pavelzw.ljosbru</string>
  <key>CFBundleName</key>
  <string>ljosbru</string>
  <key>CFBundlePackageType</key>
  <string>APPL</string>
  <key>CFBundleShortVersionString</key>
  <string>${version}</string>
  <key>CFBundleVersion</key>
  <string>${version}</string>
  <key>NSHighResolutionCapable</key>
  <true/>
</dict>
</plist>
EOF

codesign --force --sign "$identity" --identifier io.github.pavelzw.ljosbru "$app"
codesign --verify --strict "$app"

echo "Built $app (signed with: $identity)"
echo "Open it with: open $app"
