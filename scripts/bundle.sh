#!/bin/sh
# Builds Pult.app (debug by default; `release` as the argument) with the Icon Composer icon (assets/AppIcon.icon) via actool.
set -eu
cd "$(dirname "$0")/.."
PROFILE="${1:-debug}"
if [ "$PROFILE" = release ]; then
  # A release must not carry the paths of the computer that built it (panic messages and C sources keep them).
  export RUSTFLAGS="--remap-path-prefix=$HOME=/build --remap-path-prefix=$PWD=/pult"
  export CFLAGS="-ffile-prefix-map=$HOME=/build -ffile-prefix-map=$PWD=/pult"
  export CXXFLAGS="$CFLAGS"
  cargo build --release
  if grep -a -q -F "$HOME/" target/release/pult; then echo "the binary still holds paths from this computer"; exit 1; fi
else
  cargo build
fi
APP="target/$PROFILE/Pult.app"
FINAL="$APP"
# Build into a temporary folder and swap the whole bundle: LaunchServices remembers a half-built bundle
# (no Info.plist), and the Dock then shows an empty icon.
APP="target/$PROFILE/.Pult.app.tmp"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp "target/$PROFILE/pult" "$APP/Contents/MacOS/pult"
VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
xcrun actool assets/AppIcon.icon \
  --compile "$APP/Contents/Resources" \
  --platform macosx --minimum-deployment-target 14.0 \
  --app-icon AppIcon --include-all-app-icons \
  --output-partial-info-plist "target/$PROFILE/icon-partial.plist" \
  --output-format human-readable-text --errors
cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>Pult</string>
  <key>CFBundleDisplayName</key><string>Pult</string>
  <key>CFBundleIdentifier</key><string>io.hitkey.pult</string>
  <key>CFBundleExecutable</key><string>pult</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleVersion</key><string>$VERSION</string>
  <key>CFBundleShortVersionString</key><string>$VERSION</string>
  <key>CFBundleIconName</key><string>AppIcon</string>
  <key>CFBundleIconFile</key><string>AppIcon</string>
  <key>LSMinimumSystemVersion</key><string>14.0</string>
  <key>NSHighResolutionCapable</key><true/>
  <key>LSApplicationCategoryType</key><string>public.app-category.developer-tools</string>
</dict>
</plist>
PLIST
# Signing: with a Developer ID (SIGN_IDENTITY) use the hardened runtime and a timestamp, as Apple
# notarization requires; without it sign ad hoc, which only works on this Mac.
if [ -n "${SIGN_IDENTITY:-}" ]; then
  codesign --force --options runtime --timestamp --sign "$SIGN_IDENTITY" "$APP"
else
  codesign --force --sign - "$APP"
fi
rm -rf "$FINAL"
mv "$APP" "$FINAL"
APP="$FINAL"
# The bundle was recreated in place: refresh its LaunchServices record, or the Dock shows an empty icon.
touch "$APP"
/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister -f "$APP" >/dev/null 2>&1 || true
echo "$APP"
