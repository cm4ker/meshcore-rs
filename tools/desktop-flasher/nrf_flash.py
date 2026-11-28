"""Install an application UF2 on supported Nordic bootloaders by USB or BLE."""

import argparse
import asyncio
import binascii
import hashlib
import json
import os
from pathlib import Path
import re
import struct
import sys
import zipfile

from ble_scan import BluetoothScanUnavailable, discover_ble
from meshcore_ota import cli_progress


BOARDS = {
    "heltec-t096": "Heltec T096",
    "heltec-t114": "Heltec T114",
    "promicro": "Pro Micro (FakeTec)",
    "t-echo": "LilyGO T-Echo",
}

ADVERTISEMENTS = {
    "heltec-t096": {"t096dfu", "heltect096dfu", "htn5262gota", "htnrf5262gota"},
    "heltec-t114": {"t114dfu", "heltect114dfu", "htn5262ota", "htnrf5262ota"},
    "promicro": {"promdfu", "promicrodfu", "promicroota", "faketecdfu", "faketecota"},
    "t-echo": {"lgtedfu", "techodfu", "techoota", "lilygotechodfu"},
}

DFU = "00001530-1212-efde-1523-785feabcd123"
CONTROL = "00001531-1212-efde-1523-785feabcd123"
PACKET = "00001532-1212-efde-1523-785feabcd123"
APP_START = 0x26000
APP_END = 0xEA000
MAX_UF2 = 8 * 1024 * 1024


def _normalized(text):
    return re.sub(r"[^a-z0-9]", "", (text or "").lower())


def _advertised_board(name):
    name = _normalized(name)
    return next((board for board, aliases in ADVERTISEMENTS.items() if name in aliases), None)


def _usb_board(model, board_id):
    identity = _normalized(f"{model} {board_id}")
    if any(alias in identity for alias in ("t096", "n5262g", "nrf5262g")):
        return "heltec-t096"
    if "t114" in identity:
        return "heltec-t114"
    if "promicro" in identity or "faketec" in identity:
        return "promicro"
    if ("techo" in identity and "+" not in model
            and not any(alias in identity for alias in ("techolite", "techoplus", "ttgoeink"))):
        return "t-echo"
    # HT-n5262 alone does not distinguish all Heltec boards.
    return None


def _file_board(path):
    name = _normalized(Path(path).name)
    for board, aliases in {
        "heltec-t096": ("heltect096", "t096"),
        "heltec-t114": ("heltect114", "t114"),
        "promicro": ("promicro", "faketec", "nrf52840promicro"),
        "t-echo": ("techo", "lilygotecho"),
    }.items():
        if name.startswith(aliases):
            return board
    return None


def _check_board(path, selected, detected):
    if selected is not None and selected not in BOARDS:
        raise ValueError("Выберите поддерживаемую модель платы.")
    if selected and detected and selected != detected:
        raise ValueError(f"Загрузчик определён как {BOARDS[detected]}. Проверьте выбранную модель.")
    board = detected or selected
    if board is None:
        raise ValueError("Модель загрузчика не определилась. Укажите свою плату вручную.")
    target = _file_board(path)
    if target and target != board:
        raise ValueError(f"Прошивка для {BOARDS[target]}, выбрана плата {BOARDS[board]}.")
    return board


