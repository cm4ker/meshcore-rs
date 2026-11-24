"""Local ESP serial installation for two-slot meshcore-rs boards."""
import argparse
import asyncio
import csv
import hashlib
import pathlib
import struct
import sys
import tempfile
import zlib

import esptool
from esptool.bin_image import LoadFirmwareImage
from ota_app import SerialLink, OTA_INFO, running_sha8
from meshcore_ota import BOARDS

ROOT = pathlib.Path(__file__).parent
CHIPS = {"t3s3": ("ESP32-S3", 9), "heltec-v3": ("ESP32-S3", 9),
         "heltec-v4": ("ESP32-S3", 9), "heltec-paper": ("ESP32-S3", 9),
         "xiao-c6": ("ESP32-C6", 13)}


def require(condition, message):
    if not condition:
        raise ValueError(message)


async def identify(port):
    link = await SerialLink.open(port)
    try:
        reply = await link.request(bytes([OTA_INFO]))
        running = reply[5:].partition(b"\0")[2]
        require(len(running) >= 19, "Не удалось определить модель работающей платы.")
        return BOARDS.get(running[12]), int.from_bytes(running[:4], "little"), running[4:12]
    finally:
        await link.close()
        await asyncio.sleep(0.15)


def read_image(path, board):
    data = path.read_bytes()
    if path.suffix.lower() == ".mcfw":
        require(len(data) >= 54 and data[:5] == b"MCFW\x01", "Неверный заголовок MCFW.")
        require(BOARDS.get(data[5]) == board, "Файл выпуска предназначен для другой платы.")
        n = 53 + data[52]
        require(n < len(data), "Повреждён заголовок выпуска.")
        payload = data[n + 1 + 96 * data[n]:]
        require(len(payload) == int.from_bytes(data[12:16], "little")
                and hashlib.sha256(payload).digest() == data[16:48], "Не совпадает SHA-256 выпуска.")
        print(f"Выбран выпуск #{int.from_bytes(data[8:12], 'little')} для {board}.")
        data = payload
    else:
        require(path.suffix.lower() == ".bin", "Выберите .mcfw или образ приложения .bin.")
    require(len(data) > 36 and data[0] == 0xe9 and data[32:36] == bytes.fromhex("3254cdab"),
            "Нужен образ приложения ESP с дескриптором приложения.")
    require(int.from_bytes(data[12:14], "little") == CHIPS[board][1], "Образ для другого чипа.")
    return data


def layout(raw, board):
    expected = {}
    for row in csv.reader((ROOT / (board + ".csv")).read_text("utf-8").splitlines()):
        if not row or row[0].startswith("#"):
            continue
        name, kind, sub, addr, size = [v.strip() for v in row[:5]]
        expected[name] = ({"app": 0, "data": 1}[kind],
                          {"nvs": 2, "ota": 0, "ota_0": 16, "ota_1": 17, "undefined": 6}.get(sub),
                          int(addr, 0), int(size, 0))
    entries = {}
    for pos in range(0, len(raw)-31, 32):
        magic, kind, sub, addr, size, label, flags = struct.unpack_from("<HBBII16sI", raw, pos)
        if magic == 0xebeb:
            require(raw[pos+16:pos+32] == hashlib.md5(raw[:pos]).digest(), "Ошибка контрольной суммы таблицы.")
            break
        if magic == 0xffff:
            break
        require(magic == 0x50aa and not flags, "Разметка или флаги разделов не поддерживаются.")
        name = label.rstrip(b"\0").decode("ascii")
        require(name not in entries, "Повтор имени раздела.")
        entries[name] = (kind, sub, addr, size)
    require(entries == expected, "Разметка платы отличается от схемы выпуска. Запись остановлена.")
    return entries


def next_slot(records):
    valid = []
    for sector in range(2):
        seq, state, crc = struct.unpack_from("<I20xII", records, sector * 4096)
        if seq == 0xffffffff:
            continue
        require(0 < seq < 0xfffffffd and crc == zlib.crc32(struct.pack("<I", seq), 0xffffffff),
                "Повреждена запись выбора загрузочного раздела.")
        if state in (0xffffffff, 0, 1, 2):
            valid.append((seq, sector))
    seq, sector = max(valid, default=(1, 1))
    return (seq-1) % 2, 1-sector, seq+1


