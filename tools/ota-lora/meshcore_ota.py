"""Update a repeater running this firmware over LoRa, through any MeshCore companion radio.

The image travels as admin requests (REQ types 0xF0-0xF5, see crates/meshcore/src/repeater.rs):
encrypted with the admin's shared secret, so only a logged-in admin can push firmware.

    pip install meshcore
    espflash save-image --chip esp32s3 --flash-size 16mb <elf> firmware.bin
    python meshcore_ota.py --port COM4 --node PKIO --password-file pw.txt firmware.bin

Delta updates: the repeater reports the SHA-256 of the image it runs; if that image is in the
image store (--images, default firmware/models/heltec-v4/images), only a bsdiff patch is sent (made by
tools/ota-delta, `cargo build --release -p ota-delta`). Every image sent is added to the store;
add images flashed over USB with --register.

Progress lines go to stdout; the transfer can be re-run and resumes missing chunks only if the
repeater was not rebooted in between (sessions live in RAM).

Signed releases: give the release's `.mcfw` file (tools/ota-sign) instead of the `.bin`. Its
envelope goes first, and a repeater that pins a root key (`set ota.root`) only takes an image
that key signed.
"""

import argparse
import asyncio
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

from meshcore import EventType, MeshCore

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
          14: "unreadable release envelope"}
BOARDS = {1: "heltec-v4", 2: "heltec-v3", 3: "xiao-c6", 4: "t-echo"}
ROLES = {1: "repeater", 2: "companion"}
REPO = pathlib.Path(__file__).resolve().parents[2]
DEFAULT_IMAGES = REPO / "firmware" / "models" / "heltec-v4" / "images"
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
    """What INFO says after the running image: `[board][role][release counter u32][pinned roots]`."""
    if len(running) < 19:
        return None, "the device predates signed releases"
    board, role, counter, roots = struct.unpack_from("<BBIB", running, 12)
    pins = f"pins {roots} root key(s): signed releases only" if roots else "pins no root key: unsigned images accepted"
    return roots, f"device build {BOARDS.get(board, '?')}/{ROLES.get(role, '?')} #{counter}, {pins}"


def envelope_pieces(env, size):
    """MANIFEST request payloads: `[offset u16][total u16][len u8][piece]`."""
    for off in range(0, len(env), size):
        piece = env[off:off + size]
        yield struct.pack("<HHB", off, len(env), len(piece)) + piece


def store_image(images, image):
    """Keep `image` in the store under its SHA-256 prefix (a future delta base)."""
    images.mkdir(parents=True, exist_ok=True)
    path = images / f"{hashlib.sha256(image).hexdigest()[:16]}.bin"
    if not path.exists():
        path.write_bytes(image)
    return path


def find_base(images, length, sha8):
    """The stored image the repeater runs, by length and SHA-256 prefix."""
    if not images.is_dir():
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


class Ota:
    def __init__(self, mc, contact):
        self.mc = mc
        self.pubkey = bytes.fromhex(contact["public_key"])

    async def send(self, req_type, payload=b""):
        """Send a request; returns the expected response tag (hex) or None."""
        frame = bytes([CMD_SEND_BINARY_REQ]) + self.pubkey + bytes([req_type]) + payload
        res = await self.mc.commands.send(frame, [EventType.MSG_SENT, EventType.ERROR])
        if res.type != EventType.MSG_SENT:
            return None
        return res.payload["expected_ack"].hex()

    async def request(self, req_type, payload=b"", timeout=15, retries=4):
        """Request with response: returns the response body (after the 4-byte tag)."""
        for _ in range(retries):
            tag = await self.send(req_type, payload)
            if tag is None:
                await asyncio.sleep(1)
                continue
            ev = await self.mc.wait_for_event(EventType.BINARY_RESPONSE, attribute_filters={"tag": tag}, timeout=timeout)
            if ev is not None:
                return bytes.fromhex(ev.payload["data"])
        raise RuntimeError(f"no response to request 0x{req_type:02X}")


