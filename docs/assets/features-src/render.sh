#!/bin/sh
# Renders ../features/<name>.png from each <name>.html here, at 2x with a transparent background.
cd "$(dirname "$0")"
CHROME="${CHROME:-/Applications/Google Chrome.app/Contents/MacOS/Google Chrome}"
mkdir -p ../features
for f in *.html; do
  "$CHROME" --headless=new --hide-scrollbars --force-device-scale-factor=2 --default-background-color=00000000 \
    --window-size=800,400 --virtual-time-budget=3000 \
    --screenshot="$PWD/../features/${f%.html}.png" "file://$PWD/$f" 2>/dev/null
done