def write_verified(stub, chip, offset, path):
    esptool.main(["--chip", chip.lower().replace("-", ""), "--no-stub", "--after", "no_reset_stub",
                  "write_flash", "--compress", hex(offset), str(path)], esp=stub)
    data = path.read_bytes()
    require(stub.flash_md5sum(offset, len(data)).lower() == hashlib.md5(data).hexdigest(),
            "Контрольная сумма записанных данных не совпала.")


def install(args):
    board, running_size, running_hash = asyncio.run(identify(args.port))
    require(board in CHIPS, f"Неподдерживаемая модель: {board}.")
    chip = CHIPS[board][0]
    image = read_image(args.image, board)
    with tempfile.TemporaryDirectory() as tmp:
        app = pathlib.Path(tmp) / "application.bin"
        app.write_bytes(image)
        parsed = LoadFirmwareImage(chip, str(app))
        require(parsed.append_digest and parsed.data_length + 32 == len(image),
                "Нужен отдельный образ приложения ESP с SHA-256, без других разделов.")
        require(parsed.checksum == parsed.calculate_checksum()
                and (not parsed.append_digest or parsed.calc_digest == parsed.stored_digest),
                "Ошибка контрольной суммы образа ESP.")
        print(f"Подключение к загрузчику {board} на {args.port}…", flush=True)
        rom = esptool.detect_chip(args.port, connect_attempts=3)
        try:
            require(rom.CHIP_NAME == chip and not rom.secure_download_mode
                    and not rom.get_secure_boot_enabled() and not rom.get_flash_encryption_enabled(),
                    "Модель или режим защиты ESP несовместимы с этой операцией.")
            stub = rom.run_stub()
            stub.change_baud(460800)
            parts = layout(stub.read_flash(0x8000, 4096), board)
            meta = parts["otadata"][2]
            active, sector, seq = next_slot(stub.read_flash(meta, 8192))
            old = parts[f"ota_{active}"]
            target = parts[f"ota_{1-active}"]
            require(0 < running_size <= old[3] and len(image) <= target[3], "Размер образа вне раздела.")
            require(hashlib.sha256(stub.read_flash(old[2], running_size)).digest()[:8] == running_hash,
                    "Не удалось подтвердить активный раздел. Запись остановлена.")
            print(f"Проверено: {board}, слот ota_{1-active}, {len(image)} байт.", flush=True)
            if args.check:
                return
            write_verified(stub, chip, target[2], app)
            if args.new_node:
                for name in ("nvs", "meshcore", "companion"):
                    _, _, address, size = parts[name]
                    print(f"Сброс данных узла: {name}…", flush=True)
                    stub.erase_region(address, size)
                    blank = hashlib.md5(b"\xff" * size).hexdigest()
                    require(stub.flash_md5sum(address, size).lower() == blank,
                            f"Не удалось подтвердить очистку раздела {name}. Повторите создание новой ноды.")
            flag = pathlib.Path(tmp) / "boot-slot.bin"
            sequence = struct.pack("<I", seq)
            flag.write_bytes(sequence + b"\xff"*20 + struct.pack("<II", 2, zlib.crc32(sequence, 0xffffffff)))
            write_verified(stub, chip, meta+sector*4096, flag)
        finally:
            try:
                rom.hard_reset()
            finally:
                rom._port.close()
    digest = asyncio.run(running_sha8(args))
    require(digest == hashlib.sha256(image).digest()[:8],
            "Запись выполнена, но запуск выбранного образа не подтверждён. Проверьте версию устройства.")
    print("Плата перезагрузилась и подтвердила выбранный образ.")


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--port", required=True)
    parser.add_argument("--new-node", action="store_true")
    parser.add_argument("--check", action="store_true")
    parser.add_argument("image", type=pathlib.Path)
    try:
        install(parser.parse_args())
    except Exception as exc:
        sys.exit(str(exc))


