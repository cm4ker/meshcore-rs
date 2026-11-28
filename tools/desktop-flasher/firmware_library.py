"""Keep verified GitHub releases available without a network connection."""

import argparse
import base64
import hashlib
import json
import os
from pathlib import Path
import re
import struct
import sys
import tempfile

from firmware_catalog import (API, BOARDS, ROLES, ROLE_LABELS, UF2_BOARDS, MAX_SIZE,
                              BUILD_NUMBER, DELTA_BASE, asset_url, build_number, download_data,
                              fetch, update_issue, validate_firmware)
from meshcore_ota import cli_progress


BOARD_LABELS = {
    1: "Heltec V4", 2: "Heltec V3", 3: "Seeed XIAO ESP32-C6", 4: "LilyGO T-Echo",
    5: "Pro Micro (FakeTec)", 6: "Heltec Wireless Paper", 7: "Heltec T114",
    8: "LilyGO T3-S3", 9: "Heltec T096", 10: "Heltec Mesh Pocket",
}
FORMATS = (".mcfw", ".mcfd", ".uf2")
FILENAME = re.compile(r"[A-Za-z0-9][A-Za-z0-9._-]{0,254}")
RELEASE_TAG = re.compile(r"[A-Za-z0-9](?:[A-Za-z0-9._-]{0,94}[A-Za-z0-9])?")
IDENTITY = re.compile(r"^(" + "|".join(map(re.escape, BOARDS.values())) + r")-(repeater|companion)(?=[.-])")
BOARD_IDS = {slug: board for board, slug in BOARDS.items()}
ROLE_IDS = {name: role for role, name in ROLES.items()}
DEFAULT_DIR = Path.home() / ".ota-app" / "firmware"


def identify_name(name):
    match = IDENTITY.match(name)
    if not match or not FILENAME.fullmatch(name):
        return None
    board, role = BOARD_IDS[match[1]], ROLE_IDS[match[2]]
    extension = Path(name).suffix.casefold()
    if extension not in FORMATS:
        return None
    if extension == ".uf2" and (board not in UF2_BOARDS or name != f"{match[1]}-{match[2]}.uf2"):
        return None
    base = build_number(DELTA_BASE, name)
    if extension == ".mcfd" and not base:
        return None
    return dict(name=name, board=board, role=role, format=extension, base=base)


def build_index(releases):
    versions = []
    for release in releases:
        tag = release["tag_name"]
        if release.get("draft") or not RELEASE_TAG.fullmatch(tag):
            continue
        prerelease = bool(release.get("prerelease"))
        build = build_number(BUILD_NUMBER, tag)
        assets = []
        for metadata in release["assets"]:
            asset = identify_name(metadata["name"])
            if not asset or not 0 < metadata["size"] <= MAX_SIZE or (asset["format"] != ".uf2" and not build):
                continue
            asset.update(tag=tag, build=build, prerelease=prerelease, size=metadata["size"],
                         url=metadata["browser_download_url"], digest=metadata.get("digest") or "")
            try:
                asset_url(asset)
            except ValueError:
                continue
            assets.append(asset)
        if assets:
            versions.append(dict(tag=tag, build=build, prerelease=prerelease, assets=assets))
    return sorted(versions, key=lambda r: (r["prerelease"], -r["build"]))


def write_atomic(path, data):
    path.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.NamedTemporaryFile(dir=path.parent, delete=False) as file:
        temp = Path(file.name)
        try:
            file.write(data)
            file.close()
            os.replace(temp, path)
        finally:
            temp.unlink(missing_ok=True)


def write_json(path, data):
    write_atomic(path, json.dumps(data, ensure_ascii=False, separators=(",", ":")).encode("utf-8"))


def index(directory=DEFAULT_DIR):
    versions = build_index(json.loads(fetch(API)))
    if not versions:
        raise ValueError("В выпусках GitHub не найдено файлов для офлайн-набора.")
    write_json(Path(directory) / "library" / "catalog.json", versions)
    brief = [
        {**version, "assets": [{k: a[k] for k in ("board", "role", "size")} for a in version["assets"]]}
        for version in versions
    ]
    return dict(versions=brief, boards=BOARD_LABELS)


