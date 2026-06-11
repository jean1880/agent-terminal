#!/bin/bash
VERSION=$1
if [ -z "$VERSION" ]; then
  echo "Usage: $0 <version>"
  exit 1
fi

# Parse config values using Python
PKG_NAME=$(python3 -c "import json; print(json.load(open('package-config.json'))['package_name'])")
BIN_NAME=$(python3 -c "import json; print(json.load(open('package-config.json'))['binary_name'])")
DESC=$(python3 -c "import json; print(json.load(open('package-config.json'))['description'])")
DESKTOP_FILE=$(python3 -c "import json; print(json.load(open('package-config.json'))['desktop_file'])")
LOGO_FILE=$(python3 -c "import json; print(json.load(open('package-config.json'))['logo_file'])")

fpm -f -s dir -t deb \
  -n "$PKG_NAME" \
  --version "$VERSION" \
  --description "$DESC" \
  -d "libvte-2.91-gtk4-0" \
  -d "libgtk-4-1" \
  "target/release/$BIN_NAME=/usr/bin/$PKG_NAME" \
  "$DESKTOP_FILE=/usr/share/applications/$(basename "$DESKTOP_FILE")" \
  "$LOGO_FILE=/usr/share/icons/$(basename "$LOGO_FILE")"
