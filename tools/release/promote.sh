#!/usr/bin/env bash
# A dev release's images signed again for stable (promote.yml): the same bytes under the same
# release number, with a stable manifest and the stable name, into out/; the USB files and the
# programs as the dev release has them. Only images the root signed are taken.
#
# In: PROMOTE_TAG, DEV_VERSION, MCFW_VERSION, MCFW_COUNTER, MCFW_ROOT_KEY, MCFW_RELEASE_ROOT,
# GH_TOKEN. Out: out/ as the release run makes it, images/<board>-<role>.bin.
set -euo pipefail
sign=target/release/ota-sign
test -n "${MCFW_ROOT_KEY:-}" || { echo "the MCFW_ROOT_KEY secret is not set"; exit 1; }
root=$($sign fingerprint --key-env MCFW_ROOT_KEY | awk '/fingerprint/{print $2}')
test "$root" = "$MCFW_RELEASE_ROOT" || { echo "MCFW_ROOT_KEY is not the root the images pin ($MCFW_RELEASE_ROOT)"; exit 1; }
mkdir -p dev images out
gh release download "$PROMOTE_TAG" -D dev -p '*.mcfw'
# what the dev release has besides: none of it is required
for p in '*.uf2' '*.zip' 'ota-app-*' 'content.txt'; do
  gh release download "$PROMOTE_TAG" -D dev -p "$p" 2>/dev/null || echo "$PROMOTE_TAG has no $p"
done
suffix="-$DEV_VERSION-r$MCFW_COUNTER.mcfw"
n=0
for f in dev/*"$suffix"; do
  [ -e "$f" ] || continue
  name=$(basename "$f" "$suffix")   # <board>-<role>
  board=${name%-*}
  role=${name##*-}
  $sign verify "$f" --root "$root" > /dev/null
  $sign extract "$f" -o "images/$name.bin"
  mcfw=out/$name-$MCFW_VERSION-r$MCFW_COUNTER.mcfw
  $sign bundle "images/$name.bin" --board "$board" --role "$role" --counter "$MCFW_COUNTER" --version "$MCFW_VERSION" --channel stable -o "$mcfw"
  $sign sign "$mcfw" --key-env MCFW_ROOT_KEY
  n=$((n + 1))
done
test "$n" -gt 0 || { echo "$PROMOTE_TAG has no images of its own (*$suffix)"; exit 1; }
echo "$n images of $PROMOTE_TAG signed again as $MCFW_VERSION, stable"
find dev -maxdepth 1 \( -name '*.uf2' -o -name '*.zip' -o -name 'ota-app-*' -o -name content.txt \) -exec cp {} out/ \;
$sign fingerprint --key-env MCFW_ROOT_KEY > out/root.txt
for mcfw in out/*.mcfw; do $sign verify "$mcfw" --root "$root"; done
