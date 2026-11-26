#!/bin/sh
# macOS installer: release build of Pult.app → target/release/Pult-<version>.dmg
# (a window with the app and an Applications shortcut: drag to install).
#
# Without notarization a downloaded copy shows "Apple could not verify…" on first launch.
# To notarize, set both variables:
#   SIGN_IDENTITY="Developer ID Application: <name> (<team>)"   # certificate in the login keychain
#   NOTARY_PROFILE=<profile>   # once: xcrun notarytool store-credentials <profile>
# Without them the installer is signed ad hoc and not notarized.
set -eu
cd "$(dirname "$0")/.."
./scripts/bundle.sh release
VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
STAGE="target/release/dmg"
OUT="target/release/Pult-$VERSION.dmg"
rm -rf "$STAGE" "$OUT"
mkdir -p "$STAGE"
cp -R target/release/Pult.app "$STAGE/Pult.app"
ln -s /Applications "$STAGE/Applications"
hdiutil create -volname "Pult $VERSION" -srcfolder "$STAGE" -fs HFS+ -format UDZO -ov "$OUT" >/dev/null
rm -rf "$STAGE"
if [ -n "${SIGN_IDENTITY:-}" ]; then
  codesign --force --timestamp --sign "$SIGN_IDENTITY" "$OUT"
  if [ -n "${NOTARY_PROFILE:-}" ]; then
    xcrun notarytool submit "$OUT" --keychain-profile "$NOTARY_PROFILE" --wait
    xcrun stapler staple "$OUT"
  fi
fi
echo "$OUT"
