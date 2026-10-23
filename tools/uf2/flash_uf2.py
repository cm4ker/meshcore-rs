"""Flash a UF2 onto a board with a UF2 bootloader (the T-Echo by default): if the bootloader's
drive is not there yet, a "1200 baud touch" on the board's USB port restarts it into the
bootloader (this firmware and Adafruit's do that); then the file is copied onto the drive.

    python tools/uf2/flash_uf2.py firmware.uf2 [--drive TECHOBOOT] [--vid 0x239A --pid 0x8029]
"""
import argparse
import ctypes
import shutil
import string
import sys
import time

import serial
from serial.tools import list_ports


def board_port(vid, pid):
    return next((p.device for p in list_ports.comports() if p.vid == vid and p.pid == pid), None)


def find_drive(label):
    for letter in string.ascii_uppercase:
        root = f"{letter}:\\"
        buf = ctypes.create_unicode_buffer(64)
        if ctypes.windll.kernel32.GetVolumeInformationW(root, buf, 64, None, None, None, None, 0) and buf.value == label:
            return root
    return None


def touch_1200(port):
    try:
        s = serial.Serial()
        s.port, s.baudrate, s.dtr = port, 1200, True
        s.open()
        time.sleep(0.2)
        s.dtr = False
        time.sleep(0.2)
        s.close()
    except serial.SerialException:
        pass  # the board may restart under the open port


def main():
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("uf2")
    ap.add_argument("--drive", default="TECHOBOOT", help="volume label of the bootloader's drive")
    ap.add_argument("--vid", type=lambda s: int(s, 0), default=0x239A)
    ap.add_argument("--pid", type=lambda s: int(s, 0), default=0x8029)
    args = ap.parse_args()

    drive = find_drive(args.drive)
    if drive is None:
        port = board_port(args.vid, args.pid)
        if port is None:
            sys.exit(f"neither the board's USB port nor the {args.drive} drive: double-tap reset")
        touch_1200(port)
        end = time.time() + 20
        while time.time() < end and (drive := find_drive(args.drive)) is None:
            time.sleep(0.3)
        if drive is None:
            sys.exit(f"{args.drive} did not appear: double-tap reset")
    time.sleep(0.8)
    try:
        shutil.copyfile(args.uf2, drive + "firmware.uf2")
    except OSError:
        pass  # the bootloader restarts as soon as the image is complete
    print("copied to", drive)
    end = time.time() + 30
    while time.time() < end and (port := board_port(args.vid, args.pid)) is None:
        time.sleep(0.3)
    print("running" + (f" on {port}" if port else ": no USB port yet"))


if __name__ == "__main__":
    main()
