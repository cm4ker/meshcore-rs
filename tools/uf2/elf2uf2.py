"""Convert an ARM ELF firmware into a UF2 file for a UF2 bootloader (Adafruit nRF52 by default).

Copy the result onto the bootloader's drive (on the T-Echo: double-tap reset, drive TECHOBOOT).
No dependencies: the ELF's loadable segments are read directly.

    python tools/uf2/elf2uf2.py firmware.elf firmware.uf2 [--family 0xADA52840]
"""
import argparse
import struct

UF2_MAGIC_START0 = 0x0A324655
UF2_MAGIC_START1 = 0x9E5D5157
UF2_MAGIC_END = 0x0AB16F30
UF2_FLAG_FAMILY = 0x00002000
PAYLOAD = 256
NRF52840_FAMILY = 0xADA52840


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


def to_uf2(segments, family):
    # one flat image from the lowest to the highest address, gaps filled with 0xFF
    start = segments[0][0]
    end = max(a + len(b) for a, b in segments)
    image = bytearray(b"\xff" * (end - start))
    for addr, blob in segments:
        image[addr - start:addr - start + len(blob)] = blob
    blocks = [image[i:i + PAYLOAD] for i in range(0, len(image), PAYLOAD)]
    out = bytearray()
    for n, chunk in enumerate(blocks):
        header = struct.pack("<IIIIIIII", UF2_MAGIC_START0, UF2_MAGIC_START1, UF2_FLAG_FAMILY,
                             start + n * PAYLOAD, PAYLOAD, n, len(blocks), family)
        out += header + bytes(chunk).ljust(476, b"\x00") + struct.pack("<I", UF2_MAGIC_END)
    return bytes(out), start, end


def main():
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("elf")
    ap.add_argument("uf2")
    ap.add_argument("--family", type=lambda s: int(s, 0), default=NRF52840_FAMILY)
    args = ap.parse_args()
    uf2, start, end = to_uf2(load_segments(args.elf), args.family)
    open(args.uf2, "wb").write(uf2)
    print(f"{args.uf2}: 0x{start:05X}..0x{end:05X} ({end - start} bytes), {len(uf2) // 512} blocks")


if __name__ == "__main__":
    main()
