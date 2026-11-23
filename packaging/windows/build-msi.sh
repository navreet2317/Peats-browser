#!/usr/bin/env bash
# Costruisce dist/peats-browser-<versione>-x64.msi dall'eseguibile Windows
# compilato con cargo-xwin. Usa wixl (pacchetto msitools), quindi funziona su
# Linux senza il toolset WiX di Microsoft.
#
#   packaging/windows/build-msi.sh [percorso/eseguibile.exe]
set -euo pipefail

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
exe=${1:-$root/target/x86_64-pc-windows-msvc/release/peats-windows.exe}
wixl=${WIXL:-wixl}

command -v "$wixl" >/dev/null || {
    echo "serve wixl: dnf install msitools" >&2
    exit 1
}

version=$(sed -n 's/^version = "\(.*\)"/\1/p' "$root/windows/Cargo.toml" | head -n1)
[ -n "$version" ] || {
    echo "versione non trovata in windows/Cargo.toml" >&2
    exit 1
}
[ -f "$exe" ] || {
    echo "eseguibile mancante: $exe" >&2
    exit 1
}

# L'MSI richiede quattro numeri: 0.1.0 diventa 0.1.0.0.
msi_version="$version"
while [ "$(printf '%s' "$msi_version" | awk -F. '{print NF}')" -lt 4 ]; do
    msi_version="$msi_version.0"
done

out="$root/dist/peats-browser-$version-x64.msi"
mkdir -p "$root/dist"

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

sed -e "s|@@VERSION@@|$msi_version|" -e "s|@@SOURCE@@|$exe|" \
    "$root/packaging/windows/peats.wxs.in" >"$tmp/peats.wxs"

"$wixl" --arch x64 -o "$out" "$tmp/peats.wxs"
echo "$out"
