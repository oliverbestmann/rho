#!/usr/bin/env bash
# Downloads Inter (variable, latin) and Iosevka (latin) from Fontsource to
# web/src/fonts. Iosevka is subset to Latin, punctuation, arrows, math and box
# drawing, which takes each file from about 1 MB to 60 kB.
set -euo pipefail
cd "$(dirname "$0")/../web/src/fonts"
cdn=https://cdn.jsdelivr.net/npm

curl -sSfo inter.woff2 $cdn/@fontsource-variable/inter@5/files/inter-latin-wght-normal.woff2
curl -sSfo inter-italic.woff2 $cdn/@fontsource-variable/inter@5/files/inter-latin-wght-italic.woff2

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
for face in 400-normal: 400-italic:-italic 700-normal:-bold 700-italic:-bold-italic; do
  curl -sSfo "$tmp/in.woff2" "$cdn/@fontsource/iosevka@5/files/iosevka-latin-${face%%:*}.woff2"
  uvx --from 'fonttools[woff]' pyftsubset "$tmp/in.woff2" --flavor=woff2 \
    --unicodes='U+0000-00FF,U+0131,U+0152-0153,U+02BB-02BC,U+02C6,U+02DA,U+02DC,U+2000-206F,U+20AC,U+2122,U+2190-23FF,U+2500-25FF,U+2713-2717,U+FEFF,U+FFFD' \
    --layout-features='kern,liga,calt,ccmp,locl,mark,mkmk' \
    --output-file="iosevka${face#*:}.woff2"
done
