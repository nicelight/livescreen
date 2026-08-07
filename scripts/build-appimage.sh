#!/usr/bin/env bash
set -euo pipefail

project_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$project_dir"

download_tool() {
    local url="$1"
    local output="$2"
    if [[ ! -x "$output" ]]; then
        curl --fail --location --retry 3 "$url" --output "$output"
        chmod +x "$output"
    fi
}

mkdir -p tools
download_tool "https://github.com/linuxdeploy/linuxdeploy/releases/download/continuous/linuxdeploy-x86_64.AppImage" "tools/linuxdeploy-x86_64.AppImage"
download_tool "https://github.com/AppImage/appimagetool/releases/download/continuous/appimagetool-x86_64.AppImage" "tools/appimagetool-x86_64.AppImage"

cargo build --release
rm -rf AppDir
mkdir -p AppDir/usr/bin AppDir/usr/share/applications AppDir/usr/share/icons/hicolor/scalable/apps
install -m 0755 target/release/livescreen AppDir/usr/bin/livescreen
install -m 0644 data/com.serg.livescreen.desktop AppDir/usr/share/applications/com.serg.livescreen.desktop
install -m 0644 data/com.serg.livescreen.svg AppDir/usr/share/icons/hicolor/scalable/apps/com.serg.livescreen.svg
install -m 0755 data/AppRun AppDir/AppRun

plugin_dir="AppDir/usr/lib/gstreamer-1.0"
mkdir -p "$plugin_dir" AppDir/usr/libexec/gstreamer-1.0
for element in pipewiresrc ximagesrc v4l2src jpegdec appsink capsfilter queue filesink pulsesrc videoconvert videoscale videorate audioconvert audioresample audiomixer cairooverlay compositor x264enc h264parse voaacenc mp4mux; do
    plugin_path="$(gst-inspect-1.0 "$element" 2>/dev/null | sed -n 's/^  Filename[[:space:]]*//p' | head -n 1)"
    if [[ -n "$plugin_path" && -f "$plugin_path" ]]; then
        cp --dereference "$plugin_path" "$plugin_dir/"
    else
        echo "Не найден GStreamer plugin для $element" >&2
        exit 1
    fi
done
plugin_scanner="$(find /usr/lib -type f -path '*/gstreamer-1.0/gst-plugin-scanner' -print -quit)"
if [[ -n "$plugin_scanner" ]]; then
    cp --dereference "$plugin_scanner" AppDir/usr/libexec/gstreamer-1.0/gst-plugin-scanner
fi

./tools/linuxdeploy-x86_64.AppImage \
    --appdir AppDir \
    --desktop-file AppDir/usr/share/applications/com.serg.livescreen.desktop \
    --icon-file AppDir/usr/share/icons/hicolor/scalable/apps/com.serg.livescreen.svg \
    --output appimage

echo "AppImage создан в корне проекта."