def _read_uf2(path):
    with Path(path).open("rb") as source:
        raw = source.read(MAX_UF2 + 1)
    if not raw or len(raw) > MAX_UF2 or len(raw) % 512:
        raise ValueError("Некорректный размер UF2.")
    count = len(raw) // 512
    regions = []
    seen = set()
    for pos in range(0, len(raw), 512):
        start, magic, flags, address, size, index, total, family = struct.unpack_from("<8I", raw, pos)
        tail, = struct.unpack_from("<I", raw, pos + 508)
        if (start, magic, tail) != (0x0A324655, 0x9E5D5157, 0x0AB16F30):
            raise ValueError("Повреждён блок UF2.")
        if flags != 0x2000 or family != 0xADA52840:
            raise ValueError("Нужен UF2 приложения nRF52840.")
        if not 0 < size <= 476 or not APP_START <= address <= APP_END - size:
            raise ValueError("UF2 выходит за область приложения S140 6.1.1.")
        if total != count or index >= count or index in seen:
            raise ValueError("В UF2 не хватает блоков или повторяется их номер.")
        seen.add(index)
        regions.append((address, raw[pos + 32:pos + 32 + size]))

    image = bytearray()
    for address, payload in sorted(regions):
        if address != APP_START + len(image):
            raise ValueError("Нужен непрерывный образ приложения с адреса 0x26000.")
        image.extend(payload)
    if len(image) < 8 or len(image) % 4:
        raise ValueError("Некорректный размер образа приложения.")
    stack, reset = struct.unpack_from("<II", image)
    if not (0x20000000 < stack <= 0x20040000 and stack % 4 == 0
            and reset & 1 and APP_START <= reset < APP_START + len(image)):
        raise ValueError("Неверная таблица запуска приложения.")

    # The project trailer identifies the unpadded MeshCore-RS image and its checksum.
    start = len(image) - 64
    for pos in range(start, max(0, start - 475) - 1, -1):
        if image[pos:pos + 8] != b"MCRS-NRF":
            continue
        image_size, address, chip = struct.unpack_from("<III", image, pos + 8)
        if image_size != pos:
            continue
        if ((address, chip) != (APP_START, 0x52840)
                or hashlib.sha256(image[:pos]).digest() != image[pos + 32:pos + 64]):
            raise ValueError("Контрольная сумма или адрес образа MeshCore-RS не совпадает.")
        if any(byte not in (0, 255) for byte in image[pos + 64:]):
            raise ValueError("Лишние данные после образа приложения.")
        image = image[:pos + 64]
        break
    return raw, bytes(image)


def _init_packet(image):
    crc = binascii.crc_hqx(image, 0xFFFF)
    return struct.pack("<HHIHHH", 0x52, 0xFFFF, 0xFFFFFFFF, 1, 0xB6, crc), crc


def export_dfu_zip(path, output):
    """Save a Nordic Legacy DFU application archive."""
    _, image = _read_uf2(path)
    if Path(path).resolve() == Path(output).resolve():
        raise ValueError("Укажите отдельный путь для DFU ZIP.")
    init, crc = _init_packet(image)
    manifest = {"manifest": {"application": {
        "bin_file": "firmware.bin", "dat_file": "firmware.dat", "init_packet_data": {
            "application_version": 0xFFFFFFFF, "device_revision": 0xFFFF, "device_type": 0x52,
            "firmware_crc16": crc, "softdevice_req": [0xB6],
        },
    }, "dfu_version": 0.5}}
    with zipfile.ZipFile(output, "w", compression=zipfile.ZIP_DEFLATED) as archive:
        archive.writestr("firmware.bin", image)
        archive.writestr("firmware.dat", init)
        archive.writestr("manifest.json", json.dumps(manifest))


def inspect_uf2_drive(root):
    """Read the model, board slug and bootloader versions of a mounted UF2 volume."""
    root = Path(root).resolve(strict=True)
    if not root.is_dir() or not os.path.ismount(root):
        raise ValueError("Выберите корень съёмного диска загрузчика.")
    info_file = next((item for item in root.iterdir() if item.name.lower() == "info_uf2.txt"), None)
    if info_file is None or not info_file.is_file() or info_file.is_symlink():
        raise ValueError("На выбранном диске нет INFO_UF2.TXT.")
    with info_file.open("rb") as source:
        content = source.read(4097)
    if len(content) > 4096:
        raise ValueError("Некорректный размер INFO_UF2.TXT.")
    content = content.decode("utf-8-sig")
    version = re.search(r"(?im)^[ \t]*UF2 Bootloader(?:[ \t]*:[ \t]*|[ \t]+)(\S+)", content)
    fields = {}
    for line in content.splitlines():
        key, sep, value = line.partition(":")
        if sep:
            fields[key.strip().lower()] = value.strip()
    model = fields.get("model", "")
    board_id = fields.get("board-id", fields.get("board id", ""))
    if not version or not (model or board_id):
        raise ValueError("В INFO_UF2.TXT нет модели и версии загрузчика.")
    board = _usb_board(model, board_id)
    identity = _normalized(f"{model} {board_id}")
    if board is None and not any(chip in identity for chip in ("nrf52840", "n5262", "nrf5262")):
        raise ValueError("Диск не определён как UF2-загрузчик nRF52840.")
    softdevice = fields.get("softdevice", "")
    if softdevice and not re.fullmatch(r"S140\b.*\b6\.1\.1\b.*", softdevice, re.IGNORECASE):
        raise ValueError(f"Загрузчик сообщает {softdevice}. Требуется S140 6.1.1.")
    return {"model": model or board_id, "board": board, "version": version[1], "softdevice": softdevice}


