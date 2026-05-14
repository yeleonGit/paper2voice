#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
RELEASE_DIR="$ROOT_DIR/release/one-click-apple-silicon"
RUNTIME_DIR="$ROOT_DIR/runtime"

cd "$ROOT_DIR"

if [[ ! -x ".venv/bin/python" ]]; then
  echo "Missing .venv/bin/python. Run: uv pip install --python .venv/bin/python -r scripts/requirements-kokoro.txt"
  exit 1
fi

PYTHON_BIN="$(readlink ".venv/bin/python")"
PYTHON_ROOT="$(cd "$(dirname "$PYTHON_BIN")/.." && pwd)"
SITE_PACKAGES="$(find "$ROOT_DIR/.venv/lib" -path '*/site-packages' -type d | head -n 1)"

if [[ -z "$SITE_PACKAGES" || ! -d "$SITE_PACKAGES" ]]; then
  echo "Missing Kokoro site-packages in .venv."
  exit 1
fi

echo "Preparing bundled Python runtime..."
mkdir -p "$RUNTIME_DIR/python" "$RUNTIME_DIR/site-packages"
rsync -a --exclude '.DS_Store' "$PYTHON_ROOT/" "$RUNTIME_DIR/python/"
rsync -a --exclude '.DS_Store' "$SITE_PACKAGES/" "$RUNTIME_DIR/site-packages/"

echo "Building Paper2Voice macOS app..."
npm run tauri build -- --bundles app

mkdir -p "$RELEASE_DIR"
mkdir -p "$RELEASE_DIR/Paper2Voice.app"
rsync -a --exclude '.DS_Store' "target/release/bundle/macos/Paper2Voice.app/" "$RELEASE_DIR/Paper2Voice.app/"

echo "Trying to build Paper2Voice macOS DMG..."
if npm run tauri build -- --bundles dmg; then
  cp target/release/bundle/dmg/*.dmg "$RELEASE_DIR/"
else
  echo "DMG build failed; continuing with zipped .app package."
fi

cp README.md "$RELEASE_DIR/README.md"
cp SEND_TO_FRIEND.md "$RELEASE_DIR/SEND_TO_FRIEND.md"

cd "$ROOT_DIR/release"
ditto -c -k --sequesterRsrc --keepParent one-click-apple-silicon Paper2Voice-one-click-apple-silicon.zip

echo
echo "Created:"
echo "$ROOT_DIR/release/Paper2Voice-one-click-apple-silicon.zip"

if command -v hdiutil >/dev/null 2>&1; then
  if hdiutil create \
    -volname Paper2Voice \
    -srcfolder "$RELEASE_DIR" \
    -ov \
    -format UDZO \
    "$ROOT_DIR/release/Paper2Voice-one-click-apple-silicon.dmg"; then
    echo "$ROOT_DIR/release/Paper2Voice-one-click-apple-silicon.dmg"
  else
    echo "Manual DMG creation failed; zip package is still available."
  fi
fi

echo
echo "Note: this private alpha bundles the app resources and Python/Kokoro runtime for Apple Silicon Macs."
echo "The app is unsigned, so macOS may still require right-click Open or Privacy & Security approval."
