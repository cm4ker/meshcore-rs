"""Русский интерфейс прошивальщика для macOS и Linux."""
import asyncio
import contextlib
import io
import multiprocessing
import pathlib
import queue
import re
import sys
import threading
import tkinter as tk
from argparse import Namespace
from tkinter import filedialog, messagebox, ttk
from tkinter.scrolledtext import ScrolledText

from bleak import BleakScanner
from serial.tools import list_ports
import firmware_catalog
import nrf_flash
import ota_app
import usb_flash


USB = "USB"
BLE = "Обновление по Bluetooth"
TCP = "Wi-Fi / TCP"
DFU = "Загрузчик nRF52 (UF2)"
BOARD_NAMES = {"Heltec T096": "heltec-t096", "Heltec T114": "heltec-t114",
               "Pro Micro (FakeTec)": "promicro", "LilyGO T-Echo": "t-echo"}
ROLE_NAMES = {"Компаньон": "companion", "Репитер": "repeater"}
HELP = {
    USB: "Подключите плату кабелем с передачей данных. Выберите порт и нажмите «Проверить устройство».\n\n"
         "Для обновления выберите .mcfw, .mcfd или .bin для вашей платы. Кнопка «Из GitHub» подберёт подходящий выпуск.\n\n"
         "Для .uf2 переведите плату в режим USB-загрузчика и через «Диск UF2…» выберите корень её диска. "
         "Выберите файл и нажмите «Прошить UF2 по USB».\n\n"
         "На поддерживаемых ESP можно прошить .mcfw или .bin через загрузчик. Действие «Новая нода со сбросом…» "
         "также удаляет ключ, настройки, контакты и каналы.",
    BLE: "Включите Bluetooth и откройте на плате окно подключения. Для репитера MeshCore-RS используйте команду "
         "fw ble. Нажмите «Найти устройства» и выберите плату. Подтвердите сопряжение в системе.\n\n"
         "Нажмите «Проверить устройство». Выберите файл для своей модели и роли или нажмите «Из GitHub». "
         "Файл .bin подходит, когда плата принимает полный неподписанный образ. Для .uf2 выберите «Загрузчик nRF52 (UF2)».",
    TCP: "Подключите компьютер и плату к одной сети. Введите адрес платы с портом, например 192.168.1.50:5000.\n\n"
         "Нажмите «Проверить устройство». Выберите подписанный .mcfw или .mcfd для своей платы либо воспользуйтесь каталогом GitHub.",
    DFU: "Откройте BLE OTA на плате и нажмите «Найти устройства». Выберите загрузчик и проверьте модель. "
         "Если она не распознана, укажите её вручную.\n\n"
         "Выберите файл .uf2 для этой платы или укажите роль и нажмите «Из GitHub». Сверьте модель и имя файла перед запуском. "
         "«Сохранить DFU ZIP» упаковывает выбранный UF2 в архив для отдельного DFU-прошивальщика.",
}


def file_formats(transport, disk=False):
    if transport == DFU or disk:
        return (".uf2",)
    if transport == TCP:
        return (".mcfw", ".mcfd")
    formats = (".mcfw", ".mcfd", ".bin")
    return formats + (".uf2",) if transport == USB else formats


class LogStream(io.TextIOBase):
    def __init__(self, events):
        self.events = events

    def write(self, text):
        if text:
            self.events.put(("log", text))
        return len(text)


