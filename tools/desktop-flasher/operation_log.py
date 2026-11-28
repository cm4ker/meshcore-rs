"""Save an optional UTF-8 diagnostic log to the user's Downloads folder."""

from datetime import datetime
import os
from pathlib import Path
import shlex
import sys


OPERATIONS = {
    "scan": "Поиск устройств", "info": "Проверка устройства", "update": "Обновление прошивки",
    "usb": "Прошивка через USB", "new": "Новая нода со сбросом", "export": "Сохранение DFU-пакета",
    "download": "Загрузка прошивки", "catalog": "Каталог выпусков", "catalog-uf2": "Каталог UF2",
    "library": "Офлайн-библиотека", "library-index": "Список доступных наборов",
    "library-sync": "Загрузка набора", "library-check": "Проверка сохранённой прошивки",
}


def downloads_directory():
    home = Path.home()
    if sys.platform.startswith("linux"):
        config = Path(os.environ.get("XDG_CONFIG_HOME") or home / ".config") / "user-dirs.dirs"
        try:
            for line in config.read_text(encoding="utf-8").splitlines():
                key, _, value = line.strip().partition("=")
                if key == "XDG_DOWNLOAD_DIR":
                    parts = shlex.split(value, comments=True)
                    if len(parts) == 1:
                        path = Path(parts[0].replace("${HOME}", str(home)).replace("$HOME", str(home)))
                        if path.is_absolute():
                            return path
        except (OSError, ValueError):
            pass
    return home / "Downloads"


def save_log(operation, transport, result, transcript):
    directory = downloads_directory()
    directory.mkdir(parents=True, exist_ok=True)
    now = datetime.now()
    path = directory / (now.strftime("ota-app-%Y%m%d-%H%M%S-%f") + ".txt")
    title = OPERATIONS.get(operation, "Операция с прошивкой")
    text = f"ota-app 1.3.2\n{now:%Y-%m-%d %H:%M:%S}\n{title} · {transport}\n{result}\n\n{transcript.rstrip()}\n"
    with path.open("x", encoding="utf-8") as output:
        output.write(text)
    return path
