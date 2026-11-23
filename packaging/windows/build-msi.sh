#!/usr/bin/env bash
# Costruisce dist/peats-browser-<versione>-x64.msi dall'eseguibile Windows
# compilato con cargo-xwin. Usa wixl (pacchetto msitools), quindi funziona su
# Linux senza il toolset WiX di Microsoft.
#
#   packaging/windows/build-msi.sh [percorso/eseguibile.exe]
#
# La versione si può forzare con PEATS_VERSION (utile per ricostruire un MSI
# di una release passata senza toccare windows/Cargo.toml).
set -euo pipefail

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
exe=${1:-$root/target/x86_64-pc-windows-msvc/release/peats-windows.exe}
wixl=${WIXL:-wixl}

command -v "$wixl" >/dev/null || {
    echo "serve wixl: dnf install msitools" >&2
    exit 1
}

version=${PEATS_VERSION:-$(sed -n 's/^version = "\(.*\)"/\1/p' "$root/windows/Cargo.toml" | head -n1)}
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

# Il ProductCode deve cambiare a ogni versione, altrimenti un MSI nuovo con lo
# stesso codice viene trattato come «già installato» e l'aggiornamento non
# sostituisce nulla. Derivandolo dalla versione (UUID v5 con lo stesso spazio
# dei nomi) resta identico per la stessa versione: una ricostruzione non
# rompe né la riparazione né il MajorUpgrade. L'UpgradeCode, che lega fra loro
# le versioni, rimane invece fisso nel file .wxs.
command -v python3 >/dev/null || {
    echo "serve python3 per derivare il ProductCode" >&2
    exit 1
}
upgrade_code=7C3F1D42-9A6B-4E58-8C2E-5B1F0A9D3E71
product_code=$(python3 -c 'import sys, uuid; print(str(uuid.uuid5(uuid.UUID(sys.argv[1]), sys.argv[2])).upper())' \
    "$upgrade_code" "PeatsBrowser-$msi_version")

out="$root/dist/peats-browser-$version-x64.msi"
mkdir -p "$root/dist"

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

sed -e "s|@@VERSION@@|$msi_version|" \
    -e "s|@@PRODUCTCODE@@|{$product_code}|" \
    -e "s|@@SOURCE@@|$exe|" \
    "$root/packaging/windows/peats.wxs.in" >"$tmp/peats.wxs"

# Se la sostituzione non è avvenuta il GUID resterebbe vuoto (o finirebbe nel
# testo): meglio fermarsi che produrre un MSI con un ProductCode condiviso.
grep -q "Id=\"{$product_code}\"" "$tmp/peats.wxs" || {
    echo "ProductCode non inserito nel .wxs" >&2
    exit 1
}

"$wixl" --arch x64 -o "$out" "$tmp/peats.wxs"
echo "$out (ProductCode {$product_code})"