class Flasher:
    def __init__(self, root):
        self.root = root
        root.title("ota-app · MeshCore")
        self.events = queue.Queue()
        self.busy = False
        self.labels = {}
        self.ble = {}
        self.dfu = {}
        self.disk = None
        self.checked = None
        self.pending_catalog = None
        self.journal_open = True

        panel = ttk.Frame(root, padding=14)
        self.panel = panel
        panel.pack(fill="both", expand=True)
        panel.columnconfigure(2, weight=1)
        panel.rowconfigure(8, weight=1)
        ttk.Label(panel, text="Подключение").grid(row=0, column=0, sticky="w")
        self.method = ttk.Combobox(panel, values=(USB, BLE, TCP, DFU), width=25, state="readonly")
        self.method.grid(row=0, column=1, padx=6)
        self.method.current(0)
        self.address_text = tk.StringVar()
        self.address = ttk.Combobox(panel, textvariable=self.address_text, width=32)
        self.address.grid(row=0, column=2, sticky="ew")
        self.scan = ttk.Button(panel, text="Найти устройства", command=lambda: self.start("scan"))
        self.scan.grid(row=0, column=3, padx=(6, 0))

        connection = ttk.Frame(panel)
        connection.grid(row=1, column=0, columnspan=4, sticky="w", pady=(6, 2))
        self.info = ttk.Button(connection, text="Проверить устройство", command=lambda: self.start("info"))
        self.info.pack(side="left", padx=(0, 6))
        ttk.Button(connection, text="ИНСТРУКЦИЯ", command=self.help).pack(side="left")
        self.disk_button = ttk.Button(connection, text="Диск UF2…", command=self.pick_disk)
        self.disk_button.pack(side="left", padx=6)
        self.details = tk.StringVar()
        ttk.Label(panel, textvariable=self.details, wraplength=700).grid(row=2, column=0, columnspan=4, sticky="w")

        self.nrf = ttk.Frame(panel)
        self.nrf.grid(row=3, column=0, columnspan=4, sticky="w", pady=4)
        ttk.Label(self.nrf, text="Модель платы").pack(side="left")
        self.board = ttk.Combobox(self.nrf, values=tuple(BOARD_NAMES), state="readonly", width=23)
        self.board.pack(side="left", padx=6)
        self.board.bind("<<ComboboxSelected>>", lambda _: self.controls())
        ttk.Label(self.nrf, text="Роль для GitHub").pack(side="left", padx=(8, 4))
        self.role = ttk.Combobox(self.nrf, values=tuple(ROLE_NAMES), state="readonly", width=13)
        self.role.pack(side="left")

        ttk.Label(panel, text="Файл прошивки").grid(row=4, column=0, sticky="w", pady=10)
        self.file = tk.StringVar()
        ttk.Entry(panel, textvariable=self.file, state="readonly").grid(row=4, column=1, columnspan=2, sticky="ew", padx=6)
        files = ttk.Frame(panel)
        files.grid(row=4, column=3, sticky="e")
        self.browse = ttk.Button(files, text="Выбрать…", command=self.choose)
        self.browse.pack(side="left")
        self.github = ttk.Button(files, text="Из GitHub", command=lambda: self.start("catalog"))
        self.github.pack(side="left", padx=3)
        self.clear = ttk.Button(files, text="×", width=2, command=lambda: self.file.set(""))
        self.clear.pack(side="left")

        bar = ttk.Frame(panel)
        bar.grid(row=5, column=0, columnspan=4, sticky="w", pady=6)
        self.buttons = {}
        for name, label in (("update", "Обновить прошивку"), ("usb", "Прошить через USB"),
                            ("new", "Новая нода со сбросом…"), ("export", "Сохранить DFU ZIP")):
            button = ttk.Button(bar, text=label, command=lambda op=name: self.start(op))
            button.pack(side="left", padx=2)
            self.buttons[name] = button

        self.status = tk.StringVar(value="Готово к работе")
        ttk.Label(panel, textvariable=self.status, wraplength=700).grid(row=6, column=0, columnspan=4, sticky="w", pady=4)
        self.progress = ttk.Progressbar(panel, mode="indeterminate")
        self.progress.grid(row=7, column=0, columnspan=3, sticky="ew", padx=(0, 6))
        self.progress.grid_remove()
        self.toggle = ttk.Button(panel, text="Свернуть журнал", command=self.toggle_journal)
        self.toggle.grid(row=7, column=3, sticky="e", pady=4)
        self.log = ScrolledText(panel, width=86, height=12, state="disabled")
        self.log.grid(row=8, column=0, columnspan=4, sticky="nsew")

        self.method.bind("<<ComboboxSelected>>", self.changed)
        self.address.bind("<<ComboboxSelected>>", self.target_changed)
        self.address_text.trace_add("write", self.update_summary)
        self.file.trace_add("write", lambda *_: self.controls())
        self.changed()
        root.protocol("WM_DELETE_WINDOW", self.close)
        root.after(100, self.poll)
        root.after(1500, self.refresh)

    def help(self):
        messagebox.showinfo(self.method.get(), HELP[self.method.get()] + "\n\n"
                            "Ход работы виден в журнале. Дельта .mcfd подходит только к указанной в её имени "
                            "сборке. Сверьте файл и устройство перед запуском.", parent=self.root)

    def choose(self):
        formats = file_formats(self.method.get(), self.disk is not None)
        patterns = tuple(pattern for ext in formats for pattern in ("*" + ext, "*" + ext.upper()))
        path = filedialog.askopenfilename(parent=self.root, filetypes=[("Прошивка", patterns)])
        if path and pathlib.Path(path).suffix.lower() in formats:
            self.file.set(path)
            return path
        if path:
            self.notice("Выберите прошивку с расширением " + ", ".join(formats) + ".")
        return ""

    def notice(self, message):
        messagebox.showinfo("ota-app", message, parent=self.root)

    def toggle_journal(self):
        self.journal_open = not self.journal_open
        if self.journal_open:
            self.panel.rowconfigure(8, weight=1)
            self.log.grid()
        else:
            self.panel.rowconfigure(8, weight=0)
            self.log.grid_remove()
        self.toggle.configure(text="Свернуть журнал" if self.journal_open else "Развернуть журнал")

    def ports(self):
        if self.disk:
            return
        current = self.labels.get(self.address.get(), self.address.get())
        self.labels = {f"{port.description} ({port.device})": port.device for port in sorted(list_ports.comports())}
        self.address.configure(values=tuple(self.labels))
        selected = next((label for label, device in self.labels.items() if device == current), "")
        if not selected and len(self.labels) == 1:
            selected = next(iter(self.labels))
        if selected != self.address.get():
            self.address.set(selected)

    def target(self):
        transport, value = self.method.get(), self.address.get().strip()
        if transport == USB:
            return self.disk[0] if self.disk else self.labels.get(value, value)
        if transport == BLE:
            return self.ble.get(value, value)
        if transport == DFU:
            return self.dfu.get(value, {}).get("address", "")
        if re.fullmatch(r"[^:\s]+", value):
            return value + ":5000"
        return value

    def changed(self, event=None):
        self.disk = None
        self.checked = None
        self.address.set("")
        self.address.configure(values=())
        transport = self.method.get()
        if transport == USB:
            self.ports()
        elif transport == BLE:
            self.address.configure(values=tuple(self.ble))
        elif transport == DFU:
            self.address.configure(values=tuple(self.dfu))
        self.target_changed()
        if pathlib.Path(self.file.get()).suffix.lower() not in file_formats(transport):
            self.file.set("")
        self.controls()

    def target_changed(self, event=None):
        self.board.set("")
        detected = self.nrf_detected_board()
        if detected:
            self.board.set(next((title for title, slug in BOARD_NAMES.items() if slug == detected), ""))
        self.update_summary()
        self.controls()

    def nrf_detected_board(self):
        if self.disk:
            return self.disk[1].get("board")
        if self.method.get() == DFU:
            return self.dfu.get(self.address.get(), {}).get("board")
        return None

    def update_summary(self, *_):
        if self.disk:
            info = self.disk[1]
            self.details.set(" · ".join(info[k] for k in ("model", "version", "softdevice") if info.get(k)))
        elif self.checked and self.checked[:2] == (self.method.get(), self.target()):
            self.details.set(self.checked[2]["summary"])
        else:
            self.details.set("")

    def controls(self):
        transport = self.method.get()
        is_uf2 = pathlib.Path(self.file.get()).suffix.lower() == ".uf2"
        nrf = transport == DFU or self.disk is not None
        self.method.configure(state="disabled" if self.busy else "readonly")
        self.address.configure(state="disabled" if self.busy or self.disk else "readonly" if transport in (USB, DFU) else "normal")
        self.scan.configure(state="disabled" if self.busy or transport == TCP else "normal")
        self.scan.configure(text="Найти загрузчик" if transport == DFU else "Найти устройства")
        self.info.configure(state="disabled" if self.busy or transport == DFU else "normal")
        self.disk_button.configure(state="normal" if transport == USB and not self.busy else "disabled")
        self.disk_button.configure(text="Закрыть диск UF2" if self.disk else "Диск UF2…")
        self.browse.configure(state="disabled" if self.busy else "normal")
        self.github.configure(state="disabled" if self.busy else "normal")
        self.clear.configure(state="disabled" if self.busy or not self.file.get() else "normal")
        if nrf:
            self.nrf.grid()
        else:
            self.nrf.grid_remove()
        self.board.configure(state="disabled" if self.busy or self.nrf_detected_board() else "readonly")
        self.role.configure(state="disabled" if self.busy else "readonly")
        caption = "Начать" if transport == DFU else "Прошить UF2 по USB" if self.disk or is_uf2 else "Обновить прошивку"
        self.buttons["update"].configure(text=caption, state="disabled" if self.busy else "normal")
        for name in ("usb", "new"):
            enabled = not self.busy and transport == USB and not (self.disk or is_uf2)
            self.buttons[name].configure(state="normal" if enabled else "disabled")
        if transport == DFU:
            self.buttons["export"].pack(side="left", padx=2)
            self.buttons["export"].configure(state="normal" if not self.busy and is_uf2 else "disabled")
        else:
            self.buttons["export"].pack_forget()

    def refresh(self):
        if not self.busy and self.method.get() == USB and not self.disk:
            self.ports()
        self.root.after(1500, self.refresh)

    def close(self):
        if self.busy:
            self.notice("Дождитесь завершения операции.")
        else:
            self.root.destroy()

    def pick_disk(self, select_only=False):
        if self.disk and not select_only:
            self.disk = None
            self.address.set("")
            self.ports()
            self.target_changed()
            return True
        path = filedialog.askdirectory(parent=self.root, mustexist=True, title="Выберите корень диска UF2")
        if not path:
            return False
        try:
            info = nrf_flash.inspect_uf2_drive(path)
        except (OSError, ValueError) as exc:
            self.notice(str(exc))
            return False
        self.disk = (path, info)
        self.address.set(info.get("model") or path)
        self.target_changed()
        if pathlib.Path(self.file.get()).suffix.lower() != ".uf2":
            self.file.set("")
        return True

    def start(self, op):
        if self.busy:
            return
        transport = self.method.get()
        path = self.file.get()
        if op == "scan" and transport == USB:
            if self.disk:
                self.update_summary()
            else:
                self.ports()
                self.status.set(f"Портов найдено: {len(self.labels)}")
            return
        if op in ("update", "usb", "new", "export"):
            path = path or self.choose()
            if not path:
                return
            extension = pathlib.Path(path).suffix.lower()
            if extension not in file_formats(transport, self.disk is not None):
                self.notice("Формат файла не подходит для этого подключения.")
                return
            if op in ("usb", "new") and extension not in (".mcfw", ".bin"):
                self.notice("Для дельты .mcfd нажмите «Обновить прошивку».")
                return
            if op == "update" and transport == USB and extension == ".uf2" and not self.disk:
                if not self.pick_disk(select_only=True):
                    return
        target = self.target()
        nrf = transport == DFU or self.disk is not None
        board = BOARD_NAMES.get(self.board.get())
        if op == "info" and self.disk:
            try:
                self.disk = (target, nrf_flash.inspect_uf2_drive(target))
                self.target_changed()
            except (OSError, ValueError) as exc:
                self.notice(str(exc))
            return
        if op == "export":
            output = filedialog.asksaveasfilename(parent=self.root, defaultextension=".zip",
                                                  initialfile=pathlib.Path(path).stem + "-dfu.zip",
                                                  filetypes=[("Архив DFU", "*.zip")])
            if output:
                self.launch(op, transport, output, path, board)
            return
        if op == "catalog" and nrf:
            role = ROLE_NAMES.get(self.role.get())
            if not board or not role:
                self.notice("Для поиска UF2 выберите модель платы и роль прошивки.")
                return
            self.launch("catalog-uf2", transport, role, "", board)
            return
        if op != "scan" and not target:
            self.notice("Выберите порт или устройство либо укажите адрес платы.")
            return
        if transport == TCP and not re.fullmatch(r"(?:\[[^]]+\]|[^:\s]+):[0-9]{1,5}", target):
            self.notice("Укажите адрес и порт, например 192.168.1.50:5000.")
            return
        if op in ("update", "usb", "new"):
            if nrf and not board:
                self.notice("Укажите точную модель платы перед прошивкой UF2.")
                return
            label = self.board.get() if nrf else self.address.get()
            message = f"Установить {pathlib.Path(path).name} на {label}?"
            if op == "new":
                message += "\n\nПрежний ключ, контакты, каналы, настройки и сопряжения обеих ролей будут удалены."
            if not messagebox.askyesno("Подтвердите прошивку", message, default="no", parent=self.root):
                return
        self.launch(op, transport, target, path, board)

    def launch(self, op, transport, target, path, board=None):
        self.busy = True
        self.controls()
        self.status.set("Выполняется операция…")
        self.progress.grid()
        self.progress.start(16)
        self.log.configure(state="normal")
        self.log.delete("1.0", "end")
        self.log.configure(state="disabled")
        disk = self.disk[0] if self.disk else None
        threading.Thread(target=self.worker, args=(op, transport, target, path, board, disk), daemon=True).start()

    def worker(self, op, transport, target, path, board, disk):
        stream = LogStream(self.events)
        try:
            with contextlib.redirect_stdout(stream), contextlib.redirect_stderr(stream):
                if op == "scan" and transport == DFU:
                    devices = asyncio.run(nrf_flash.scan_dfu())
                    self.events.put(("dfu", {f"{device['name']} ({device['address']})": device for device in devices}))
                elif op == "scan":
                    found = asyncio.run(BleakScanner.discover(timeout=10, return_adv=True))
                    devices = {}
                    for device, advertisement in found.values():
                        name = advertisement.local_name or device.name or ""
                        if name.startswith("MeshCore"):
                            devices[f"{name} ({device.address})"] = device.address
                    self.events.put(("ble", devices))
                elif op == "catalog-uf2":
                    self.events.put(("catalog", firmware_catalog.catalog(firmware_catalog.uf2_target(board, target))))
                elif op == "download":
                    self.events.put(("file", str(firmware_catalog.download(target, pathlib.Path.home() / ".ota-app" / "firmware"))))
                elif op == "export":
                    nrf_flash.export_dfu_zip(path, target)
                elif op == "update" and disk:
                    nrf_flash.write_uf2(path, disk, board)
                elif op == "update" and transport == DFU:
                    asyncio.run(nrf_flash.install_dfu(target, path, board))
                elif op in ("usb", "new"):
                    usb_flash.install(Namespace(port=target, image=pathlib.Path(path), check=False, new_node=op == "new"))
                else:
                    flag = {USB: "port", BLE: "ble", TCP: "tcp"}[transport]
                    if op in ("info", "catalog"):
                        args = dict(port=None, ble=None, tcp=None, pin=None)
                        args[flag] = target
                        device = asyncio.run(ota_app.probe(Namespace(**args)))
                        self.events.put(("device", (transport, target, device)))
                        print(device["summary"])
                        if op == "catalog":
                            self.events.put(("catalog", firmware_catalog.catalog(device)))
                    else:
                        asyncio.run(ota_app.main(["--" + flag, target, path]))
            self.events.put(("done", (True, op)))
        except (Exception, SystemExit) as exc:
            self.events.put(("log", f"\n{exc or type(exc).__name__}\n"))
            self.events.put(("done", (False, op)))

    def poll(self):
        while not self.events.empty():
            kind, value = self.events.get_nowait()
            if kind == "log":
                self.log.configure(state="normal")
                self.log.insert("end", value)
                self.log.see("end")
                self.log.configure(state="disabled")
            elif kind in ("ble", "dfu"):
                setattr(self, kind, value)
                self.address.configure(values=tuple(value))
                self.address.set(next(iter(value)) if len(value) == 1 else "")
                self.target_changed()
                self.events.put(("log", f"Найдено устройств: {len(value)}\n"))
            elif kind == "device":
                self.checked = value
                self.update_summary()
            elif kind == "catalog":
                self.pending_catalog = value
            elif kind == "file":
                self.file.set(value)
            elif kind == "done":
                success, operation = value
                self.busy = False
                self.progress.stop()
                self.progress.grid_remove()
                if success and operation in ("update", "usb", "new"):
                    self.checked = None
                    if self.disk:
                        self.disk = None
                        self.address.set("")
                        self.ports()
                    self.file.set("")
                    self.update_summary()
                self.status.set("Операция завершена." if success else "Ошибка. Подробности в журнале.")
                self.controls()
                if self.pending_catalog is not None:
                    catalog, self.pending_catalog = self.pending_catalog, None
                    self.show_catalog(catalog)
        self.root.after(100, self.poll)

    def show_catalog(self, catalog):
        assets = catalog["assets"]
        if not assets:
            self.notice("В выпусках GitHub нет файлов для этой платы, роли и сборки.")
            return
        dialog = tk.Toplevel(self.root)
        dialog.title("Прошивки из GitHub")
        dialog.transient(self.root)
        frame = ttk.Frame(dialog, padding=14)
        frame.pack(fill="both", expand=True)
        ttk.Label(frame, text=catalog["device"]["summary"], wraplength=720).pack(anchor="w", pady=(0, 9))
        recommended = catalog["recommended"]
        if recommended is None:
            ttk.Label(frame, text="Подходящих обновлений для этой сборки нет.").pack(anchor="w")
        else:
            choice = assets[recommended]
            details = f"Рекомендуем  ·  {choice['tag']}  ·  {choice['format']}"
            if choice["base"]:
                details += f" с №{choice['base']}"
            if choice["prerelease"]:
                details += "  ·  предварительный выпуск"
            ttk.Label(frame, text=details).pack(anchor="w", pady=(0, 5))

        columns = ("release", "format", "size", "status")
        rest = ttk.Frame(frame)
        tree = ttk.Treeview(rest, columns=columns, show="headings", height=8, selectmode="browse")
        scrollbar = ttk.Scrollbar(rest, orient="vertical", command=tree.yview)
        scrollbar.pack(side="right", fill="y")
        tree.configure(yscrollcommand=scrollbar.set)
        tree.pack(fill="both", expand=True)
        for name, text, width in zip(columns, ("Выпуск", "Файл", "Размер", "Состояние"), (164, 120, 80, 305)):
            tree.heading(name, text=text)
            tree.column(name, width=width)
        tree.tag_configure("recommended", background="#d1ede5", foreground="#173e35")
        for index, asset in enumerate(assets):
            kind = asset["format"] + (f" с №{asset['base']}" if asset["base"] else "")
            state = asset["issue"] or ("Рекомендуем" if index == recommended else "Подходит")
            if asset["prerelease"] and not asset["issue"]:
                state += " · предварительный"
            size = f"{asset['size'] / 1024:.1f} КиБ" if asset["size"] >= 1024 else f"{asset['size']} Б"
            tree.insert("", "end", iid=str(index), values=(asset["tag"], kind, size, state),
                        tags=("recommended",) if index == recommended else ())

        bar = ttk.Frame(frame)
        bar.pack(fill="x", pady=(8, 0))
        show_all = tk.BooleanVar(value=recommended is None)

        def toggle():
            if show_all.get():
                rest.pack(fill="both", expand=True, before=bar, pady=(8, 0))
            else:
                rest.pack_forget()

        ttk.Checkbutton(bar, text="Другие файлы и архив", variable=show_all, command=toggle).pack(side="left")

        def download():
            selection = tree.selection() if show_all.get() else ()
            index = int(selection[0]) if selection else recommended
            if index is None:
                return
            asset = assets[index]
            if asset["issue"] and not messagebox.askyesno("Скачать из архива", asset["issue"] + ". Скачать файл?", parent=dialog):
                return
            dialog.destroy()
            self.launch("download", self.method.get(), asset, "")

        ttk.Button(bar, text="Закрыть", command=dialog.destroy).pack(side="right")
        ttk.Button(bar, text="Скачать", command=download).pack(side="right", padx=6)
        if recommended is not None:
            tree.selection_set(str(recommended))
        tree.bind("<Double-1>", lambda _: download())
        toggle()
        dialog.grab_set()


if __name__ == "__main__":
    multiprocessing.freeze_support()
    root = tk.Tk()
    ui = Flasher(root)
    if "--smoke-test" in sys.argv:
        assert ui.method.get() == USB and set(ui.buttons) == {"update", "usb", "new", "export"}
        assert (usb_flash.ROOT / "t3s3.csv").is_file()
        ui.method.current(2)
        ui.changed()
        assert ".bin" not in file_formats(ui.method.get())
        ui.toggle_journal()
        ui.toggle_journal()
        ui.method.current(3)
        ui.changed()
        assert file_formats(ui.method.get()) == (".uf2",)
        root.after(100, root.destroy)
    root.mainloop()
