"""Select and download board-specific firmware from MeshCore-RS GitHub releases."""

import hashlib
import json
from pathlib import Path
import re
import struct
import tempfile
from urllib.error import HTTPError, URLError
from urllib.parse import urlsplit
from urllib.request import HTTPRedirectHandler, Request, build_opener

from meshcore_ota import BOARDS, ROLES


API = "https://api.github.com/repos/cm4ker/meshcore-rs/releases?per_page=20"
DOWNLOAD_PREFIX = "/cm4ker/meshcore-rs/releases/download/"
MAX_SIZE = 8 * 1024 * 1024
MAX_ENVELOPE = 54 + 31 + 8 * 96
UF2_BOARDS = {4, 5, 7, 9}
ROLE_LABELS = {1: "Репитер", 2: "Компаньон"}
BUILD_NUMBER = re.compile(r"-r([0-9]{1,10})$")
DELTA_BASE = re.compile(r"\.from-r([0-9]{1,10})\.mcfd$")


def device_summary(model, version, capacity, running):
    size = int.from_bytes(running[:4], "little") if len(running) >= 12 else 0
    base_hash = running[4:12].hex() if len(running) >= 12 else ""
    board, role, build, roots = struct.unpack_from("<BBIB", running, 12) if len(running) >= 19 else (0, 0, 0, None)
    parts = [model or BOARDS.get(board, "MeshCore")]
    if version:
        parts.append(version)
    if build:
        parts.append(f"Сборка №{build}")
    if role in ROLE_LABELS:
        parts.append(ROLE_LABELS[role])
    if capacity:
        parts.append(f"слот {capacity // 1024} КиБ")
    return dict(model=model, version=version, board=board, role=role, build=build, capacity=capacity,
                running_size=size, base_hash=base_hash, roots=roots, summary=" · ".join(parts))


def uf2_target(board_slug, role_name):
    board = next((n for n in UF2_BOARDS if BOARDS[n] == board_slug), 0)
    role = next((n for n, name in ROLES.items() if name == role_name), 0)
    if not board or not role:
        raise ValueError("Выберите поддерживаемую модель платы и роль UF2.")
    return dict(device_summary(board_slug, "", 0, b""), board=board, role=role, uf2=True)


class HttpsRedirect(HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        target = urlsplit(newurl)
        if target.scheme != "https" or target.netloc not in (
            "github.com", "api.github.com", "objects.githubusercontent.com", "release-assets.githubusercontent.com"
        ):
            raise ValueError("GitHub перенаправил загрузку на неожиданный адрес.")
        return super().redirect_request(req, fp, code, msg, headers, newurl)


def fetch(url):
    request = Request(url, headers={"User-Agent": "ota-app-desktop", "Accept": "application/vnd.github+json" if url == API else "application/octet-stream"})
    try:
        with build_opener(HttpsRedirect()).open(request, timeout=25) as response:
            body = response.read(MAX_SIZE + 1)
    except HTTPError as exc:
        if exc.code in (403, 429):
            raise ValueError("GitHub ограничил запросы. Попробуйте позже.") from exc
        raise ValueError(f"GitHub вернул ошибку HTTP {exc.code}.") from exc
    except URLError as exc:
        raise ValueError(f"Не удалось подключиться к GitHub: {exc.reason}") from exc
    if len(body) > MAX_SIZE:
        raise ValueError("Файл превышает допустимый размер.")
    return body


def build_number(pattern, text):
    match = pattern.search(text)
    number = int(match[1]) if match else 0
    return number if number <= 0xFFFFFFFF else 0


def update_issue(device, asset):
    if device.get("uf2"):
        return ""
    if not device["capacity"]:
        return "На плате нет слота обновления"
    if device.get("tcp") and not device["roots"]:
        return "Для Wi-Fi / TCP нужен доверенный ключ выпуска"
    current, release, board = device["build"], asset["build"], device["board"]
    if not current or not release:
        return "Номер сборки неизвестен"
    if release == current:
        return "Эта сборка уже на плате"
    if release < current:
        return "Старее установленной сборки"
    if asset["format"] == ".mcfd":
        if not device["running_size"] or device["base_hash"] in ("", "0000000000000000"):
            return "Плата не сообщила данные исходной прошивки"
    elif board in (7, 9) or (board == 5 and current >= 142):
        return "Для обновления этой платы нужна дельта .mcfd"
    elif asset["size"] - MAX_ENVELOPE > device["capacity"]:
        return "Образ больше слота обновления"
    return ""


def parse(releases, device):
    board, role = device["board"], device["role"]
    if board not in BOARDS or role not in ROLES:
        raise ValueError("Плата не сообщила поддерживаемую модель или роль прошивки.")
    prefix = f"{BOARDS[board]}-{ROLES[role]}"
    assets = []
    for release in releases:
        if release.get("draft"):
            continue
        tag = release["tag_name"]
        for metadata in release["assets"]:
            name = metadata["name"]
            extension = Path(name).suffix.casefold()
            size = metadata["size"]
            base = build_number(DELTA_BASE, name)
            if not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9._-]{0,254}", name) or not 0 < size <= MAX_SIZE:
                continue
            if device.get("uf2"):
                if name != prefix + ".uf2":
                    continue
            elif not name.startswith((prefix + "-", prefix + ".")) or not (
                extension == ".mcfw" or (extension == ".mcfd" and base and base == device["build"])
            ):
                continue
            asset = dict(name=name, url=metadata["browser_download_url"], digest=metadata.get("digest") or "",
                         tag=tag, size=size, build=build_number(BUILD_NUMBER, tag), board=board, role=role,
                         base=base, prerelease=bool(release.get("prerelease")), format=extension)
            asset["issue"] = update_issue(device, asset)
            assets.append(asset)
    assets.sort(key=lambda asset: (-asset["build"], asset["format"] != ".mcfd"))
    compatible = [i for i, asset in enumerate(assets) if not asset["issue"]]
    recommended = next((i for i in compatible if not assets[i]["prerelease"]), compatible[0] if compatible else None)
    return dict(device=device, assets=assets, recommended=recommended)