def write_uf2(path, root, board=None, log=print, *, progress=None):
    """Validate the selected volume and copy the original UF2 bytes to it."""
    raw, _ = _read_uf2(path)
    root = Path(root).resolve(strict=True)
    info = inspect_uf2_drive(root)
    _check_board(path, board, info["board"])
    written = 0
    last = 0
    try:
        with (root / "UPDATE.UF2").open("xb", buffering=0) as output:
            if progress:
                progress(0, len(raw))
            while written < len(raw):
                count = output.write(raw[written:written + 16 * 1024])
                if not count:
                    raise OSError("Диск перестал принимать данные.")
                written += count
                if progress:
                    progress(written, len(raw))
                percent = written * 100 // len(raw)
                if percent // 10 > last:
                    last = percent // 10
                    log(f"UF2 передано {percent} %")
            os.fsync(output.fileno())
    except FileExistsError as error:
        raise OSError("На диске уже есть UPDATE.UF2. Переведите плату в USB-загрузчик заново.") from error
    except OSError as error:
        if written == len(raw):
            raise OSError("Диск не подтвердил запись. Проверьте версию после перезагрузки платы.") from error
        raise OSError(f"Записано {written // 1024} из {len(raw) // 1024} КиБ. {error}") from error
    log("Диск принял UF2. Проверьте версию после перезагрузки платы.")


async def scan_dfu(timeout=10):
    found = await discover_ble(timeout)
    items = []
    for device, advertisement in found.values():
        name = advertisement.local_name or device.name
        board = _advertised_board(name) or _advertised_board(device.name)
        legacy = any(uuid.lower() == DFU for uuid in advertisement.service_uuids)
        if legacy or board or (name and "dfu" in name.lower()):
            items.append({"address": device.address, "name": name or "Nordic DFU", "board": board})
    return sorted(items, key=lambda item: (item["name"].casefold(), item["address"]))


def _receipt(reply, sent):
    if reply[:1] != b"\x11" or len(reply) < 5:
        raise OSError(f"После передачи {sent} байт получен неожиданный ответ DFU: {reply.hex(' ')}.")
    received, = struct.unpack_from("<I", reply, 1)
    if received != sent:
        raise OSError(f"Передано {sent} байт, загрузчик подтвердил {received}.")


async def _transfer_legacy(image, control, packet, notice, log, *, progress=None):
    async def response(command, reply=None):
        if reply is None:
            reply = await notice()
        if len(reply) != 3 or reply[:2] != bytes((16, command)):
            raise OSError(f"Неожиданный ответ DFU на команду {command}: {reply.hex(' ')}.")
        if command == 1 and reply[2] == 2:
            await control(b"\x06")
            raise OSError("Загрузчик занят и перезапущен. Снова откройте BLE OTA и найдите плату.")
        if reply[2] != 1:
            raise OSError(f"Загрузчик отклонил команду {command} (статус {reply[2]}).")

    log("Согласование начала DFU…")
    await control(b"\x01\x04")
    await packet(struct.pack("<III", 0, 0, len(image)))
    await response(1)
    init, _ = _init_packet(image)
    await control(b"\x02\x00")
    await packet(init)
    await control(b"\x02\x01")
    await response(2)
    # An acknowledgement after every packet lets small bootloader queues keep up.
    await control(b"\x08\x01\x00")
    await control(b"\x03")
    if progress:
        progress(0, len(image))
    last_reported_percent = 0
    last = 0
    for offset in range(0, len(image), 20):
        payload = image[offset:offset + 20]
        await packet(payload)
        sent = offset + len(payload)
        if sent < len(image):
            reply = await notice()
            if reply[:1] == b"\x10":
                await response(3, reply)
                raise OSError(f"Загрузчик завершил приём после {sent} байт.")
            _receipt(reply, sent)
        percent = sent * 100 // len(image)
        if progress and sent < len(image) and percent > last_reported_percent:
            last_reported_percent = percent
            progress(sent, len(image))
        if percent // 10 > last:
            last = percent // 10
            log(f"Передано {percent} %")

    reply = await notice()
    if reply[:1] == b"\x11":
        _receipt(reply, len(image))
        reply = await notice()
    await response(3, reply)
    if progress:
        progress(len(image), len(image))
    await control(b"\x04")
    await response(4)
    log("Загрузчик подтвердил образ.")
    await control(b"\x05")
    log("Отправлена команда перезагрузки. Проверьте версию после запуска платы.")


