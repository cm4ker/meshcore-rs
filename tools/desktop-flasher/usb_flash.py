"""Install MeshCore-RS updates or merged ESP images over USB."""
import argparse
import asyncio
from contextlib import contextmanager, nullcontext, redirect_stdout
import csv
import hashlib
import io
import pathlib
import re
import struct
import sys
import tempfile
import zlib

import esptool
from esptool.bin_image import LoadFirmwareImage
from esptool.util import flash_size_bytes
from ota_app import SerialLink, OTA_INFO, running_sha8
from meshcore_ota import BOARDS, cli_progress, is_direct_esp_file, is_merged_esp_file

ROOT = pathlib.Path(__file__).parent
FACTORY_IMAGES = ROOT / "esp-factory"
if not FACTORY_IMAGES.is_dir():
    FACTORY_IMAGES = ROOT.parent / "android-flasher" / "assets" / "esp-factory"
CHIPS = {"t3s3": ("ESP32-S3", 9), "heltec-v3": ("ESP32-S3", 9),
         "heltec-v4": ("ESP32-S3", 9), "heltec-paper": ("ESP32-S3", 9),
         "xiao-c6": ("ESP32-C6", 13)}
BOARD_TITLES = {"t3s3": "LilyGO T3-S3", "heltec-v3": "Heltec V3", "heltec-v4": "Heltec V4",
                "heltec-paper": "Heltec Wireless Paper", "xiao-c6": "Seeed XIAO ESP32-C6"}
FACTORY_ROLES = {1: "Репитер", 2: "Компаньон"}
PARTITION_TABLE = 0x8000
SECTOR_SIZE = 4096
APP_ADDRESS = 0x10000
ESP_APP_DESCRIPTOR = bytes.fromhex("3254cdab")
HELTEC_V3_FLASH_SIZE = 8 * 1024 * 1024
HELTEC_V3_MESHCORE_LAYOUT = {
    "nvs": (1, 2, 0x9000, 0x5000),
    "otadata": (1, 0, 0xe000, 0x2000),
    "app0": (0, 16, APP_ADDRESS, 0x330000),
    "app1": (0, 17, 0x340000, 0x330000),
    "spiffs": (1, 130, 0x670000, 0x180000),
    "coredump": (1, 3, 0x7f0000, 0x10000),
}


def require(condition, message):
    if not condition:
        raise ValueError(message)


async def identify(port):
    link = await SerialLink.open(port)
    try:
        try:
            reply = await link.request(bytes([OTA_INFO]))
        except RuntimeError as exc:
            raise ValueError("Плата не ответила на запрос MeshCore-RS. Для первой установки .mcfw "
                             "выберите «Новая нода со сбросом».") from exc
        running = reply[5:].partition(b"\0")[2]
        require(len(running) >= 19, "Не удалось определить модель работающей платы.")
        return BOARDS.get(running[12]), int.from_bytes(running[:4], "little"), running[4:12]
    finally:
        await link.close()
        await asyncio.sleep(0.15)


def factory_target(header):
    require(len(header) >= 7 and header[:5] == b"MCFW\x01", "Неверный заголовок MCFW.")
    board = BOARDS.get(header[5])
    role = FACTORY_ROLES.get(header[6])
    require(board in CHIPS and role, "Файл .mcfw для другой платы или роли.")
    return board, role


def mcfw_payload(data, board):
    require(len(data) >= 54 and data[:5] == b"MCFW\x01", "Неверный заголовок MCFW.")
    require(BOARDS.get(data[5]) == board, "Файл выпуска предназначен для другой платы.")
    n = 53 + data[52]
    require(n < len(data), "Повреждён заголовок выпуска.")
    payload = data[n + 1 + 96 * data[n]:]
    require(len(payload) == int.from_bytes(data[12:16], "little")
            and hashlib.sha256(payload).digest() == data[16:48], "Не совпадает SHA-256 выпуска.")
    print(f"Выбран выпуск #{int.from_bytes(data[8:12], 'little')} для {board}.")
    return payload


