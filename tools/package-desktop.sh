#!/usr/bin/env bash
set -euo pipefail

root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)
cd -- "$root"
id=local.sharkdriver.SharkDriver
version=0.1.0
runtime=org.gnome.Platform
branch=49
asset_dir="$root/gui/packaging"
work="$root/.runtime/packages"
exports="$root/dist"
appimagetool="$work/bin/appimagetool-1.9.1"
appimage_runtime="$work/bin/runtime-x86_64"

mkdir -p "$work"/{cache,config,data,tmp,flatpak-user} "$exports"
export FLATPAK_USER_DIR="$work/flatpak-user"
export XDG_CACHE_HOME="$work/cache" XDG_CONFIG_HOME="$work/config" XDG_DATA_HOME="$work/data"
export TMPDIR="$work/tmp"

if [[ $(uname -m) != x86_64 ]]; then
    printf 'This release targets x86-64.\n' >&2
    exit 1
fi
if [[ ! -x "$appimagetool" || ! -f "$appimage_runtime" ]]; then
    printf 'Place the verified appimagetool and AppImage runtime in %s.\n' "$work/bin" >&2
    exit 1
fi
printf '%s  %s\n' \
    ed4ce84f0d9caff66f50bcca6ff6f35aae54ce8135408b3fa33abfc3cb384eb0 "$appimagetool" \
    617aa77314d57c8b8e4fae9c3f0b35af7b30d5529cb69e98f64f92510ed98a46 "$appimage_runtime" \
    | sha256sum -c -
if ! flatpak --user info "$runtime//$branch" >/dev/null; then
    printf 'Install %s//%s into the project Flatpak installation.\n' "$runtime" "$branch" >&2
    exit 1
fi

if [[ -x "$root/.runtime/rootfs/usr/bin/cargo" ]]; then
    bash tools/native-build.sh build --release --locked
    bash tools/native-build.sh build --release --locked --manifest-path gui/launcher/Cargo.toml \
        --target-dir /workspace/target/desktop
else
    cargo build --release --locked --target-dir "$root/target"
    cargo build --release --locked --manifest-path gui/launcher/Cargo.toml \
        --target-dir "$root/target/desktop"
fi

stage=$(mktemp -d "$work/release.XXXXXXXX")
trap 'rm -rf -- "$stage"' EXIT
launcher="$root/target/desktop/release/shark-driver"
icon="$root/design/logo/shark-silhouette.svg"
appimage_name="Shark-Driver-$version-x86_64.AppImage"
flatpak_name="Shark-Driver-$version-x86_64.flatpak"

appdir="$stage/SharkDriver.AppDir"
install -Dm755 "$launcher" "$appdir/usr/bin/shark-driver"
install -Dm755 "$asset_dir/appimage/AppRun" "$appdir/AppRun"
install -Dm644 "$asset_dir/$id.desktop" "$appdir/$id.desktop"
printf 'X-AppImage-Integrate=false\n' >> "$appdir/$id.desktop"
install -Dm644 "$icon" "$appdir/$id.svg"
magick -background none "$icon" -resize 256x256 -gravity center \
    -extent 256x256 "$appdir/$id.png"
ln -s "$id.png" "$appdir/.DirIcon"
install -Dm644 LICENSE "$appdir/usr/share/licenses/shark-driver/LICENSE"
install -Dm644 THIRD_PARTY_NOTICES "$appdir/usr/share/licenses/shark-driver/THIRD_PARTY_NOTICES"
install -Dm644 "$asset_dir/appimage/runtime-LICENSE" \
    "$appdir/usr/share/licenses/shark-driver/AppImage-runtime-LICENSE"
install -Dm644 "$asset_dir/$id.metainfo.xml" "$appdir/usr/share/metainfo/$id.metainfo.xml"
APPIMAGE_EXTRACT_AND_RUN=1 APPIMAGELAUNCHER_DISABLE=1 QT_QPA_PLATFORM=xcb ARCH=x86_64 \
    "$appimagetool" --no-appstream --runtime-file "$appimage_runtime" "$appdir" "$stage/$appimage_name"

build="$stage/flatpak"
repo="$stage/repo"
# The launcher is prebuilt. An SDK deployment is unnecessary for this export.
flatpak build-init "$build" "$id" "$runtime" "$runtime" "$branch"
install -Dm755 "$launcher" "$build/files/libexec/shark-driver"
install -Dm755 "$asset_dir/flatpak/shark-driver" "$build/files/bin/shark-driver"
install -Dm644 "$asset_dir/$id.desktop" "$build/files/share/applications/$id.desktop"
install -Dm644 "$asset_dir/$id.metainfo.xml" "$build/files/share/metainfo/$id.metainfo.xml"
install -Dm644 "$appdir/$id.png" "$build/files/share/icons/hicolor/256x256/apps/$id.png"
install -Dm644 LICENSE "$build/files/share/licenses/shark-driver/LICENSE"
install -Dm644 THIRD_PARTY_NOTICES "$build/files/share/licenses/shark-driver/THIRD_PARTY_NOTICES"
flatpak build-finish --command=shark-driver --socket=wayland --socket=fallback-x11 \
    --share=ipc --share=network --device=all --filesystem=host-os:ro \
    --system-talk-name=org.bluez --talk-name=org.freedesktop.Flatpak "$build"
flatpak build-export "$repo" "$build" stable
flatpak build-bundle "$repo" "$stage/$flatpak_name" "$id" stable \
    --runtime-repo=https://dl.flathub.org/repo/flathub.flatpakrepo

mv -f -- "$stage/$appimage_name" "$stage/$flatpak_name" "$exports/"
(
    cd -- "$exports"
    sha256sum "$appimage_name" "$flatpak_name" > SHA256SUMS
)
