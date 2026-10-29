#!/bin/sh
# Scarica pdfium (bblanchon/pdfium-binaries) nella versione per cui è compilato
# pdfium-render (feature pdfium_latest = 7881) in pdfium/linux-x64 e pdfium/win-x64.
# Uso: ./scarica-pdfium.sh [piattaforma...]  (es. ./scarica-pdfium.sh linux-x64
# per la CI Linux; senza argomenti scarica entrambe).
set -eu
VERSIONE=7881
cd "$(dirname "$0")"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
if [ "$#" -gt 0 ]; then
    piattaforme="$*"
else
    piattaforme="linux-x64 win-x64"
fi
for piattaforma in $piattaforme; do
    curl -fsSL -o "$tmp/$piattaforma.tgz" \
        "https://github.com/bblanchon/pdfium-binaries/releases/download/chromium%2F$VERSIONE/pdfium-$piattaforma.tgz"
    mkdir -p "pdfium/$piattaforma"
    case $piattaforma in
        linux-x64) tar xzf "$tmp/$piattaforma.tgz" -C "$tmp" lib/libpdfium.so && mv "$tmp/lib/libpdfium.so" "pdfium/$piattaforma/" ;;
        win-x64) tar xzf "$tmp/$piattaforma.tgz" -C "$tmp" bin/pdfium.dll LICENSE && mv "$tmp/bin/pdfium.dll" "pdfium/$piattaforma/" && mv "$tmp/LICENSE" pdfium/LICENSE ;;
        *) echo "piattaforma non supportata: $piattaforma" >&2; exit 1 ;;
    esac
done
echo "pdfium $VERSIONE scaricato in pdfium/"
