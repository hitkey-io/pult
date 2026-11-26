#!/bin/sh
# Publish a built and notarized installer: a GitHub release, then latest.json on the gh-pages branch, which
# GitHub Pages serves as https://pult.hitkey.io/latest.json — what Pult reads to find updates.
#   ./scripts/publish-update.sh   — version from Cargo.toml, release notes from scripts/notes/<version>.txt
# Before it: commit and push, then SIGN_IDENTITY=… NOTARY_PROFILE=… ./scripts/dmg.sh
set -eu
cd "$(dirname "$0")/.."
REPO=hitkey-io/pult
VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
DMG="target/release/Pult-$VERSION.dmg"
NOTES="scripts/notes/$VERSION.txt"
[ -f "$DMG" ] || { echo "no $DMG — run scripts/dmg.sh first"; exit 1; }
[ -f "$NOTES" ] || { echo "no $NOTES — release notes, one line per item"; exit 1; }
spctl -a -t open --context context:primary-signature "$DMG" >/dev/null 2>&1 || { echo "$DMG is not notarized — build with SIGN_IDENTITY and NOTARY_PROFILE"; exit 1; }
gh release create "v$VERSION" "$DMG" --repo "$REPO" --target "$(git rev-parse HEAD)" --title "Pult $VERSION" --notes-file "$NOTES"
PAGES=$(mktemp -d)
git clone --quiet --depth 1 --branch gh-pages "https://github.com/$REPO.git" "$PAGES"
SHA=$(shasum -a 256 "$DMG" | cut -d' ' -f1)
python3 -I - "$PAGES/latest.json" "$VERSION" "$SHA" "$(stat -f %z "$DMG")" "https://github.com/$REPO/releases/download/v$VERSION/Pult-$VERSION.dmg" "$NOTES" <<'PY'
import json, sys
out, version, sha256, size, url, notes = sys.argv[1:]
lines = [line.strip() for line in open(notes, encoding="utf-8") if line.strip()]
json.dump({"version": version, "sha256": sha256, "size": int(size), "url": url, "notes": lines}, open(out, "w", encoding="utf-8"), ensure_ascii=False, indent=2)
PY
git -C "$PAGES" add latest.json
git -C "$PAGES" -c user.name="$(git config user.name)" -c user.email="$(git config user.email)" commit --quiet -m "chore(release): publish $VERSION"
git -C "$PAGES" push --quiet origin gh-pages
rm -rf "$PAGES"
echo "published $VERSION"