async def install_dfu(address, path, board, log=print, *, progress=None):
    from bleak import BleakClient, BleakScanner
    from bleak.exc import BleakError

    _check_board(path, board, None)
    _, image = _read_uf2(path)
    advertised = None

    def matches(device, advertisement):
        nonlocal advertised
        if device.address.casefold() != address.casefold():
            return False
        advertised = _advertised_board(advertisement.local_name) or _advertised_board(device.name)
        return True

    device = await BleakScanner.find_device_by_filter(matches, timeout=10)
    if device is None:
        raise OSError("Загрузчик не найден. Включите BLE OTA на плате и повторите поиск.")
    _check_board(path, board, advertised)
    replies = asyncio.Queue()
    loop = asyncio.get_running_loop()

    def disconnected(_):
        loop.call_soon_threadsafe(replies.put_nowait, None)

    def notification(_, value):
        loop.call_soon_threadsafe(replies.put_nowait, bytes(value))

    async with BleakClient(device, disconnected_callback=disconnected) as client:
        service = client.services.get_service(DFU)
        if service is None:
            raise OSError("Нет службы Nordic Legacy DFU. Откройте BLE OTA на плате.")
        command_char = service.get_characteristic(CONTROL)
        packet_char = service.get_characteristic(PACKET)
        if command_char is None or packet_char is None:
            raise OSError("Загрузчик не предоставил характеристики Nordic Legacy DFU.")
        await client.start_notify(command_char, notification)

        async def control(data):
            try:
                await asyncio.wait_for(client.write_gatt_char(command_char, data, response=True), 15)
            except (OSError, BleakError):
                if data[0] not in (5, 6) or client.is_connected:
                    raise

        async def packet(data):
            await asyncio.wait_for(client.write_gatt_char(packet_char, data, response=False), 15)

        async def notice():
            try:
                reply = await asyncio.wait_for(replies.get(), 60)
            except asyncio.TimeoutError as error:
                raise OSError("Истекло время ожидания ответа DFU.") from error
            if reply is None:
                raise OSError("Загрузчик отключился.")
            return reply

        await _transfer_legacy(image, control, packet, notice, log, progress=progress)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    commands.add_parser("scan")
    commands.add_parser("inspect").add_argument("root")
    dfu = commands.add_parser("dfu")
    dfu.add_argument("address")
    dfu.add_argument("path")
    dfu.add_argument("board", choices=BOARDS)
    write = commands.add_parser("write")
    write.add_argument("path")
    write.add_argument("root")
    write.add_argument("--board", choices=BOARDS)
    export = commands.add_parser("export")
    export.add_argument("path")
    export.add_argument("output")
    args = parser.parse_args()
    progress = cli_progress()
    try:
        if args.command == "scan":
            print(json.dumps(asyncio.run(scan_dfu()), ensure_ascii=False))
        elif args.command == "inspect":
            print(json.dumps(inspect_uf2_drive(args.root), ensure_ascii=False))
        elif args.command == "dfu":
            asyncio.run(install_dfu(args.address, args.path, args.board, progress=progress))
        elif args.command == "write":
            write_uf2(args.path, args.root, args.board, progress=progress)
        elif args.command == "export":
            export_dfu_zip(args.path, args.output)
            print("DFU ZIP сохранён.")
    except BluetoothScanUnavailable as error:
        print(error, file=sys.stderr)
        return 1
    except Exception as error:
        print(f"Ошибка: {error or type(error).__name__}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