def paths(asset, directory):
    if not RELEASE_TAG.fullmatch(asset["tag"]) or not identify_name(asset["name"]):
        raise ValueError("Неверное имя сохранённой прошивки.")
    folder = Path(directory) / "library" / ("release-" + asset["tag"])
    return folder / asset["name"], folder / (asset["name"] + ".json")


def make_record(asset, data):
    record = {k: asset[k] for k in ("name", "board", "role", "format", "base", "tag", "build", "prerelease", "size")}
    record["digest"] = "sha256:" + hashlib.sha256(data).hexdigest()
    if record["format"] != ".uf2":
        record["image_size"] = int.from_bytes(data[12:16], "little")
    if record["format"] == ".mcfd":
        count = 53 + data[52]
        offset = count + 1 + 96 * data[count]
        record["base_size"] = int.from_bytes(data[offset:offset+4], "little")
        record["base_hash"] = data[offset+4:offset+12].hex()
    return record


def read_record(path):
    if not path.is_file() or path.is_symlink() or path.stat().st_size > 8192:
        return None
    record = json.loads(path.read_bytes())
    if not isinstance(record, dict):
        return None
    firmware = path.with_suffix("")
    identity = identify_name(firmware.name)
    if (not identity or any(record.get(k) != v for k, v in identity.items()) or not firmware.is_file() or
            firmware.is_symlink() or not 0 < firmware.stat().st_size == record.get("size") <= MAX_SIZE or
            not isinstance(record.get("build"), int) or not isinstance(record.get("tag"), str) or
            not isinstance(record.get("digest"), str) or not re.fullmatch(r"sha256:[0-9a-f]{64}", record["digest"])):
        return None
    return dict(record, path=str(firmware.resolve()))


def verify(path, digest, directory=DEFAULT_DIR):
    file, root = Path(path).resolve(), Path(directory).expanduser().resolve()
    if (not file.is_relative_to(root) or not identify_name(file.name) or not file.is_file() or
            not 0 < file.stat().st_size <= MAX_SIZE or "sha256:" + hashlib.sha256(file.read_bytes()).hexdigest() != digest):
        raise ValueError("Сохранённый файл повреждён или удалён. Скачайте его заново.")
    return file


def store(asset, directory=DEFAULT_DIR, *, progress=None):
    directory = Path(directory).expanduser().resolve()
    file, metadata = paths(asset, directory)
    try:
        saved = read_record(metadata)
        if (saved and saved["size"] == asset["size"] and
                (not asset["digest"] or saved["digest"] == asset["digest"].lower())):
            verify(file, saved["digest"], directory)
            return file, False
    except (OSError, ValueError):
        pass
    data = download_data(asset, progress=progress)
    write_atomic(file, data)
    write_json(metadata, make_record(asset, data))
    return file, True


def sync(tag, board, role, directory=DEFAULT_DIR, *, progress=None):
    versions = json.loads((Path(directory) / "library" / "catalog.json").read_bytes())
    version = next((v for v in versions if v["tag"] == tag), None)
    plan = [a for a in version["assets"] if (not board or board == a["board"]) and (not role or role == a["role"])] if version else []
    if not plan:
        raise ValueError("Для этого выпуска и фильтра нет файлов.")
    total = sum(a["size"] for a in plan)
    done = downloaded = existing = 0
    if progress:
        progress(done, total)
    for asset in plan:
        def report(current, _total):
            progress(done + current, total)

        try:
            _, fresh = store(asset, directory, progress=report if progress else None)
        except (OSError, ValueError) as exc:
            raise ValueError(f"В наборе сохранено {downloaded + existing} из {len(plan)} файлов. {asset['name']}. {exc}") from exc
        downloaded += int(fresh)
        existing += int(not fresh)
        done += asset["size"]
        if progress:
            progress(done, total)
    return dict(downloaded=downloaded, existing=existing)


def legacy_record(path):
    asset = identify_name(path.name)
    if not asset or not path.is_file() or path.is_symlink() or not 0 < path.stat().st_size <= MAX_SIZE:
        return None
    data = path.read_bytes()
    build = struct.unpack_from("<I", data, 8)[0] if asset["format"] != ".uf2" and len(data) >= 12 else 0
    asset.update(build=build, tag=f"r{build}" if build else "Ранее скачано", prerelease=False, size=len(data))
    validate_firmware(asset, data)
    return dict(make_record(asset, data), path=str(path.resolve()))


