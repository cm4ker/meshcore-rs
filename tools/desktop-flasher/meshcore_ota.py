"""Update a repeater running this firmware over LoRa, through any MeshCore companion radio.

The image travels as admin requests (REQ types 0xF0-0xF5, see crates/meshcore/src/repeater.rs):
encrypted with the admin's shared secret, so only a logged-in admin can push firmware.

    pip install meshcore
    espflash save-image --chip esp32s3 --flash-size 16mb <elf> firmware.bin
    python meshcore_ota.py --port COM4 --node PKIO --password-file pw.txt firmware.bin
    python meshcore_ota.py --ble D8:E6:C5:14:21:A8 --node PKIO ...   # a companion over BLE

Delta updates: the repeater reports the SHA-256 of the image it runs; if that image is in the
image store (--images, default firmware/models/heltec/images), only a bsdiff patch is sent (made by
tools/ota-delta, `cargo build --release -p ota-delta`). Every image sent is added to the store;
add images flashed over USB with --register.

Progress lines go to stdout; the transfer can be re-run and resumes missing chunks only if the
repeater was not rebooted in between (sessions live in RAM).

Signed releases: give the release's `.mcfw` file (tools/ota-sign) instead of the `.bin`. Its
envelope goes first, and a repeater that pins a root key (`set fw.root`) only takes an image
that key signed.
"""

import argparse
import asyncio
import getpass
import hashlib
import math
import os
import pathlib
import shutil
import struct
import subprocess
import sys
import tempfile
import time
import zlib



REQ_OTA_BEGIN, REQ_OTA_DATA, REQ_OTA_STATUS, REQ_OTA_COMMIT, REQ_OTA_ABORT, REQ_OTA_INFO, REQ_OTA_MANIFEST = \
    range(0xF0, 0xF7)
CMD_SEND_BINARY_REQ = 0x32
# companion serial frames are at most 176 bytes: cmd + pubkey (33) + req type + index (3) + data
CHUNK = 140
# a piece of a release envelope: cmd + pubkey (33) + req type + offset, total, length (5) + data
ENVELOPE_PIECE = 136
ERRORS = {1: "no OTA slot", 2: "image too large", 3: "bad parameters", 4: "no session", 5: "flash error",
          6: "incomplete", 7: "SHA-256 mismatch", 8: "not an image for this chip",
          9: "the repeater does not run the delta's base image",
          10: "unsigned: the device pins a root key, give it the release's .mcfw",
          11: "the release is not signed by the root key the device pins",
          12: "the release is for another board or role", 13: "the release is older than the one the device runs",
          14: "unreadable release envelope",
          15: "the release is not signed by the owner key the device pins (sign it on the release server)",
          16: "over LoRa or TCP the device takes only signed releases, and it pins no root key "
              "(pin one with `set fw.root`, or update over BLE or USB)",
          17: "an update is already installed and the device is restarting into it"}
BOARDS = {1: "heltec-v4", 2: "heltec-v3", 3: "xiao-c6", 4: "t-echo", 5: "promicro", 6: "heltec-paper",
          7: "heltec-t114", 8: "t3s3", 9: "heltec-t096", 10: "heltec-mesh-pocket"}
ROLES = {1: "repeater", 2: "companion"}
REPO = pathlib.Path(__file__).resolve().parents[2]
DEFAULT_IMAGES = REPO / "firmware" / "models" / "heltec" / "images"
DEFAULT_DELTA_TOOL = REPO / "target" / "release" / ("ota-delta.exe" if os.name == "nt" else "ota-delta")


def log(msg):
    print(f"[{time.strftime('%H:%M:%S')}] {msg}", flush=True)


def airtime_ms(length, sf, bw_khz, cr):
    """LoRa time on air (explicit header, CRC on), same formula as the firmware."""
    preamble = 32 if sf <= 8 else 16
    ldro = (1 << sf) / bw_khz >= 16.0
    div = 4 * (sf - 2) if ldro else 4 * sf
    bits = max(8 * length + 16 - 4 * sf + 20, 0)
    n = preamble + 4.25 + math.ceil(bits / div) * cr + 8
    return (1 << sf) / bw_khz * n


def load_firmware(path):
    """The image, and the envelope if the file is a signed release (.mcfw, crates/meshcore/src/release.rs)."""
    data = open(path, "rb").read()
    if data[:4] != b"MCFW":
        return data, None
    vlen = data[52]
    env_len = 53 + vlen + 1 + 96 * data[53 + vlen]
    return data[env_len:], data[:env_len]


def describe_envelope(env):
    board, role, _, counter, image_len = struct.unpack_from("<BBBII", env, 5)
    version = env[53:53 + env[52]].decode(errors="replace")
    signers = env[53 + env[52]]
    return (f"release {BOARDS.get(board, board)}/{ROLES.get(role, role)} {version} #{counter}, "
            f"{image_len} bytes, {signers} signature(s)")


def describe_device(running):
    """What INFO says after the running image: `[board][role][release counter u32][pinned roots]
    [pinned owners]` (the owners from the firmware that knows owner keys on)."""
    if len(running) < 19:
        return None, "the device predates signed releases"
    board, role, counter, roots = struct.unpack_from("<BBIB", running, 12)
    owners = running[19] if len(running) >= 20 else 0
    pins = f"pins {roots} root key(s): signed releases only" if roots else "pins no root key: unsigned images accepted"
    if roots and owners:
        pins += f", and {owners} owner key(s): one of them must have signed it too"
    return roots, f"device build {BOARDS.get(board, '?')}/{ROLES.get(role, '?')} #{counter}, {pins}"


def envelope_pieces(env, size):
    """MANIFEST request payloads: `[offset u16][total u16][len u8][piece]`."""
    for off in range(0, len(env), size):
        piece = env[off:off + size]
        yield struct.pack("<HHB", off, len(env), len(piece)) + piece


def store_image(images, image):
    """Keep `image` in the store under its SHA-256 prefix (a future delta base); no store, nothing."""
    if images is None:
        return None
    images.mkdir(parents=True, exist_ok=True)
    path = images / f"{hashlib.sha256(image).hexdigest()[:16]}.bin"
    if not path.exists():
        path.write_bytes(image)
    return path


def find_base(images, length, sha8):
    """The stored image the repeater runs, by length and SHA-256 prefix."""
    if images is None or not images.is_dir():
        return None
    for path in sorted(images.glob("*.bin")):
        if path.stat().st_size == length:
            data = path.read_bytes()
            if hashlib.sha256(data).digest()[:8] == sha8:
                return data
    return None


def make_delta(tool, base, image):
    """bsdiff patch base -> image via tools/ota-delta."""
    with tempfile.TemporaryDirectory() as d:
        b, n, out = (pathlib.Path(d) / x for x in ("base.bin", "new.bin", "patch.bin"))
        b.write_bytes(base)
        n.write_bytes(image)
        subprocess.run([str(tool), str(b), str(n), str(out)], check=True, stdout=subprocess.DEVNULL)
        return out.read_bytes()



