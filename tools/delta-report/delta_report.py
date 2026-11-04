#!/usr/bin/env python3
"""How large the next release's deltas would be. For each image: the delta file (.mcfd, what a node
fetches from a neighbour over LoRa) from the newest release of its board and role, and from the
newest stable one, made by tools/ota-sign as the release makes it. CI runs this on every push and
pull request; run it by hand on images you built, before you push:

    cargo build --release -p ota-sign
    python tools/delta-report/delta_report.py heltec-v4-repeater.bin [--from v0.8.2-r42] [--budget 64]

An image is <board>-<role>.bin, as CI's artifacts name them (or say --as heltec-v4-repeater).
A delta over the budget is worth a release before more goes in: each LoRa update holds the shared
channel for minutes. One over the LoRa limit reaches nodes over Wi-Fi, USB or BLE, or over LoRa
by hand (`fw update`).

On the ESP32 boards most of a large delta comes from statics that moved, not from the code that
changed: when the code in RAM (IRAM) or the initialised statics (.data) change size, the addresses
after them change, and with them the literal pools all over the code (0.8.1 -> 0.8.2: 620 bytes
more IRAM moved .data by 624 bytes, 97 KB of a 112 KB delta). The "RAM layout" column reads that
from the images' segments; .bss isn't in an image, so its changes don't show.
"""

import argparse
import json
import os
import pathlib
import re
import struct
import subprocess
import sys
import tempfile

REPO = pathlib.Path(__file__).resolve().parents[2]
DEFAULT_OTA_SIGN = REPO / "target" / "release" / ("ota-sign.exe" if os.name == "nt" else "ota-sign")
DEFAULT_CACHE = REPO / "target" / "delta-report"
# What a node takes over LoRa by itself: meshcore::fwshare::MAX_LORA_FILE
LORA_MAX = 163840
# A Pro Micro running release #62 or before stages a delta in 0x7B000..0xD9000
# (firmware/models/promicro/src/ota.rs), next to the image it rebuilds; later ones apply it as it comes
PROMICRO_STAGING = 0xD9000 - 0x7B000
PROMICRO_LAST_STAGED = 62


