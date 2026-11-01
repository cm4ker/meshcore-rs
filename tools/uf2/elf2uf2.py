"""Convert an ARM ELF firmware into a UF2 file for a UF2 bootloader (Adafruit nRF52 by default),
and optionally into the image for updates over the app link (tools/ota-app/ota_app.py).

Copy the UF2 onto the bootloader's drive (on the T-Echo: double-tap reset, drive TECHOBOOT).
No dependencies: the ELF's loadable segments are read directly.

Both carry the app's flash contents followed by a trailer (crates/meshcore/src/ota.rs,
`ImageFormat::Nrf`): `[b"MCRS-NRF"][app length u32][load address u32][chip u32][12 zero bytes]
[SHA-256 of the app]`. The trailer is flashed too: the firmware finds its own image by it, the
base of delta updates, whether it came over USB or over the air.

    python tools/uf2/elf2uf2.py firmware.elf firmware.uf2 [--bin firmware.bin] [--family 0xADA52840]
                                [--data node.bin@0xDA000]

`--data` puts more flash contents into the UF2 only (not into the update image): e.g. the node's
data region from tools/provision, so a node moved from upstream keeps its identity. The Adafruit
bootloader ignores blocks from 0xEA000 on, and a UF2 with any of them never completes.
"""
import argparse
import hashlib
import struct

UF2_MAGIC_START0 = 0x0A324655
UF2_MAGIC_START1 = 0x9E5D5157
UF2_MAGIC_END = 0x0AB16F30
UF2_FLAG_FAMILY = 0x00002000
PAYLOAD = 256
NRF52840_FAMILY = 0xADA52840
TRAILER_MAGIC = b"MCRS-NRF"
NRF52840_CHIP = 0x52840
# The Adafruit nRF52 bootloader's USER_FLASH_END: it drops UF2 blocks from here on (its
# CURRENT.UF2 ends here too), so the transfer would never count as complete
UF2_FLASH_END = 0xEA000


def load_segments(path):
    """(address, bytes) of every PT_LOAD segment with file contents, at its load (physical) address."""
    data = open(path, "rb").read()
    assert data[:4] == b"\x7fELF" and data[4] == 1 and data[5] == 1, "a little-endian ELF32"
    phoff, = struct.unpack_from("<I", data, 28)
    phentsize, phnum = struct.unpack_from("<HH", data, 42)
    segs = []
    for i in range(phnum):
        p_type, p_offset, _vaddr, p_paddr, p_filesz = struct.unpack_from("<IIIII", data, phoff + i * phentsize)
        if p_type == 1 and p_filesz:
            segs.append((p_paddr, data[p_offset:p_offset + p_filesz]))
    return sorted(segs)


def flat_image(segments, chip=NRF52840_CHIP):
    """The app from its lowest to its highest address (gaps 0xFF, padded to a word), then the
    trailer; and the load address."""
    start = segments[0][0]
    end = max(a + len(b) for a, b in segments)
    app = bytearray(b"\xff" * (end - start))
    for addr, blob in segments:
        app[addr - start:addr - start + len(blob)] = blob
    app += b"\xff" * (-len(app) % 4)
    trailer = TRAILER_MAGIC + struct.pack("<III", len(app), start, chip) + bytes(12) + hashlib.sha256(app).digest()
    return bytes(app + trailer), start


def to_uf2(regions, family):
    """UF2 blocks for `(address, bytes)` regions, numbered across all of them."""
    blocks = [(addr + i, data[i:i + PAYLOAD]) for addr, data in regions for i in range(0, len(data), PAYLOAD)]
    out = bytearray()
    for n, (addr, chunk) in enumerate(blocks):
        header = struct.pack("<IIIIIIII", UF2_MAGIC_START0, UF2_MAGIC_START1, UF2_FLAG_FAMILY,
                             addr, PAYLOAD, n, len(blocks), family)
        out += header + bytes(chunk).ljust(476, b"\x00") + struct.pack("<I", UF2_MAGIC_END)
    return bytes(out)


def data_region(arg):
    """`FILE@ADDR` -> (address, contents padded to whole UF2 payloads with 0xFF)."""
    path, _, addr = arg.rpartition("@")
    data = open(path, "rb").read()
    return int(addr, 0), data + b"\xff" * (-len(data) % PAYLOAD)


def main():
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("elf")
    ap.add_argument("uf2")
    ap.add_argument("--bin", help="also write the image for updates over the app link")
    ap.add_argument("--family", type=lambda s: int(s, 0), default=NRF52840_FAMILY)
    ap.add_argument("--data", action="append", default=[], metavar="FILE@ADDR", help="more flash contents for the UF2")
    args = ap.parse_args()
    image, start = flat_image(load_segments(args.elf))
    extra = [data_region(d) for d in args.data]
    for addr, data in [(start, image)] + extra:
        if addr + len(data) > UF2_FLASH_END:
            ap.error(f"0x{addr:05X}..0x{addr + len(data):05X} runs past 0x{UF2_FLASH_END:05X}, where the bootloader stops taking blocks")
    uf2 = to_uf2([(start, image)] + extra, args.family)
    open(args.uf2, "wb").write(uf2)
    end = start + len(image)
    print(f"{args.uf2}: 0x{start:05X}..0x{end:05X} ({end - start} bytes), {len(uf2) // 512} blocks")
    for addr, data in extra:
        print(f"  + 0x{addr:05X}..0x{addr + len(data):05X} ({len(data)} bytes)")
    if args.bin:
        open(args.bin, "wb").write(image)
        print(f"{args.bin}: update image, sha256 {hashlib.sha256(image).hexdigest()[:16]}...")


if __name__ == "__main__":
    main()