async def main():
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("image")
    ap.add_argument("--port")
    ap.add_argument("--node", help="repeater contact name (or public key prefix)")
    ap.add_argument("--password", help="admin password")
    ap.add_argument("--password-file")
    ap.add_argument("--gap-ms", type=int, default=150, help="extra gap between chunks")
    ap.add_argument("--no-compress", action="store_true", help="send the raw image (older firmware)")
    ap.add_argument("--probe", type=int, default=0, help="link test: send N chunks, report delivery, abort")
    ap.add_argument("--images", type=pathlib.Path, default=DEFAULT_IMAGES, help="image store (delta bases)")
    ap.add_argument("--delta-tool", type=pathlib.Path, default=DEFAULT_DELTA_TOOL)
    ap.add_argument("--no-delta", action="store_true", help="always send the whole image")
    ap.add_argument("--register", action="store_true", help="only add the image to the store (flashed over USB)")
    args = ap.parse_args()

    if args.register:
        log(f"stored as {store_image(args.images, load_firmware(args.image)[0])}")
        return
    if not args.port or not args.node:
        sys.exit("--port and --node are required")

    password = args.password
    if args.password_file:
        password = open(args.password_file, encoding="utf-8").read().strip()
    image, envelope = load_firmware(args.image)
    sha = hashlib.sha256(image).digest()  # always of the inflated image
    if envelope:
        log(describe_envelope(envelope))
    payload = image if args.no_compress else zlib.compress(image, 9)

    mc = await MeshCore.create_serial(args.port, 115200, only_error=True)
    try:
        info = (await mc.commands.send_appstart()).payload
        sf, bw, cr = info["radio_sf"], info["radio_bw"], info["radio_cr"]
        frame_ms = airtime_ms(2 + 4 + 16 * math.ceil((4 + 3 + CHUNK) / 16) + 2, sf, bw, cr)
        log(f"companion '{info['name']}' SF{sf} BW{bw} CR{cr}: ~{frame_ms:.0f} ms per chunk frame")

        await mc.ensure_contacts()
        contact = mc.get_contact_by_name(args.node) or mc.get_contact_by_key_prefix(args.node)
        if not contact:
            sys.exit(f"contact {args.node!r} not found on the companion")
        ota = Ota(mc, contact)

        login = await mc.commands.send_login_sync(contact, password, min_timeout=10)
        if login is None or login.type != EventType.LOGIN_SUCCESS:
            sys.exit("login failed")
        log(f"logged in to {contact['adv_name']}")

        body = await ota.request(REQ_OTA_INFO)
        if not body[0]:
            sys.exit("repeater has no OTA slot (old partition table?)")
        capacity = struct.unpack_from("<I", body, 1)[0]
        # `version [0 image_len sha8]`; decrypted payloads are zero-padded
        version, _, running = body[5:].partition(b"\0")
        log(f"repeater runs {version.decode(errors='replace')}, slot capacity {capacity // 1024} KiB")
        roots, about = describe_device(running)
        log(about)
        if roots and not envelope:
            sys.exit("the repeater only takes signed releases: give the release's .mcfw")
        if envelope and roots is None:
            # firmware from before signed releases knows no MANIFEST request: the image alone
            log("the repeater predates signed releases: sending the release's image without its envelope")
        elif envelope:
            for piece in envelope_pieces(envelope, ENVELOPE_PIECE):
                body = await ota.request(REQ_OTA_MANIFEST, piece)
            if body[0]:
                sys.exit(f"release refused: {ERRORS.get(body[0], body[0])}")
            log("release accepted" if roots else "release envelope sent (not needed: no root key pinned)")

        begin = struct.pack("<I", len(payload)) + sha + struct.pack("<H", CHUNK)
        how = "raw" if args.no_compress else f"zlib {len(image)} -> {len(payload)} bytes"
        if not args.no_compress:
            begin += struct.pack("<BI", 1, len(image))
            base = None
            # older firmware sends no image identity (only cipher padding after the version)
            if len(running) >= 12 and struct.unpack_from("<I", running)[0] and not args.no_delta:
                run_len, run_sha8 = struct.unpack_from("<I", running)[0], running[4:12]
                log(f"running image: {run_len} bytes, sha256 {run_sha8.hex()}...")
                base = find_base(args.images, run_len, run_sha8)
                if base is None:
                    log("running image is not in the store: sending the whole image")
            if base is not None:
                if base == image:
                    sys.exit("the repeater already runs this image")
                delta = zlib.compress(make_delta(args.delta_tool, base, image), 9)
                if len(delta) < len(payload):
                    payload = delta
                    how = f"delta {len(base)} -> {len(image)} bytes as {len(delta)} bytes"
                    begin = struct.pack("<I", len(payload)) + sha + struct.pack("<H", CHUNK)
                    begin += struct.pack("<BII", 3, len(image), len(base)) + hashlib.sha256(base).digest()
        total = math.ceil(len(payload) / CHUNK)
        body = await ota.request(REQ_OTA_BEGIN, begin, timeout=30)
        if body[0]:
            sys.exit(f"begin failed: {ERRORS.get(body[0], body[0])}")
        log(f"session open: {how}, {total} chunks, ETA ~{total * (frame_ms + args.gap_ms) / 60000:.0f} min")
        log(f"payload sha256 {hashlib.sha256(payload).hexdigest()[:16]}")

        async def send_chunk(i):
            data = payload[i * CHUNK:(i + 1) * CHUNK]
            await ota.send(REQ_OTA_DATA, struct.pack("<H", i) + data)
            await asyncio.sleep((frame_ms + args.gap_ms) / 1000)

        if args.probe:
            for i in range(min(args.probe, total)):
                await send_chunk(i)
            body = await ota.request(REQ_OTA_STATUS, struct.pack("<H", 0))
            received = struct.unpack_from("<H", body, 1)[0]
            log(f"probe: repeater got {received}/{min(args.probe, total)} chunks")
            await ota.request(REQ_OTA_ABORT)
            return

        start = time.time()
        for i in range(total):
            await send_chunk(i)
            if i % 100 == 99 or i == total - 1:
                pct = 100 * (i + 1) / total
                left = (time.time() - start) / (i + 1) * (total - i - 1)
                log(f"sent {i + 1}/{total} chunks ({pct:.0f}%), ~{left / 60:.0f} min left")

        for round_no in range(1, 30):
            body = await ota.request(REQ_OTA_STATUS, struct.pack("<H", 0))
            if body[0]:
                sys.exit(f"status failed: {ERRORS.get(body[0], body[0])}")
            received, tot, n = struct.unpack_from("<HHB", body, 1)
            missing = list(struct.unpack_from(f"<{n}H", body, 6))
            log(f"repeater has {received}/{tot} chunks")
            if received == tot:
                break
            log(f"resending {len(missing)} missing chunks (round {round_no})")
            for i in missing:
                await send_chunk(i)
        else:
            sys.exit("transfer did not complete")

        # rewriting the slot from a compressed stream or a delta takes ~20 s on the repeater
        log("committing (the repeater rebuilds and verifies the image)")
        body = await ota.request(REQ_OTA_COMMIT, timeout=90, retries=2)
        if body[0]:
            sys.exit(f"commit failed: {ERRORS.get(body[0], body[0])}")
        store_image(args.images, image)
        log("image verified by the repeater; it reboots into the new firmware (trial until healthy)")
    finally:
        await mc.disconnect()


if __name__ == "__main__":
    asyncio.run(main())