def lora_room(name, image_len, tag):
    if name.startswith("promicro-") and int(tag.rsplit("-r", 1)[1]) <= PROMICRO_LAST_STAGED:
        return min(LORA_MAX, PROMICRO_STAGING - (image_len + 4095) // 4096 * 4096)
    return LORA_MAX


def run(*args):
    return subprocess.run([str(a) for a in args], check=True, capture_output=True, text=True).stdout


class Releases:
    """The published releases, newest first, and their files (fetched when asked for)."""

    def __init__(self):
        rows = json.loads(run("gh", "release", "list", "--limit", "30", "--exclude-drafts", "--json", "tagName,isPrerelease"))
        self.tags = [(r["tagName"], not r["isPrerelease"]) for r in rows]
        self._assets = {}

    def release_file(self, tag, name):
        """The .mcfw of `name` (<board>-<role>) in release `tag`, if it carries one."""
        if tag not in self._assets:
            self._assets[tag] = run("gh", "release", "view", tag, "--json", "assets", "-q", ".assets[].name").split()
        pattern = re.compile(re.escape(name) + r"-.*\.mcfw")
        return next((a for a in self._assets[tag] if pattern.fullmatch(a)), None)

    def bases(self, name):
        """The newest release that carries `name`, and the newest stable one if that is another."""
        newest = stable = None
        for tag, is_stable in self.tags:
            file = self.release_file(tag, name)
            if file is None:
                continue
            newest = newest or (tag, file)
            if is_stable:
                stable = (tag, file)
                break
        return [b for b in dict.fromkeys([newest, stable]) if b]


def segments(image):
    """An ESP32 image's segments as (load address, length), or None for another image."""
    if len(image) < 24 or image[0] != 0xE9:
        return None
    out, off = [], 24
    for _ in range(image[1]):
        addr, length = struct.unpack_from("<II", image, off)
        out.append((addr, length))
        off += 8 + length
    return out


def ram_regions(segs):
    """The RAM segments merged where they adjoin (the image tool splits .data to pad the flash
    segments), as (label, start, end)."""
    flash = lambda a: 0x3C000000 <= a < 0x3E000000 or 0x42000000 <= a < 0x44000000
    regions = []
    for addr, length in sorted(s for s in segs if not flash(s[0])):
        if regions and regions[-1][1] == addr:
            regions[-1][1] = addr + length
        else:
            regions.append([addr, addr + length])
    label = lambda a: "IRAM" if 0x40370000 <= a < 0x403E0000 else ".data" if 0x3FC80000 <= a < 0x3FD00000 else "RAM"
    return [(label(s), s, e) for s, e in regions]


def ram_layout(base, new):
    """What moved or grew in RAM between two ESP32 images."""
    a, b = segments(base), segments(new)
    if a is None or b is None:
        return "-"
    ra, rb = ram_regions(a), ram_regions(b)
    if [r[0] for r in ra] != [r[0] for r in rb]:
        return "other RAM segments"
    changes = []
    for (label, s0, e0), (_, s1, e1) in zip(ra, rb):
        if s1 != s0:
            changes.append(f"{label} moved {s1 - s0:+} B")
        if e1 - s1 != e0 - s0:
            changes.append(f"{label} {(e1 - s1) - (e0 - s0):+} B")
    return ", ".join(changes) or "same"


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("images", nargs="+", type=pathlib.Path, help="<board>-<role>.bin")
    ap.add_argument("--as", dest="name", help="the board and role of a single image, e.g. heltec-v4-repeater")
    ap.add_argument("--from", dest="tags", action="append", help="a release to make the delta from (default: the newest and the newest stable)")
    ap.add_argument("--budget", type=int, default=64, help="KB a delta should stay under (default 64)")
    ap.add_argument("--ota-sign", type=pathlib.Path, default=DEFAULT_OTA_SIGN)
    ap.add_argument("--cache", type=pathlib.Path, default=DEFAULT_CACHE, help="where releases are downloaded to")
    args = ap.parse_args()
    if args.name and len(args.images) > 1:
        ap.error("--as names a single image")
    if not args.ota_sign.exists():
        ap.error(f"{args.ota_sign}: build it with `cargo build --release -p ota-sign`")

    releases = Releases()
    budget = args.budget * 1024
    github = os.environ.get("GITHUB_ACTIONS") == "true"
    rows = ["| image | from | delta | RAM layout |", "|---|---|---|---|"]
    work = pathlib.Path(tempfile.mkdtemp(prefix="delta-report-"))
    for path in args.images:
        name = args.name or path.stem
        board, _, role = name.rpartition("-")
        image = path.read_bytes()
        new = work / f"{name}.mcfw"
        run(args.ota_sign, "bundle", path, "--board", board, "--role", role, "--counter", "1", "--version", "check", "--issued", "0", "-o", new)
        bases = [(t, releases.release_file(t, name)) for t in args.tags] if args.tags else releases.bases(name)
        if not bases:
            rows.append(f"| {name} | | no release yet | |")
        for tag, file in bases:
            if file is None:
                rows.append(f"| {name} | {tag} | not in that release | |")
                continue
            base = args.cache / tag / file
            if not base.exists():
                run("gh", "release", "download", tag, "-p", file, "-D", args.cache / tag, "--clobber")
            delta = work / f"{name}.from-{tag}.mcfd"
            run(args.ota_sign, "delta", base, new, "-o", delta)
            size = delta.stat().st_size
            base_image = work / f"{name}.{tag}.bin"
            run(args.ota_sign, "extract", base, "-o", base_image)
            layout = ram_layout(base_image.read_bytes(), image)
            room = lora_room(name, len(image), tag)
            mark = warning = ""
            if size > room:
                title, mark = "Delta over the LoRa limit", f", over the LoRa limit ({room // 1024} KB)"
                warning = f"{name} from {tag} would be {size // 1024} KB, more than {room // 1024} KB: nodes on {tag} would not take it over LoRa by themselves, only over Wi-Fi, USB or BLE, or by hand (fw update) where {tag} has that"
            elif size > budget:
                title, mark = "Delta over the budget", f", over the budget ({args.budget} KB)"
                warning = f"{name} from {tag} would be {size // 1024} KB, more than {args.budget} KB (RAM layout: {layout}): release before more goes in"
            if warning and github:
                print(f"::warning title={title}::{warning}")
            elif warning:
                print(f"warning: {warning}", file=sys.stderr)
            rows.append(f"| {name} | {tag} | {size // 1024} KB{mark} | {layout} |")
    table = "\n".join(rows) + "\n"
    print(table)
    summary = os.environ.get("GITHUB_STEP_SUMMARY")
    if summary:
        with open(summary, "a", encoding="utf-8") as f:
            f.write(f"Deltas this build would have, against the budget of {args.budget} KB:\n\n{table}")


if __name__ == "__main__":
    main()
