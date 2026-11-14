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
    python ota_app.py --ble <address> --pin 123456 release.mcfw   # or pair here (Windows), with
                                                                  # the PIN `fw ble` replied
    python ota_app.py --tcp 192.168.1.50:5000 firmware.bin
    python ota_app.py --port COM4 --info                     # what the device runs
    python ota_app.py --list                                 # serial ports, MeshCore devices on BLE

Every release also carries this script as a program for Windows, macOS and Linux (ota-app-*,
built by tools/ota-app/build.py), with no Python needed: the same options, `ota-app` in place of
`python ota_app.py`.

Delta updates work as over LoRa: the device reports the SHA-256 of the image it runs, and if that
image is in the image store (--images, by default the board's firmware/models/<board>/images) only a
bsdiff patch is sent (tools/ota-delta). Every image sent is added to the store. The program has no
store unless --images names one: it sends whole images.

Signed releases: give the release's `.mcfw` file (tools/ota-sign) instead of the `.bin`; its
envelope goes first. A device that pins a root key (`fw_root` custom variable on a companion,
`set fw.root` on a repeater) only takes an image that key signed.
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

# run as the release's program (PyInstaller), away from the repository and its image stores
FROZEN = getattr(sys, "frozen", False)
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

    async def request(self, frame, timeout=15, confirm=True):
        while not self.frames.empty():
            self.frames.get_nowait()
        await self.send(frame, confirm)
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

    async def send(self, frame, confirm=True):
        self.writer.write(b"<" + struct.pack("<H", len(frame)) + frame)
        await self.writer.drain()

    async def close(self):
        self.task.cancel()
        self.writer.close()


async def ota_status(link, tries=4):
    """OTA_STATUS, asked again if it goes unanswered: a device behind a USB-serial bridge that is
    busy writing flash loses bytes, its frame parser then takes a stray '<' in the chunks that
    follow for a frame and swallows what comes until the link is quiet for half a second, the
    request too."""
    for i in range(tries):
        try:
            return await link.request(bytes([OTA_STATUS, 0, 0]), timeout=5)
        except RuntimeError:
            if i == tries - 1:
                raise


class SerialLink(StreamLink):
    @classmethod
    async def open(cls, port):
        import serial
        import serial.tools.list_ports
        import serial_asyncio_fast as serial_asyncio
        # as the meshcore library does: DTR on, RTS off (RTS alone resets an ESP32 on USB-Serial-JTAG).
        # Behind a CP210x bridge (Heltec V3, Wireless Paper) both go to the auto-reset circuit:
        # RTS pulls EN low and DTR GPIO0, and esptool's way into the bootloader is just a reset
        # released with DTR on, so there both stay off from the moment the port opens.
        cp210x = any(p.device == port and p.vid == 0x10C4 for p in serial.tools.list_ports.comports())
        ser = serial.serial_for_url(port, baudrate=115200, do_not_open=True)
        ser.dtr = not cp210x
        ser.rts = False
        ser.open()
        loop = asyncio.get_running_loop()
        reader = asyncio.StreamReader()
        protocol = asyncio.StreamReaderProtocol(reader)
        transport = serial_asyncio.SerialTransport(loop, protocol, ser)
        writer = asyncio.StreamWriter(transport, protocol, reader, loop)
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


async def pair_windows(address, pin):
    """Pair this Windows PC with the device by passkey entry (bleak only does Just Works), dropping
    an earlier bond first: a repeater forgets its bonds when it restarts (the Pro Micro), and
    Windows would keep offering the stale key."""
    from bleak import BleakScanner
    from winrt.windows.devices.bluetooth import BluetoothAddressType, BluetoothLEDevice
    from winrt.windows.devices.enumeration import (
        DevicePairingKinds, DevicePairingProtectionLevel, DevicePairingResultStatus,
    )
    if await BleakScanner.find_device_by_address(address, timeout=20) is None:
        sys.exit(f"{address} is not advertising: is its update window open (`fw ble`)?")
    raw = int(address.replace(":", ""), 16)
    dev = await BluetoothLEDevice.from_bluetooth_address_async(raw)
    if dev is None:  # a static random address, as the nRF52 boards have
        dev = await BluetoothLEDevice.from_bluetooth_address_with_bluetooth_address_type_async(raw, BluetoothAddressType.RANDOM)
    if dev is None:
        sys.exit(f"Windows does not find {address}")
    pairing = dev.device_information.pairing
    if pairing.is_paired:
        await pairing.unpair_async()
    custom = pairing.custom
    token = custom.add_pairing_requested(lambda _, req: req.accept_with_pin(pin))
    try:
        result = await custom.pair_with_protection_level_async(
            DevicePairingKinds.PROVIDE_PIN, DevicePairingProtectionLevel.ENCRYPTION_AND_AUTHENTICATION)
    finally:
        custom.remove_pairing_requested(token)
    if result.status not in (DevicePairingResultStatus.PAIRED, DevicePairingResultStatus.ALREADY_PAIRED):
        sys.exit(f"pairing failed: {result.status.name}")
    log("paired with the PIN")


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

    async def send(self, frame, confirm=True):
        await self.client.write_gatt_char(NUS_RX, frame, response=confirm)

    async def close(self):
        await self.client.disconnect()


async def open_link(args):
    if args.port:
        return await SerialLink.open(args.port)
    if args.ble:
        if args.pin and not getattr(args, "paired", False):
            if sys.platform != "win32":
                sys.exit("--pin pairs on Windows only. On macOS leave it out: the system asks for the PIN when it "
                         "connects. On Linux pair first (bluetoothctl, `pair <address>`, type the PIN), then "
                         "run without --pin.")
            await pair_windows(args.ble, args.pin)
            args.paired = True
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


async def list_devices():
    """The serial ports, and the MeshCore devices advertising over BLE."""
    import serial.tools.list_ports
    # USB ones only: Linux lists thirty-odd /dev/ttyS* of the board's own UARTs, nothing behind them
    ports = sorted((p for p in serial.tools.list_ports.comports() if p.vid is not None), key=lambda p: p.device)
    log("USB serial ports (--port):" if ports else "no USB serial ports")
    for p in ports:
        usb = f"  USB {p.vid:04X}:{p.pid:04X}" if p.vid is not None else ""
        print(f"  {p.device}  {p.description}{usb}")
    log("MeshCore devices advertising over BLE (--ble), 10 s:")
    try:
        from bleak import BleakScanner
        found = await BleakScanner.discover(timeout=10, return_adv=True)
    except Exception as e:  # no adapter, or Bluetooth off
        log(f"no BLE: {e or type(e).__name__}")
        return
    devices = sorted(((adv.rssi, d.address, adv.local_name or d.name or "") for d, adv in found.values()
                      if (adv.local_name or d.name or "").startswith("MeshCore")), reverse=True)
    for rssi, address, name in devices:
        print(f"  {address}  {name}  RSSI {rssi}")
    if not devices:
        print("  none (a repeater advertises only while its update window is open: `fw ble`)")


async def main():
    ap = argparse.ArgumentParser(description=" ".join(__doc__.split("\n\n")[0].split()))
    ap.add_argument("image", nargs="?", help="the release's .mcfw (or an image .bin)")
    link_args = ap.add_mutually_exclusive_group()
    link_args.add_argument("--port", help="USB serial port")
    link_args.add_argument("--ble", help="BLE address (pair it first)")
    link_args.add_argument("--tcp", help="host:port of the Wi-Fi interface")
    ap.add_argument("--pin", help="with --ble on Windows: pair with this PIN first (the one `fw ble` replied)")
    ap.add_argument("--images", type=pathlib.Path, help="image store (delta bases)")
    ap.add_argument("--delta-tool", type=pathlib.Path, default=DEFAULT_DELTA_TOOL)
    ap.add_argument("--no-delta", action="store_true", help="always send the whole image")
    ap.add_argument("--raw", action="store_true",
                    help="send the whole image uncompressed: the device only checks it at the end (slow, but "
                    "it gets firmware whose commit of a compressed update fails onto a new one)")
    ap.add_argument("--info", action="store_true", help="only show the device and the image it runs")
    ap.add_argument("--list", action="store_true", help="list the serial ports and the MeshCore devices on BLE")
    args = ap.parse_args()
    if args.list:
        return await list_devices()
    if not (args.port or args.ble or args.tcp):
        ap.error("say how to reach the device: --port, --ble or --tcp (--list shows what is there)")
    if not args.info and not args.image:
        ap.error("an image is required (or --info)")

    link = await open_link(args)
    try:
        dev = await link.request(bytes([CMD_DEVICE_QUERY, 3]))
        model = dev[19:59].split(b"\0")[0].decode(errors="replace")
        version = dev[59:79].split(b"\0")[0].decode(errors="replace")
        log(f"device: {model}, firmware {version}")
        board = ("xiao-c6" if "Xiao" in model else "t-echo" if "T-Echo" in model
                 else "promicro" if "ProMicro" in model else "heltec")
        images = args.images or (None if FROZEN else REPO / "firmware" / "models" / board / "images")
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
                + ("" if images is None or find_base(images, run_len, run_sha8) else f" (not in {images})"))
        roots, about = describe_device(running)
        log(about)
        repeater = len(running) >= 19 and running[13] == 1
        if args.info:
            return

        image, envelope = load_firmware(args.image)
        sha = hashlib.sha256(image).digest()
        if len(running) >= 12 and running[4:12] == sha[:8]:
            sys.exit("the device already runs this image")
        if envelope:
            log(describe_envelope(envelope))
        elif roots:
            sys.exit("the device only takes signed releases: give the release's .mcfw")
        if envelope and roots is None:
            # firmware from before signed releases knows no MANIFEST request: the image alone
            log("the device predates signed releases: sending the release's image without its envelope")
        elif envelope:
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
        if images is not None and len(running) >= 12 and not args.no_delta and not args.raw:
            run_len, run_sha8 = struct.unpack_from("<I", running)[0], running[4:12]
            base = find_base(images, run_len, run_sha8)
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
                    reply = await ota_status(link)
                    received = struct.unpack_from("<H", reply, 1)[0] if not reply[0] else k + 1
                    rate = received * CHUNK / max(time.time() - start, 1e-3) / 1024
                    log(f"device has {received}/{total} chunks ({rate:.1f} KiB/s)")
            reply = await ota_status(link)
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
            # Sent without the BLE write's own confirmation: the device builds and checks the image
            # before it can confirm anything, and on an S3 that takes about 30 s, as long as a BLE
            # write may wait (the write then failed although the commit went through). Its answer
            # comes as a notification once it is done.
            reply = await link.request(bytes([OTA_COMMIT]), timeout=120, confirm=False)
        except Exception as e:  # rebuilding the image stalls the device; a BLE link may drop meanwhile
            log(f"no answer to the commit ({e or type(e).__name__})")
            reply = None
        if reply is None:
            with contextlib.suppress(Exception):
                await link.close()
            link = None
            if args.ble and repeater:
                sys.exit("a repeater's BLE window closes when it reboots, so this link cannot tell whether the "
                         "image took: check `ver` and `fw` on its console or over LoRa")
            log("asking again after the reboot")
            got = await running_sha8(args)
            if got is None:
                sys.exit("the device did not come back on this link (a repeater's BLE window closes when it "
                         "reboots): check `ver` and `fw` on its console or over LoRa")
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
