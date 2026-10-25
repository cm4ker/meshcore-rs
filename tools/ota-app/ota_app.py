"""Update a companion running this firmware over its app link (USB serial, BLE or TCP), or a
repeater over BLE while its update window is open (`start ota ble` prints the pairing PIN).

The image travels as companion frames 0xF0-0xF5 (the requests of crates/meshcore/src/ota.rs, the
same as the LoRa update of a repeater), so no LoRa airtime is used.

    pip install meshcore            # pulls in bleak and pyserial
    espflash save-image --chip esp32c6 --flash-size 4mb <elf> firmware.bin
    python tools/uf2/elf2uf2.py <elf> firmware.uf2 --bin firmware.bin   # T-Echo
    python ota_app.py --port COM4 firmware.bin
    python ota_app.py --ble D8:E6:C5:14:21:A8 firmware.bin   # paired first: Windows Settings >
                                                             # Bluetooth > Add device, enter the PIN
    python ota_app.py --tcp 192.168.1.50:5000 firmware.bin
    python ota_app.py --port COM4 --info                     # what the device runs

Delta updates work as over LoRa: the device reports the SHA-256 of the image it runs, and if that
image is in the image store (--images, by default the board's firmware/models/<board>/images) only a
bsdiff patch is sent (tools/ota-delta). Every image sent is added to the store.

Signed releases: give the release's `.mcfw` file (tools/ota-sign) instead of the `.bin`; its
envelope goes first. A device that pins a root key (`ota_root` custom variable on a companion,
`set ota.root` on a repeater) only takes an image that key signed.
"""

import argparse
import asyncio
import contextlib
import hashlib
import math
import pathlib
import struct
import sys
import time
import zlib

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parents[1] / "ota-lora"))
from meshcore_ota import (  # noqa: E402
    DEFAULT_DELTA_TOOL, ERRORS, REPO, describe_device, describe_envelope, envelope_pieces, find_base, load_firmware, log,
    make_delta, store_image,
)

OTA_BEGIN, OTA_DATA, OTA_STATUS, OTA_COMMIT, OTA_ABORT, OTA_INFO, OTA_MANIFEST = range(0xF0, 0xF7)
# a piece of a release envelope: command + offset, total, length (5) + data, within 176 bytes
ENVELOPE_PIECE = 160
CMD_DEVICE_QUERY, RESP_DEVICE_INFO = 22, 13
# up to 176-byte frames: command + chunk index + data (a multiple of 4, for flash writes) + the
# CRC-32 of index and data, so a frame the link mangled is dropped and sent again
CHUNK = 168
FLAG_ZLIB, FLAG_DELTA, FLAG_CRC = 1, 2, 4
NUS_RX = "6e400002-b5a3-f393-e0a9-e50e24dcca9e"
NUS_TX = "6e400003-b5a3-f393-e0a9-e50e24dcca9e"


def sector_align(n):
    return (n + 4095) // 4096 * 4096


class Link:
    """Frames to and from the device; replies are matched by their first byte."""

    def __init__(self):
        self.frames = asyncio.Queue()

    async def request(self, frame, timeout=15):
        while not self.frames.empty():
            self.frames.get_nowait()
        await self.send(frame)
        end = time.time() + timeout
        while (left := end - time.time()) > 0:
            try:
                reply = await asyncio.wait_for(self.frames.get(), left)
            except asyncio.TimeoutError:
                break
            if reply[0] == frame[0] or (frame[0] == CMD_DEVICE_QUERY and reply[0] == RESP_DEVICE_INFO):
                return reply[1:]
        raise RuntimeError(f"no reply to 0x{frame[0]:02X}")


class StreamLink(Link):
    """USB serial and TCP: `<` + length + frame out, `>` + length + frame in."""

    def __init__(self, reader, writer):
        super().__init__()
        self.reader, self.writer = reader, writer
        self.task = asyncio.create_task(self.pump())

    async def pump(self):
        buf = b""
        while True:
            data = await self.reader.read(1024)
            if not data:
                return
            buf += data
            while True:
                start = buf.find(b">")
                if start < 0:
                    buf = b""
                    break
                buf = buf[start:]
                if len(buf) < 3:
                    break
                n = struct.unpack_from("<H", buf, 1)[0]
                if n > 300:
                    buf = buf[1:]
                    continue
                if len(buf) < 3 + n:
                    break
                await self.frames.put(buf[3:3 + n])
                buf = buf[3 + n:]

    async def send(self, frame):
        self.writer.write(b"<" + struct.pack("<H", len(frame)) + frame)
        await self.writer.drain()

    async def close(self):
        self.task.cancel()
        self.writer.close()