def read_image(path, board):
    data = path.read_bytes()
    if path.suffix.lower() == ".mcfw":
        data = mcfw_payload(data, board)
    else:
        require(path.suffix.lower() == ".bin", "Выберите .mcfw или образ приложения .bin.")
    require(len(data) > 36 and data[0] == 0xe9 and data[32:36] == ESP_APP_DESCRIPTOR,
            "Нужен образ приложения ESP с дескриптором приложения.")
    require(int.from_bytes(data[12:14], "little") == CHIPS[board][1], "Образ для другого чипа.")
    return data


def partition_entries(raw):
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
    return entries


def layout(raw, board):
    expected = {}
    for row in csv.reader((ROOT / (board + ".csv")).read_text("utf-8").splitlines()):
        if not row or row[0].startswith("#"):
            continue
        name, kind, sub, addr, size = [v.strip() for v in row[:5]]
        expected[name] = ({"app": 0, "data": 1}[kind],
                          {"nvs": 2, "ota": 0, "ota_0": 16, "ota_1": 17, "undefined": 6}.get(sub),
                          int(addr, 0), int(size, 0))
    entries = partition_entries(raw)
    require(entries == expected, "Разметка платы отличается от схемы выпуска. Запись остановлена.")
    return entries


def validate_esp_image(data, chip, chip_id):
    require(len(data) > 36 and data[0] == 0xe9 and int.from_bytes(data[12:14], "little") == chip_id,
            "Повреждён образ ESP или указан другой чип.")
    image = LoadFirmwareImage(chip, io.BytesIO(data))
    require(image.checksum == image.calculate_checksum()
            and (not image.append_digest or image.calc_digest == image.stored_digest),
            "Ошибка контрольной суммы образа ESP.")
    return image.data_length + (32 if image.append_digest else 0)


def inspect_merged_image(data):
    require(len(data) > APP_ADDRESS + 36 and data[0] == 0xe9,
            "В полном *.bin должны быть загрузчик, таблица разделов и приложение ESP.")
    chip_id = int.from_bytes(data[12:14], "little")
    chip = {9: "ESP32-S3", 13: "ESP32-C6"}.get(chip_id)
    require(chip, "Полный .bin рассчитан на другой чип. Поддерживаются ESP32-S3 и ESP32-C6.")
    flash_code = data[3] >> 4
    require(flash_code <= 7, "Неизвестный размер флеш-памяти в образе ESP.")
    capacity = (1 << flash_code) * 1024 * 1024
    require(len(data) <= capacity, "Полный образ превышает размер флеш-памяти.")
    validate_esp_image(data[:PARTITION_TABLE], chip, chip_id)
    entries = partition_entries(data[PARTITION_TABLE:PARTITION_TABLE + SECTOR_SIZE])
    end = PARTITION_TABLE + SECTOR_SIZE
    for _, _, address, size in sorted(entries.values(), key=lambda entry: entry[2]):
        require(address >= end and size > 0 and address % SECTOR_SIZE == 0
                and size % SECTOR_SIZE == 0 and address + size <= capacity,
                "Разделы полного образа пересекаются или выходят за пределы флеш-памяти.")
        end = address + size
    app_size = next((size for kind, subtype, address, size in entries.values()
                     if kind == 0 and subtype in (0, 16) and address == APP_ADDRESS), 0)
    require(app_size > 0 and data[APP_ADDRESS + 32:APP_ADDRESS + 36] == ESP_APP_DESCRIPTOR,
            "В полном образе отсутствует приложение в первом загрузочном разделе.")
    validate_esp_image(data[APP_ADDRESS:APP_ADDRESS + app_size], chip, chip_id)
    return chip, capacity


def next_slot(records):
    valid = []
    for sector in range(2):
        offset = sector * SECTOR_SIZE
        seq, state, crc = struct.unpack_from("<I20xII", records, offset)
        if seq == 0xffffffff:
            continue
        # PlatformIO's boot_app0.bin uses a zero-sequence record with an erased state.
        initial_record = records[offset:offset + 32] == b"\0" * 4 + b"\xff" * 28
        require((initial_record or 0 < seq < 0xfffffffd)
                and crc == zlib.crc32(struct.pack("<I", seq), 0xffffffff),
                "Повреждена запись выбора загрузочного раздела.")
        if state in (0xffffffff, 0, 1, 2):
            valid.append((seq, sector))
    seq, sector = max(valid, default=(1, 1))
    return (seq-1) % 2, 1-sector, seq+1