def records(directory):
    root = Path(directory).expanduser().resolve()
    if not root.is_dir():
        return
    for folder in (root / "library").glob("release-*"):
        if not folder.is_dir() or folder.is_symlink():
            continue
        for path in folder.glob("*.json"):
            try:
                record = read_record(path)
                if record:
                    yield record
            except (OSError, ValueError):
                continue
    for folder in root.glob("release-*"):
        if not folder.is_dir() or folder.is_symlink():
            continue
        for path in folder.iterdir():
            try:
                record = legacy_record(path)
                if record:
                    yield record
            except (OSError, ValueError):
                continue


def local_issue(device, asset):
    issue = update_issue(device, asset)
    if issue:
        return issue
    if asset["format"] == ".mcfd" and (asset["base"] != device["build"] or
            asset.get("base_hash") != device["base_hash"] or asset.get("base_size") != device["running_size"]):
        return "Дельта рассчитана на другую исходную прошивку"
    if asset["format"] == ".mcfw" and asset.get("image_size", asset["size"]) > device["capacity"]:
        return "Образ больше слота обновления"
    return ""


def file_label(asset):
    version = f"r{asset['build']}" if asset["build"] else asset["tag"]
    if asset["format"] == ".mcfd":
        kind = f"Дельта .mcfd · r{asset['base']} → {version}"
    elif asset["format"] == ".mcfw":
        kind = f"Полная .mcfw · {version}"
    else:
        kind = f"UF2 · {version}"
    recommendation = "Рекомендуем · " if asset["recommended"] else ""
    return f"{recommendation}{kind} · {asset['model']} · {asset['role_title']}"


def saved(context, directory=DEFAULT_DIR):
    formats = context.get("formats", FORMATS)
    device = context.get("device")
    if device and (device["board"] not in BOARDS or device["role"] not in ROLES):
        device = None
    board = device["board"] if device else context.get("board", 0)
    all_files, selected = set(), {}
    for asset in records(directory):
        key = asset["name"], asset["digest"], asset["build"]
        all_files.add(key)
        if (asset["format"] not in formats or (board and asset["board"] != board) or
                (device and asset["format"] != ".uf2" and asset["role"] != device["role"])):
            continue
        if key in selected:
            continue
        issue = local_issue(device, asset) if device and asset["format"] != ".uf2" else ""
        usb_install = (context.get("usb") and asset["format"] == ".mcfw" and asset["board"] in (1, 2, 3, 6, 8))
        selected[key] = dict(asset, model=BOARD_LABELS[asset["board"]], role_title=ROLE_LABELS[asset["role"]],
                             issue=issue, enabled=not issue or bool(usb_install), recommended=False)
    files = sorted(selected.values(), key=lambda a: (a["issue"] != "", a["prerelease"], -a["build"],
                                                     a["format"] != ".mcfd", a["model"], a["role"]))
    candidate = next((a for a in files if not a["issue"] and a["format"] != ".uf2"), None)
    if device and candidate:
        candidate["recommended"] = True
    for asset in files:
        asset["label"] = file_label(asset)
    return dict(files=files, total=len(all_files), device=device["summary"] if device else BOARD_LABELS.get(board, ""))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--directory", type=Path, required=True)
    commands = parser.add_subparsers(dest="command", required=True)
    commands.add_parser("index")
    listing = commands.add_parser("list")
    listing.add_argument("context")
    batch = commands.add_parser("sync")
    batch.add_argument("tag")
    batch.add_argument("board", type=int)
    batch.add_argument("role", type=int)
    check = commands.add_parser("check")
    check.add_argument("path")
    check.add_argument("digest")
    args = parser.parse_args()
    try:
        if args.command == "list":
            result = saved(json.loads(base64.b64decode(args.context, validate=True)), args.directory)
        elif args.command == "index":
            result = index(args.directory)
        elif args.command == "sync":
            result = sync(args.tag, args.board, args.role, args.directory, progress=cli_progress())
        else:
            result = dict(path=str(verify(args.path, args.digest, args.directory)))
    except (KeyError, OSError, ValueError) as exc:
        sys.exit(str(exc))
    print(json.dumps(result, ensure_ascii=True))


if __name__ == "__main__":
    main()