def catalog(device):
    if device["board"] not in BOARDS or device["role"] not in ROLES:
        raise ValueError("Плата не сообщила поддерживаемую модель или роль прошивки.")
    return parse(json.loads(fetch(API)), device)


def asset_url(asset):
    name, board, role = asset["name"], asset["board"], asset["role"]
    url = urlsplit(asset["url"])
    prefix = f"{BOARDS.get(board, '')}-{ROLES.get(role, '')}"
    if (board not in BOARDS or role not in ROLES or not name.startswith((prefix + "-", prefix + ".")) or
            not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9._-]{0,254}", name) or url.scheme != "https" or
            url.netloc != "github.com" or url.query or url.fragment or
            not url.path.startswith(DOWNLOAD_PREFIX) or url.path.rsplit("/", 1)[-1] != name or
            len(url.path[len(DOWNLOAD_PREFIX):].split("/")) != 2 or ".." in url.path.split("/")):
        raise ValueError("Неверный адрес файла выпуска.")
    return asset["url"]


def validate_firmware(asset, data):
    extension = Path(asset["name"]).suffix.casefold()
    if extension == ".uf2":
        if asset["board"] not in UF2_BOARDS or not data or len(data) % 512:
            raise ValueError("Неверный формат UF2.")
        for offset in range(0, len(data), 512):
            if (struct.unpack_from("<II", data, offset) != (0x0A324655, 0x9E5D5157) or
                    struct.unpack_from("<I", data, offset + 508)[0] != 0x0AB16F30):
                raise ValueError("Повреждён блок UF2.")
        return
    if extension not in (".mcfw", ".mcfd") or len(data) < 54 or data[:5] != b"MCFW\x01" or data[52] > 31:
        raise ValueError("Неверный формат подписанной прошивки.")
    board, role, _, build, size = struct.unpack_from("<BBBII", data, 5)
    if (board, role, build) != (asset["board"], asset["role"], asset["build"]):
        raise ValueError("Сборка, модель платы или роль файла не соответствуют выпуску.")
    signature_count = 53 + data[52]
    if signature_count >= len(data) or not 0 < data[signature_count] <= 8:
        raise ValueError("Неверный конверт подписанной прошивки.")
    end = signature_count + 1 + 96 * data[signature_count]
    if extension == ".mcfd":
        if not size or len(data) < end + 38 or not int.from_bytes(data[end:end+4], "little"):
            raise ValueError("Неверная длина дельты MCFD.")
    elif not size or len(data) - end != size or hashlib.sha256(data[end:]).digest() != data[16:48]:
        raise ValueError("Размер или контрольная сумма образа не совпадает с манифестом.")


def download(asset, directory):
    url = asset_url(asset)
    if not 0 < asset["size"] <= MAX_SIZE:
        raise ValueError("Недопустимый размер файла выпуска.")
    data = fetch(url)
    if len(data) != asset["size"]:
        raise ValueError("Файл загружен не полностью.")
    digest = asset["digest"]
    if digest and digest.lower() != "sha256:" + hashlib.sha256(data).hexdigest():
        raise ValueError("Контрольная сумма загрузки не совпадает.")
    validate_firmware(asset, data)
    directory = Path(directory).expanduser().resolve()
    directory.mkdir(parents=True, exist_ok=True)
    folder = Path(tempfile.mkdtemp(prefix="release-", dir=directory))
    path = folder / asset["name"]
    try:
        path.write_bytes(data)
    except OSError:
        path.unlink(missing_ok=True)
        folder.rmdir()
        raise
    return path
