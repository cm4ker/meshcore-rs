#!/usr/bin/env bash
# The deltas of a release (CI's release and promote runs): a delta from each of the last three
# releases of each board and role in out/, for the devices that run one of them and fetch updates
# over LoRa from a neighbouring repeater (meshcore::fwshare); each is checked to rebuild the
# signed image from its base. Releases that left the board and role out don't count: its devices
# still run an older one. A stable release also takes the last two stable ones: after a run of dev
# releases those are what the devices that follow stable still run.
#
# In: out/<board>-<role>-$MCFW_VERSION-r$MCFW_COUNTER.mcfw, out/root.txt, images/<board>-<role>.bin,
# MCFW_VERSION, MCFW_COUNTER, MCFW_CHANNEL, GH_TOKEN. Out: the .mcfd files in out/, deltas.md.
set -euo pipefail
sign=target/release/ota-sign
root=$(awk '/fingerprint/{print $2}' out/root.txt)
# GitHub answers a download with HTTP 500 now and then; one of the thirty or so here
# failed whole releases (0.8.7-dev.16, twice): asked again, after a pause
fetch() {
  for try in 1 2 3 4; do
    gh release download "$@" && return 0
    echo "::warning::download of $* failed (try $try)"
    sleep $((try * 10))
  done
  gh release download "$@"
}
# What a node takes over LoRa by itself: at most meshcore::fwshare::MAX_LORA_FILE, and on a Pro Micro
# running release #62 or before what its staging area (0x7B000..0xD9000,
# firmware/models/promicro/src/ota.rs) holds next to the image the delta rebuilds (later
# ones apply a delta as it comes)
lora_max=163840
lora_room() {
  case "$1" in
    promicro-*) if [ "${2##*-r}" -le 62 ]; then local img; img=$(stat -c %s "images/$1.bin"); echo $(( 385024 - (img + 4095) / 4096 * 4096 )); else echo "$lora_max"; fi ;;
    # OtaEndpoint::delta_room reserves a third of the T096's 224 KiB staging area.
    heltec-t096-*) echo $(( 229376 / 3 * 2 )) ;;
    *) echo "$lora_max" ;;
  esac
}
tags=$(gh release list --limit 30 --exclude-drafts --json tagName -q '.[].tagName')
stable_tags=" $(gh release list --limit 30 --exclude-drafts --exclude-pre-releases --json tagName -q '.[].tagName' | tr '\n' ' ') "
# what each of them carries, asked eight at a time, not one after another
mkdir -p listed
printf '%s\n' $tags | xargs -r -P 8 -I{} bash -euo pipefail -c 'gh release view "$1" --json assets -q ".assets[].name" > "listed/$1"' list {}
declare -A assets
for tag in $tags; do
  assets[$tag]=$(cat "listed/$tag")
done
# the deltas to make, in the table's order: <row> <board>-<role> <release> <its file>
: > deltas.txt
for mcfw in out/*.mcfw; do
  name=$(basename "$mcfw" "-$MCFW_VERSION-r$MCFW_COUNTER.mcfw")   # <board>-<role>
  # the releases to make deltas from: newest first, as said above
  bases="" n=0 s=0
  for tag in $tags; do
    base=$(printf '%s\n' "${assets[$tag]}" | grep -E "^$name-.*\.mcfw$" | head -1 || true)
    [ -n "$base" ] || continue
    is_stable=0
    [[ "$stable_tags" == *" $tag "* ]] && is_stable=1
    if [ "$n" -lt 3 ] || { [ "$MCFW_CHANNEL" = stable ] && [ "$is_stable" = 1 ] && [ "$s" -lt 2 ]; }; then
      bases="$bases $tag"
    fi
    n=$((n + 1))
    s=$((s + is_stable))
    if [ "$n" -ge 3 ] && { [ "$MCFW_CHANNEL" != stable ] || [ "$s" -ge 2 ]; }; then
      break
    fi
  done
  for tag in $bases; do
    base=$(printf '%s\n' "${assets[$tag]}" | grep -E "^$name-.*\.mcfw$" | head -1 || true)
    printf '%03d %s %s %s\n' "$(wc -l < deltas.txt)" "$name" "$tag" "$base" >> deltas.txt
  done
done
# One of them, and its row of the table in rows/<row>
delta() {
  local row=$1 name=$2 tag=$3 base=$4
  local mcfw=out/$name-$MCFW_VERSION-r$MCFW_COUNTER.mcfw
  local mcfd=out/$name-$MCFW_VERSION-r$MCFW_COUNTER.from-${tag##*-}.mcfd
  mkdir -p "prev/$tag"
  fetch "$tag" -p "$base" -D "prev/$tag" --clobber
  $sign delta "prev/$tag/$base" "$mcfw" -o "$mcfd"
  $sign verify "$mcfd" --root "$root" --base "prev/$tag/$base"
  local size room mark=""
  size=$(stat -c %s "$mcfd")
  room=$(lora_room "$name" "$tag")
  [ "$room" -le "$lora_max" ] || room=$lora_max
  if [ "$size" -gt "$room" ]; then
    mark=" (over the LoRa limit, $((room / 1024)) KB)"
    echo "::warning title=Delta over the LoRa limit::$name from $tag is $((size / 1024)) KB, more than $((room / 1024)) KB: nodes on $tag don't take it from a neighbour over LoRa by themselves, only over Wi-Fi, USB or BLE, or by hand (fw update) where $tag has that"
  fi
  echo "| $name | $tag | $((size / 1024)) KB$mark |" > "rows/$row"
}
# ota-sign makes a delta on one core, in seconds, and there are thirty or so: as many at
# once as there are cores. One that fails fails the step.
export -f delta fetch lora_room
export sign root lora_max
mkdir -p rows
xargs -r -P "$(nproc)" -L 1 bash -euo pipefail -c 'delta "$@"' delta < deltas.txt
{ printf '| delta | from | size |\n|---|---|---|\n'; find rows -type f | sort | xargs -r cat; } > deltas.md
cat deltas.md >> "$GITHUB_STEP_SUMMARY"
