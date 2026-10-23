#!/bin/sh
# Scarica pdfium (bblanchon/pdfium-binaries) nella versione per cui è compilato
# pdfium-render (feature pdfium_latest = 7881) in pdfium/linux-x64 e pdfium/win-x64.
set -eu
VERSIONE=7881
cd "$(dirname "$0")"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
for piattaforma in linux-x64 win-x64; do
    curl -fsSL -o "$tmp/$piattaforma.tgz" \
        "https://github.com/bblanchon/pdfium-binaries/releases/download/chromium%2F$VERSIONE/pdfium-$piattaforma.tgz"
    mkdir -p "pdfium/$piattaforma"
    case $piattaforma in
        linux-x64) tar xzf "$tmp/$piattaforma.tgz" -C "$tmp" lib/libpdfium.so && mv "$tmp/lib/libpdfium.so" "pdfium/$piattaforma/" ;;
        win-x64) tar xzf "$tmp/$piattaforma.tgz" -C "$tmp" bin/pdfium.dll LICENSE && mv "$tmp/bin/pdfium.dll" "pdfium/$piattaforma/" && mv "$tmp/LICENSE" pdfium/LICENSE ;;
    esac
done
echo "pdfium $VERSIONE scaricato in pdfium/"