class EspWriteOutput:
    """Forward esptool 4.8's text while relaying its write progress to the GUI."""

    def __init__(self, stream, progress):
        self.stream = stream
        self.progress = progress
        self.previous_step = 0

    def write(self, data):
        step = re.fullmatch(r"Writing at 0x[0-9a-f]+\.\.\. \((\d{1,3}) %\)", data.strip())
        if step:
            # esptool prints a block before sending it, so report the previous one.
            percent = int(step[1])
            if self.previous_step and percent >= self.previous_step:
                self.progress(self.previous_step, 100)
            self.previous_step = percent
        return self.stream.write(data)

    def flush(self):
        self.stream.flush()

    def isatty(self):
        return False


def write_verified(stub, chip, offset, path, *, progress=None):
    output = redirect_stdout(EspWriteOutput(sys.stdout, progress)) if progress else nullcontext()
    with output:
        esptool.main(["--chip", chip.lower().replace("-", ""), "--no-stub", "--after", "no_reset_stub",
                      "write_flash", "--compress", hex(offset), str(path)], esp=stub)
    data = path.read_bytes()
    require(stub.flash_md5sum(offset, len(data)).lower() == hashlib.md5(data).hexdigest(),
            "Контрольная сумма записанных данных не совпала.")
    if progress:
        progress(100, 100)


def select_boot_slot(stub, chip, otadata, sector, sequence, directory):
    flag = directory / "boot-slot.bin"
    record = struct.pack("<I", sequence)
    flag.write_bytes(record + b"\xff" * 20 + struct.pack("<II", 2, zlib.crc32(record, 0xffffffff)))
    write_verified(stub, chip, otadata + sector * SECTOR_SIZE, flag)


@contextmanager
def connect_stub(port, chip):
    rom = esptool.detect_chip(port, connect_attempts=3)
    try:
        require(rom.CHIP_NAME == chip and not rom.secure_download_mode
                and not rom.get_secure_boot_enabled() and not rom.get_flash_encryption_enabled(),
                "Модель или режим защиты ESP несовместимы с этой операцией.")
        yield rom.run_stub()
    finally:
        try:
            rom.hard_reset()
        finally:
            rom._port.close()


def install_full_image(args, data, *, erase, progress=None):
    chip, capacity = inspect_merged_image(data)
    print(f"Подключение к загрузчику {chip} на {args.port}…", flush=True)
    with tempfile.TemporaryDirectory() as tmp:
        # Use the validated bytes even if the original file changes during the operation.
        image = pathlib.Path(tmp) / "merged.bin"
        image.write_bytes(data)
        with connect_stub(args.port, chip) as stub:
            actual = flash_size_bytes(esptool.detect_flash_size(stub))
            require(actual is not None and actual >= capacity,
                    "Размер флеш-памяти платы не соответствует полному образу.")
            if args.check:
                print(f"Проверено. Полный образ {chip}, {len(data)} байт.")
                return
            if erase:
                print("Полная очистка флеш-памяти…", flush=True)
                stub.erase_flash()
            if progress:
                progress(0, 100)
            write_verified(stub, chip, 0, image, progress=progress)
    print("Запись полного образа проверена. Плата перезапускается.")


def install_factory(args, *, progress=None):
    require(args.image.suffix.lower() == ".mcfw", "Для первой установки выберите файл .mcfw.")
    release = args.image.read_bytes()
    board, role = factory_target(release)
    image = mcfw_payload(release, board)
    chip, chip_id = CHIPS[board]
    require(image[32:36] == ESP_APP_DESCRIPTOR and validate_esp_image(image, chip, chip_id) == len(image),
            "Файл должен содержать отдельный образ приложения ESP.")
    prefix = (FACTORY_IMAGES / f"{board}.bin").read_bytes()
    require(len(prefix) == PARTITION_TABLE + SECTOR_SIZE, "Повреждён встроенный образ загрузчика.")
    parts = layout(prefix[PARTITION_TABLE:], board)
    require(parts["ota_0"][2] == APP_ADDRESS and len(image) <= parts["ota_0"][3],
            "Приложение не помещается в первый загрузочный раздел.")
    print(f"Первая установка MeshCore-RS: {BOARD_TITLES[board]} · {role}.", flush=True)
    install_full_image(args, prefix.ljust(APP_ADDRESS, b"\xff") + image, erase=True, progress=progress)