class SerialLink(StreamLink):
    @classmethod
    async def open(cls, port):
        import serial_asyncio_fast as serial_asyncio
        reader, writer = await serial_asyncio.open_serial_connection(url=port, baudrate=115200)
        # as the meshcore library does: DTR on, RTS off (RTS alone resets an ESP32 on USB-Serial-JTAG)
        writer.transport.serial.dtr = True
        writer.transport.serial.rts = False
        # writes that wait until they are done: the transport's non-blocking ones on Windows reuse
        # one OVERLAPPED, so a write issued while the device is busy (erasing flash) and not taking
        # data cuts the end off the one still pending
        writer.transport.serial.write_timeout = 10
        await asyncio.sleep(0.2)
        return cls(reader, writer)


class TcpLink(StreamLink):
    @classmethod
    async def open(cls, hostport):
        host, port = hostport.rsplit(":", 1)
        reader, writer = await asyncio.open_connection(host, int(port))
        return cls(reader, writer)


class BleLink(Link):
    """Nordic UART: write frames to RX (with response: the device acknowledges once queued)."""

    @classmethod
    async def open(cls, address):
        from bleak import BleakClient
        from bleak.exc import BleakError
        self = cls()
        self.client = BleakClient(address)
        for attempt in range(3):  # a device that has just rebooted may not be advertising yet
            try:
                await self.client.connect()
                break
            except (OSError, BleakError, asyncio.TimeoutError):
                if attempt == 2:
                    raise
                await asyncio.sleep(3)
        await self.client.start_notify(NUS_TX, lambda _, data: self.frames.put_nowait(bytes(data)))
        return self

    async def send(self, frame):
        await self.client.write_gatt_char(NUS_RX, frame, response=True)

    async def close(self):
        await self.client.disconnect()


async def open_link(args):
    if args.port:
        return await SerialLink.open(args.port)
    if args.ble:
        return await BleLink.open(args.ble)
    return await TcpLink.open(args.tcp)


async def running_sha8(args):
    """The start of the SHA-256 of the image the device runs, asked again after its reboot."""
    for _ in range(6):
        await asyncio.sleep(10)
        try:
            link = await open_link(args)
        except Exception:
            continue
        try:
            _, _, running = (await link.request(bytes([OTA_INFO])))[5:].partition(b"\0")
            return running[4:12] if len(running) >= 12 else None
        except Exception:
            pass
        finally:
            with contextlib.suppress(Exception):
                await link.close()
    return None


