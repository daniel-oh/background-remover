#!/usr/bin/env sh
# Send a photo to a running background-remover and keep the result.
#
#   examples/curl.sh photo.jpg            # writes photo.png next to it
#   HOST=http://remover:7000 examples/curl.sh photo.jpg
set -eu

HOST="${HOST:-http://127.0.0.1:7000}"
IN="${1:?usage: curl.sh <photo.jpg|png|webp>}"
OUT="${IN%.*}.png"

case "$IN" in
  *.jpg|*.jpeg|*.JPG|*.JPEG) TYPE=image/jpeg ;;
  *.png|*.PNG) TYPE=image/png ;;
  *.webp|*.WEBP) TYPE=image/webp ;;
  *) echo "unsupported extension on $IN" >&2; exit 2 ;;
esac

curl -sS --fail --data-binary "@$IN" -H "content-type: $TYPE" "$HOST/remove" -o "$OUT"
echo "wrote $OUT"
