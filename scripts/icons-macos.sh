#!/bin/sh
set -eu
[ "$(uname -s)" = Darwin ] || { echo 'Mac icon generation requires macOS.' >&2; exit 1; }
workflow_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
workflow_master="$workflow_root/icons/icon-macos@2x.png"
mkdir -p "$workflow_root/target"
workflow_icon_temp=$(mktemp -d "$workflow_root/target/macos-icons.XXXXXX")
trap 'rm -rf -- "$workflow_icon_temp"' EXIT HUP INT TERM
workflow_iconset="$workflow_icon_temp/Workflow-Mac.iconset"
mkdir "$workflow_iconset"
for workflow_size in 16 32 128 256 512; do
  /usr/bin/sips -z "$workflow_size" "$workflow_size" "$workflow_master" \
    --out "$workflow_iconset/icon_${workflow_size}x${workflow_size}.png" >/dev/null
  workflow_retina=$((workflow_size * 2))
  /usr/bin/sips -z "$workflow_retina" "$workflow_retina" "$workflow_master" \
    --out "$workflow_iconset/icon_${workflow_size}x${workflow_size}@2x.png" >/dev/null
done
/usr/bin/iconutil --convert icns "$workflow_iconset" --output "$workflow_icon_temp/Workflow-Mac.icns"
mkdir -p "$workflow_root/target/macos-icons"
mv -f "$workflow_icon_temp/Workflow-Mac.icns" "$workflow_root/target/macos-icons/Workflow-Mac.icns"
echo 'Mac bundle icon generated for standard and Retina displays.'
