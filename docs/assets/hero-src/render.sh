#!/bin/sh
# Renders ../hero.jpg from hero.html with headless Chrome at 2x.
cd "$(dirname "$0")"
CHROME="${CHROME:-/Applications/Google Chrome.app/Contents/MacOS/Google Chrome}"
"$CHROME" --headless=new --hide-scrollbars --force-device-scale-factor=2 \
  --window-size=1600,760 --virtual-time-budget=3000 \
  --screenshot="$PWD/../hero.png" "file://$PWD/hero.html" 2>/dev/null
sips -s format jpeg -s formatOptions 88 ../hero.png --out ../hero.jpg >/dev/null && rm ../hero.png
