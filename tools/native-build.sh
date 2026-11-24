#!/usr/bin/env bash
set -euo pipefail

root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)
rootfs="$root/.runtime/rootfs"
if [[ ! -x "$rootfs/usr/bin/cargo" ]]; then
    printf 'Project-local Rust is missing. See docs/native/build.md.\n' >&2
    exit 1
fi

mkdir -p "$rootfs/workspace" "$root/.runtime/native/cargo"
exec bwrap --unshare-all --unshare-user --disable-userns --die-with-parent --new-session \
    --ro-bind "$rootfs" / --proc /proc --dev /dev --tmpfs /tmp --tmpfs /run \
    --bind "$root" /workspace --chdir /workspace --clearenv \
    --setenv PATH /usr/bin:/bin --setenv HOME /workspace/.runtime/native \
    --setenv CARGO_HOME /workspace/.runtime/native/cargo --setenv LANG C.UTF-8 \
    -- /usr/bin/cargo --offline "$@"