async def main():
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("image", nargs="?")
    link_args = ap.add_mutually_exclusive_group(required=True)
    link_args.add_argument("--port", help="USB serial port")
    link_args.add_argument("--ble", help="BLE address (pair it first)")
    link_args.add_argument("--tcp", help="host:port of the Wi-Fi interface")
    ap.add_argument("--images", type=pathlib.Path, help="image store (delta bases)")
    ap.add_argument("--delta-tool", type=pathlib.Path, default=DEFAULT_DELTA_TOOL)
    ap.add_argument("--no-delta", action="store_true", help="always send the whole image")
    ap.add_argument("--raw", action="store_true",
                    help="send the whole image uncompressed: the device only checks it at the end (slow, but "
                    "it gets firmware whose commit of a compressed update fails onto a new one)")
    ap.add_argument("--info", action="store_true", help="only show the device and the image it runs")
    args = ap.parse_args()
    if not args.info and not args.image:
        ap.error("an image is required (or --info)")

    link = await open_link(args)
    try:
        dev = await link.request(bytes([CMD_DEVICE_QUERY, 3]))
        model = dev[19:59].split(b"\0")[0].decode(errors="replace")
        version = dev[59:79].split(b"\0")[0].decode(errors="replace")
        log(f"device: {model}, firmware {version}")
        board = "xiao-c6" if "Xiao" in model else "t-echo" if "T-Echo" in model else "heltec-v4"
        images = args.images or REPO / "firmware" / "models" / board / "images"
        # the ESP boards keep the previous firmware and fall back to it; the T-Echo has room for one
        after = ("it installs the image and restarts" if board == "t-echo"
                 else "on trial until it has run healthy for 2 minutes")

        info = await link.request(bytes([OTA_INFO]))
        if not info[0]:
            sys.exit("the device has no update slot (single-slot partition table?)")
        capacity = struct.unpack_from("<I", info, 1)[0]
        _, _, running = info[5:].partition(b"\0")
        log(f"update slot {capacity // 1024} KiB")
        if len(running) >= 12:
            run_len, run_sha8 = struct.unpack_from("<I", running)[0], running[4:12]
            log(f"running image: {run_len} bytes, sha256 {run_sha8.hex()}..."
                + ("" if find_base(images, run_len, run_sha8) else f" (not in {images})"))
        roots, about = describe_device(running)
        log(about)
        if args.info:
            return

        image, envelope = load_firmware(args.image)
        sha = hashlib.sha256(image).digest()
        if envelope:
            log(describe_envelope(envelope))
        elif roots:
            sys.exit("the device only takes signed releases: give the release's .mcfw")
        if envelope:
            for piece in envelope_pieces(envelope, ENVELOPE_PIECE):
                reply = await link.request(bytes([OTA_MANIFEST]) + piece)
            if reply[0]:
                sys.exit(f"release refused: {ERRORS.get(reply[0], reply[0])}")
            log("release accepted" if roots else "release envelope sent (not needed: no root key pinned)")

        payload = zlib.compress(image, 9)
        how = f"zlib {len(image)} -> {len(payload)} bytes"
        begin = struct.pack("<I", len(payload)) + sha + struct.pack("<HBI", CHUNK, FLAG_ZLIB | FLAG_CRC, len(image))
        # the device stages a compressed stream at the end of the slot and inflates it to the start
        if args.raw or sector_align(len(payload)) + sector_align(len(image)) > capacity:
            payload = image
            how = f"raw {len(image)} bytes" + ("" if args.raw else " (image and its zlib stream do not both fit the slot)")
            begin = struct.pack("<I", len(payload)) + sha + struct.pack("<HBI", CHUNK, FLAG_CRC, 0)
        if len(running) >= 12 and not args.no_delta and not args.raw:
            run_len, run_sha8 = struct.unpack_from("<I", running)[0], running[4:12]
            base = find_base(images, run_len, run_sha8)
            if base == image:
                sys.exit("the device already runs this image")
            if base is None:
                log(f"running image ({run_len} bytes, sha256 {run_sha8.hex()}...) is not in {images}: whole image")
            else:
                delta = zlib.compress(make_delta(args.delta_tool, base, image), 9)
                if len(delta) < len(payload):
                    payload = delta
                    how = f"delta {len(base)} -> {len(image)} bytes as {len(delta)} bytes"
                    begin = struct.pack("<I", len(payload)) + sha + struct.pack("<H", CHUNK)
                    begin += struct.pack("<BII", FLAG_ZLIB | FLAG_DELTA | FLAG_CRC, len(image), len(base))
                    begin += hashlib.sha256(base).digest()

        total = math.ceil(len(payload) / CHUNK)
        reply = await link.request(bytes([OTA_BEGIN]) + begin, timeout=30)
        if reply[0]:
            sys.exit(f"begin failed: {ERRORS.get(reply[0], reply[0])}")
        log(f"session open: {how}, {total} chunks")

        start = time.time()
        missing = list(range(total))
        for round_no in range(50):
            for k, i in enumerate(missing):
                body = struct.pack("<H", i) + payload[i * CHUNK:(i + 1) * CHUNK]
                await link.send(bytes([OTA_DATA]) + body + struct.pack("<I", zlib.crc32(body)))
                if round_no == 0 and k % 500 == 499:
                    # asking for the status also keeps a serial link from queueing far ahead
                    reply = await link.request(bytes([OTA_STATUS, 0, 0]))
                    received = struct.unpack_from("<H", reply, 1)[0] if not reply[0] else k + 1
                    rate = received * CHUNK / max(time.time() - start, 1e-3) / 1024
                    log(f"device has {received}/{total} chunks ({rate:.1f} KiB/s)")
            reply = await link.request(bytes([OTA_STATUS, 0, 0]))
            if reply[0]:
                sys.exit(f"status failed: {ERRORS.get(reply[0], reply[0])}")
            received, tot, n = struct.unpack_from("<HHB", reply, 1)
            if received == tot:
                break
            missing = list(struct.unpack_from(f"<{n}H", reply, 6))
            log(f"device has {received}/{tot} chunks, resending {len(missing)}")
        else:
            sys.exit("transfer did not complete")
        took = max(time.time() - start, 1e-3)
        log(f"transfer done in {took:.0f} s ({len(payload) / took / 1024:.1f} KiB/s); "
            "committing (the device rebuilds and verifies the image)")
        try:
            reply = await link.request(bytes([OTA_COMMIT]), timeout=120)
        except Exception as e:  # rebuilding the image stalls the device; a BLE link may drop meanwhile
            log(f"no answer to the commit ({e or type(e).__name__}); asking again after the reboot")
            reply = None
        if reply is None:
            with contextlib.suppress(Exception):
                await link.close()
            link = None
            got = await running_sha8(args)
            if got is None:
                sys.exit("the device did not come back on this link (a repeater's BLE window closes when it "
                         "reboots): check `ver` and `ota info` on its console or over LoRa")
            if got != sha[:8]:
                sys.exit(f"the device runs {got.hex()}..., not the new image: the update did not take")
            store_image(images, image)
            log(f"the device runs the new image ({after})")
            return
        if reply[0]:
            sys.exit(f"commit failed: {ERRORS.get(reply[0], reply[0])}")
        store_image(images, image)
        log(f"image verified; the device reboots into it ({after})")
    finally:
        if link:
            await link.close()


if __name__ == "__main__":
    asyncio.run(main())