def install_heltec_v3_app(args, *, progress=None):
    require(not args.new_node, "Для полной очистки Heltec V3 выберите *-merged.bin для своей модели.")
    chip, chip_id = CHIPS["heltec-v3"]
    data = args.image.read_bytes()
    require(data[32:36] == ESP_APP_DESCRIPTOR and validate_esp_image(data, chip, chip_id) == len(data),
            "Нужен отдельный образ приложения Heltec V3.")
    print(f"Подключение к загрузчику Heltec V3 на {args.port}…", flush=True)
    with tempfile.TemporaryDirectory() as tmp:
        directory = pathlib.Path(tmp)
        image = directory / "application.bin"
        image.write_bytes(data)
        with connect_stub(args.port, chip) as stub:
            actual = flash_size_bytes(esptool.detect_flash_size(stub))
            require(actual == HELTEC_V3_FLASH_SIZE, "Для этого образа Heltec V3 нужны 8 МиБ флеш-памяти.")
            parts = partition_entries(stub.read_flash(PARTITION_TABLE, SECTOR_SIZE))
            require(parts == HELTEC_V3_MESHCORE_LAYOUT,
                    "Разметка платы отличается от штатной MeshCore для Heltec V3. Для первой установки "
                    "или смены разметки выберите *-merged.bin для своей модели.")
            meta = parts["otadata"][2]
            active, sector, sequence = next_slot(stub.read_flash(meta, 2 * SECTOR_SIZE))
            _, _, address, size = parts[f"app{1 - active}"]
            require(len(data) <= size, "Образ не помещается в раздел приложения.")
            if args.check:
                print(f"Проверено. Heltec V3, раздел app{1 - active}, {len(data)} байт.")
                return
            if progress:
                progress(0, 100)
            write_verified(stub, chip, address, image, progress=progress)
            select_boot_slot(stub, chip, meta, sector, sequence, directory)
    print("Запись приложения проверена. Плата перезапускается.")


def install(args, *, progress=None):
    if getattr(args, "factory", False):
        return install_factory(args, progress=progress)
    if is_merged_esp_file(args.image):
        return install_full_image(args, args.image.read_bytes(), erase=args.new_node, progress=progress)
    if is_direct_esp_file(args.image):
        return install_heltec_v3_app(args, progress=progress)
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
        with connect_stub(args.port, chip) as stub:
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
            if progress:
                progress(0, 100)
            write_verified(stub, chip, target[2], app, progress=progress)
            if args.new_node:
                for name in ("nvs", "meshcore", "companion"):
                    _, _, address, size = parts[name]
                    print(f"Сброс данных узла: {name}…", flush=True)
                    stub.erase_region(address, size)
                    blank = hashlib.md5(b"\xff" * size).hexdigest()
                    require(stub.flash_md5sum(address, size).lower() == blank,
                            f"Не удалось подтвердить очистку раздела {name}. Повторите создание новой ноды.")
            select_boot_slot(stub, chip, meta, sector, seq, pathlib.Path(tmp))
    digest = asyncio.run(running_sha8(args))
    require(digest == hashlib.sha256(image).digest()[:8],
            "Запись выполнена, но запуск выбранного образа не подтверждён. Проверьте версию устройства.")
    print("Плата перезагрузилась и подтвердила выбранный образ.")


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--port", required=True)
    mode = parser.add_mutually_exclusive_group()
    mode.add_argument("--new-node", action="store_true")
    mode.add_argument("--factory", action="store_true", help="erase the entire ESP flash and install a full .mcfw release")
    parser.add_argument("--check", action="store_true")
    parser.add_argument("image", type=pathlib.Path)
    try:
        install(parser.parse_args(), progress=cli_progress())
    except Exception as exc:
        sys.exit(str(exc))


