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
from tkinter import filedialog, font, messagebox, ttk

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
HINTS = {
    USB: "Подключите плату кабелем с передачей данных.",
    BLE: "Откройте окно подключения на плате и запустите поиск.",
    TCP: "Введите адрес платы в вашей Wi-Fi-сети. Порт по умолчанию 5000.",
    DFU: "Откройте BLE OTA на плате. Если модель не определилась, выберите её вручную.",
}
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

SCREEN = "#edf4f1"
PANEL = "#ffffff"
BRAND = "#103c34"
SUBTITLE = "#b8d7cf"
INK = "#192f29"
MUTED = "#4f645e"
BORDER = "#d6e3de"
FIELD = "#f6f9f7"
ACCENT = "#006853"
ACCENT_LIGHT = "#dfefe9"
DISABLED = "#e8eeeb"
DISABLED_INK = "#65756f"


class Appearance:
    def __init__(self, root):
        self.root = root
        self.scale = max(1.0, root.winfo_fpixels("1i") / 96)
        self.style = ttk.Style(root)
        self.style.theme_use("clam")
        base = font.nametofont("TkDefaultFont", root=root)
        self.strong = base.copy()
        self.strong.configure(weight="bold")
        self.section = self.strong.copy()
        self.section.configure(size=base.cget("size") + 5)
        self.brand_font = self.strong.copy()
        self.brand_font.configure(size=base.cget("size") + 11)

        for name, background in (("Screen", SCREEN), ("Surface", PANEL), ("Field", FIELD),
                                 ("Brand", BRAND), ("Recommend", ACCENT_LIGHT)):
            self.style.configure(f"{name}.TFrame", background=background)
            self.style.configure(f"{name}.TLabel", background=background, foreground=MUTED)
        self.style.configure("BrandTitle.TLabel", background=BRAND, foreground=PANEL, font=self.brand_font)
        self.style.configure("BrandSubtitle.TLabel", background=BRAND, foreground=SUBTITLE)
        self.style.configure("Section.TLabel", background=PANEL, foreground=INK, font=self.section)
        self.style.configure("FieldTitle.TLabel", background=PANEL, foreground=INK, font=self.strong)
        self.style.configure("Summary.TLabel", background=FIELD, foreground=INK, padding=self.px(10))
        self.style.configure("RecommendTitle.TLabel", background=ACCENT_LIGHT, foreground=ACCENT, font=self.strong)
        self.style.configure("RecommendBuild.TLabel", background=ACCENT_LIGHT, foreground=INK, font=self.section)
        self.style.configure("ProgressPercent.TLabel", background=PANEL, foreground=ACCENT, font=self.strong)

        self.style.configure("Card.TFrame", background=PANEL, bordercolor=BORDER, borderwidth=1, relief="solid")
        self.style.configure("Recommendation.TFrame", background=ACCENT_LIGHT, bordercolor=ACCENT,
                             borderwidth=1, relief="solid")
        self.button("Primary", ACCENT, BRAND, PANEL, disabled_fill=DISABLED)
        self.button("Secondary", ACCENT_LIGHT, "#cfe6dd", ACCENT, disabled_fill=DISABLED)
        self.button("Link", PANEL, FIELD, ACCENT, disabled_fill=PANEL)
        self.button("Page", SCREEN, ACCENT_LIGHT, ACCENT, disabled_fill=SCREEN)
        self.button("RecommendAction", ACCENT, BRAND, PANEL, disabled_fill=DISABLED)

        for widget in ("TEntry", "TCombobox"):
            name = f"Mesh.{widget}"
            self.style.configure(name, foreground=INK, fieldbackground=FIELD, background=FIELD,
                                 bordercolor=BORDER, lightcolor=BORDER, darkcolor=BORDER,
                                 arrowcolor=ACCENT, insertcolor=INK, selectbackground=ACCENT,
                                 selectforeground=PANEL, padding=self.px(7))
            self.style.map(name, foreground=[("disabled", DISABLED_INK)],
                           fieldbackground=[("disabled", DISABLED), ("readonly", FIELD)],
                           bordercolor=[("focus", ACCENT), ("disabled", BORDER)])
        self.style.configure("Mesh.Horizontal.TProgressbar", background=ACCENT, troughcolor=ACCENT_LIGHT,
                             bordercolor=ACCENT_LIGHT, lightcolor=ACCENT, darkcolor=ACCENT,
                             thickness=self.px(9), borderwidth=0)
        self.style.configure("Mesh.Treeview", background=PANEL, fieldbackground=PANEL, foreground=INK,
                             bordercolor=BORDER, lightcolor=BORDER, darkcolor=BORDER,
                             rowheight=max(self.px(30), base.metrics("linespace") + self.px(10)))
        self.style.configure("Mesh.Treeview.Heading", background=ACCENT_LIGHT, foreground=INK,
                             bordercolor=BORDER, relief="flat", padding=self.px(6), font=self.strong)
        self.style.map("Mesh.Treeview", background=[("selected", ACCENT)], foreground=[("selected", PANEL)])
        self.style.configure("Mesh.Vertical.TScrollbar", background=ACCENT_LIGHT, troughcolor=SCREEN,
                             bordercolor=BORDER, lightcolor=ACCENT_LIGHT, darkcolor=ACCENT_LIGHT,
                             arrowcolor=MUTED)
        for option, value in (("background", PANEL), ("foreground", INK),
                              ("selectBackground", ACCENT), ("selectForeground", PANEL)):
            root.option_add(f"*TCombobox*Listbox.{option}", value)

        self.icon = None
        icon_path = pathlib.Path(getattr(sys, "_MEIPASS", pathlib.Path(__file__).resolve().parent)) / "ommesh.png"
        try:
            self.icon = tk.PhotoImage(master=root, file=icon_path)
            root.iconphoto(True, self.icon)
        except (OSError, tk.TclError):
            pass

    def px(self, value):
        return round(value * self.scale)

    def button(self, name, fill, hover, foreground, disabled_fill):
        style = f"{name}.TButton"
        self.style.configure(style, background=fill, foreground=foreground, font=self.strong,
                             borderwidth=0, relief="flat", focuscolor=foreground,
                             padding=(self.px(10), self.px(8)))
        self.style.map(style, background=[("disabled", disabled_fill), ("pressed", hover), ("active", hover)],
                       foreground=[("disabled", DISABLED_INK)], relief=[("pressed", "flat")])

    def header(self, parent, title, subtitle, logo=False):
        header = ttk.Frame(parent, style="Brand.TFrame", padding=(self.px(20), self.px(10)))
        if logo and self.icon:
            ttk.Label(header, image=self.icon, style="BrandSubtitle.TLabel").grid(
                row=0, column=0, rowspan=2, sticky="w", padx=(0, self.px(14)), pady=self.px(2))
        ttk.Label(header, text=title, style="BrandTitle.TLabel").grid(row=0, column=1, sticky="sw", pady=(self.px(4), 0))
        ttk.Label(header, text=subtitle, style="BrandSubtitle.TLabel").grid(row=1, column=1, sticky="nw", pady=(0, self.px(4)))
        header.rowconfigure((0, 1), weight=1)
        return header


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
        root.title("ota-app 1.3.1 · MeshCore")
        self.events = queue.Queue()
        self.busy = False
        self.labels = {}
        self.ble = {}
        self.dfu = {}
        self.disk = None
        self.checked = None
        self.pending_catalog = None
        self.journal_open = False
        self.appearance = Appearance(root)
        px = self.appearance.px
        root.configure(background=SCREEN)
        available_width = max(320, root.winfo_screenwidth() - px(64))
        available_height = max(260, root.winfo_screenheight() - px(92))
        root.geometry(f"{min(px(1016), available_width)}x{min(px(724), available_height)}")
        root.minsize(min(px(440), available_width), min(px(340), available_height))
        root.columnconfigure(0, weight=1)
        root.rowconfigure(1, weight=1)
        self.appearance.header(root, "ota-app", "Прошивка MeshCore", logo=True).grid(row=0, column=0, sticky="ew")

        body = ttk.Frame(root, style="Screen.TFrame")
        body.grid(row=1, column=0, sticky="nsew")
        body.columnconfigure(0, weight=1)
        body.rowconfigure(0, weight=1)
        self.body_view = tk.Canvas(body, background=SCREEN, borderwidth=0, highlightthickness=0, width=1, height=1)
        self.body_view.grid(row=0, column=0, sticky="nsew")
        self.body_bar = ttk.Scrollbar(body, orient="vertical", command=self.body_view.yview, style="Mesh.Vertical.TScrollbar")
        self.body_bar.grid(row=0, column=1, sticky="ns")
        self.body_view.configure(yscrollcommand=self.body_bar.set)
        self.panel = ttk.Frame(self.body_view, style="Screen.TFrame", padding=(px(20), px(18)))
        self.panel_window = self.body_view.create_window(0, 0, window=self.panel, anchor="nw")
        self.panel.columnconfigure((0, 1), weight=1, uniform="card")
        self.connection = ttk.Frame(self.panel, style="Card.TFrame", padding=px(16))
        self.firmware = ttk.Frame(self.panel, style="Card.TFrame", padding=px(16))
        self.connection.columnconfigure(0, weight=1)
        self.firmware.columnconfigure(0, weight=1)
        self.side_by_side = None
        self.body_width = 0
        self.body_view.bind("<Configure>", self.layout_cards)
        self.panel.bind("<Configure>", self.fit_body)
        root.bind("<MouseWheel>", self.scroll_body)
        root.bind("<Button-4>", self.scroll_body)
        root.bind("<Button-5>", self.scroll_body)
        root.bind("<FocusIn>", self.reveal_focus, add="+")

        connection = self.connection
        ttk.Label(connection, text="Подключение", style="Section.TLabel").grid(row=0, column=0, sticky="w")
        self.method = ttk.Combobox(connection, values=(USB, BLE, TCP, DFU), width=1,
                                  state="readonly", style="Mesh.TCombobox")
        self.method.grid(row=1, column=0, sticky="ew", pady=(px(10), px(8)))
        self.method.current(0)
        self.hint = tk.StringVar()
        self.hint_label = ttk.Label(connection, textvariable=self.hint, style="Surface.TLabel", justify="left")
        self.hint_label.grid(row=2, column=0, sticky="ew", pady=(0, px(10)))
        self.target_label = ttk.Label(connection, style="FieldTitle.TLabel")
        self.target_label.grid(row=3, column=0, sticky="w", pady=(0, px(4)))
        self.address_text = tk.StringVar()
        self.address = ttk.Combobox(connection, textvariable=self.address_text, width=1, style="Mesh.TCombobox")
        self.address.grid(row=4, column=0, sticky="ew")
        self.nrf = ttk.Frame(connection, style="Surface.TFrame")
        self.nrf.grid(row=5, column=0, sticky="ew", pady=(px(10), 0))
        self.nrf.columnconfigure(0, weight=3)
        self.nrf.columnconfigure(1, weight=2)
        ttk.Label(self.nrf, text="Модель платы", style="FieldTitle.TLabel").grid(row=0, column=0, sticky="w")
        self.board = ttk.Combobox(self.nrf, values=tuple(BOARD_NAMES), state="readonly", width=1, style="Mesh.TCombobox")
        self.board.grid(row=1, column=0, sticky="ew", padx=(0, px(8)), pady=(px(4), 0))
        self.board.bind("<<ComboboxSelected>>", lambda _: self.controls())
        ttk.Label(self.nrf, text="Роль для GitHub", style="FieldTitle.TLabel").grid(row=0, column=1, sticky="w")
        self.role = ttk.Combobox(self.nrf, values=tuple(ROLE_NAMES), state="readonly", width=1, style="Mesh.TCombobox")
        self.role.grid(row=1, column=1, sticky="ew", pady=(px(4), 0))
        self.details = tk.StringVar()
        self.details_label = ttk.Label(connection, textvariable=self.details, style="Summary.TLabel", justify="left")
        self.details_label.grid(row=6, column=0, sticky="ew", pady=(px(10), 0))
        self.scan = ttk.Button(connection, text="Найти устройства", style="Secondary.TButton", command=lambda: self.start("scan"))
        self.scan.grid(row=7, column=0, sticky="ew", pady=(px(12), px(2)))
        actions = ttk.Frame(connection, style="Surface.TFrame")
        actions.grid(row=8, column=0, sticky="ew", pady=(px(3), 0))
        actions.columnconfigure(0, weight=1)
        actions.columnconfigure(1, weight=2)
        ttk.Button(actions, text="ИНСТРУКЦИЯ", style="Link.TButton", command=self.help).grid(row=0, column=0, sticky="ew")
        self.info = ttk.Button(actions, text="Проверить устройство", style="Link.TButton", command=lambda: self.start("info"))
        self.info.grid(row=0, column=1, sticky="ew")
        self.disk_button = ttk.Button(connection, text="Диск UF2…", style="Link.TButton", command=self.pick_disk)
        self.disk_button.grid(row=9, column=0, sticky="ew")

        firmware = self.firmware
        ttk.Label(firmware, text="Прошивка", style="Section.TLabel").grid(row=0, column=0, sticky="w")
        self.formats = tk.StringVar()
        self.format_label = ttk.Label(firmware, textvariable=self.formats, style="Surface.TLabel", justify="left")
        self.format_label.grid(row=1, column=0, sticky="w", pady=(px(10), px(5)))
        file_line = ttk.Frame(firmware, style="Surface.TFrame")
        file_line.grid(row=2, column=0, sticky="ew")
        file_line.columnconfigure(0, weight=1)
        self.file = tk.StringVar()
        self.file_entry = ttk.Entry(file_line, textvariable=self.file, width=1, state="readonly", style="Mesh.TEntry")
        self.file_entry.grid(row=0, column=0, sticky="nsew")
        self.clear = ttk.Button(file_line, text="Снять", style="Link.TButton", command=lambda: self.file.set(""))
        self.clear.grid(row=0, column=1, padx=(px(4), 0))
        files = ttk.Frame(firmware, style="Surface.TFrame")
        files.grid(row=3, column=0, sticky="ew", pady=(px(10), px(10)))
        files.columnconfigure((0, 1), weight=1, uniform="browse")
        self.browse = ttk.Button(files, text="Выбрать файл", style="Secondary.TButton", command=self.choose)
        self.browse.grid(row=0, column=0, sticky="ew", padx=(0, px(4)))
        self.github = ttk.Button(files, text="Из GitHub", style="Secondary.TButton", command=lambda: self.start("catalog"))
        self.github.grid(row=0, column=1, sticky="ew", padx=(px(4), 0))
        self.buttons = {}
        for row, (name, label) in enumerate((("update", "Обновить прошивку"), ("usb", "Прошить через USB"),
                            ("new", "Новая нода со сбросом…"), ("export", "Сохранить DFU ZIP"))):
            button = ttk.Button(firmware, text=label, style="Primary.TButton" if name == "update" else "Link.TButton",
                                command=lambda op=name: self.start(op))
            button.grid(row=row + 4, column=0, sticky="ew", pady=(px(3), 0))
            self.buttons[name] = button

        self.status = tk.StringVar(value="Готово к работе")
        self.percent = tk.StringVar()
        meter = ttk.Frame(root, style="Surface.TFrame", padding=(px(20), px(9), px(20), px(4)))
        meter.grid(row=2, column=0, sticky="ew")
        meter.columnconfigure(0, weight=1)
        self.status_label = ttk.Label(meter, textvariable=self.status, style="FieldTitle.TLabel", justify="left")
        self.status_label.grid(row=0, column=0, sticky="ew")
        ttk.Label(meter, textvariable=self.percent, style="ProgressPercent.TLabel").grid(
            row=0, column=1, sticky="e", padx=(px(12), 0))
        self.progress = ttk.Progressbar(meter, mode="determinate", maximum=100, style="Mesh.Horizontal.TProgressbar")
        self.progress.grid(row=1, column=0, columnspan=2, sticky="ew", pady=(px(6), 0))
        self.progress.grid_remove()

        journal = ttk.Frame(root, style="Surface.TFrame", padding=(px(20), 0, px(20), px(10)))
        journal.grid(row=3, column=0, sticky="ew")
        journal.columnconfigure(0, weight=1)
        handle = tk.Frame(journal, width=px(32), height=px(3), background=BORDER, cursor="hand2")
        handle.grid(row=1, column=0, pady=(px(6), 0))
        handle.bind("<ButtonRelease-1>", lambda _: self.toggle_journal())
        journal_header = ttk.Frame(journal, style="Surface.TFrame")
        journal_header.grid(row=2, column=0, sticky="ew")
        journal_header.columnconfigure(0, weight=1)
        ttk.Label(journal_header, text="Журнал", style="FieldTitle.TLabel").grid(row=0, column=0, sticky="w")
        self.toggle = ttk.Button(journal_header, text="Развернуть", style="Link.TButton", command=self.toggle_journal)
        self.toggle.grid(row=0, column=1, sticky="e")
        self.log_area = ttk.Frame(journal, style="Field.TFrame")
        self.log_area.grid(row=4, column=0, sticky="ew", pady=(px(6), 0))
        self.log_area.columnconfigure(0, weight=1)
        self.log = tk.Text(self.log_area, wrap="word", width=1, height=7, state="disabled",
                           background=FIELD, foreground=INK, selectbackground=ACCENT, selectforeground=PANEL,
                           borderwidth=0, highlightthickness=1, highlightbackground=BORDER, highlightcolor=ACCENT,
                           padx=px(10), pady=px(8), font="TkFixedFont")
        self.log.grid(row=0, column=0, sticky="ew")
        scroll = ttk.Scrollbar(self.log_area, orient="vertical", command=self.log.yview, style="Mesh.Vertical.TScrollbar")
        scroll.grid(row=0, column=1, sticky="ns")
        self.log.configure(yscrollcommand=scroll.set)
        self.log_area.grid_remove()
        self.log_lines = tk.StringVar()
        self.preview = ttk.Label(journal, textvariable=self.log_lines, style="Surface.TLabel", justify="left")
        self.preview.grid(row=3, column=0, sticky="ew")
        self.preview.grid_remove()

        self.method.bind("<<ComboboxSelected>>", self.changed)
        self.address.bind("<<ComboboxSelected>>", self.target_changed)
        self.address_text.trace_add("write", self.update_summary)
        self.file.trace_add("write", self.file_changed)
        self.changed()
        root.protocol("WM_DELETE_WINDOW", self.close)
        root.after(100, self.poll)
        root.after(1500, self.refresh)

    def layout_cards(self, event):
        px = self.appearance.px
        wide = event.width >= px(796)
        if event.width != self.body_width:
            self.body_width = event.width
            self.body_view.itemconfigure(self.panel_window, width=event.width)
            card_width = (event.width - px(54)) // 2 if wide else event.width - px(40)
            self.wrap_labels(card_width - px(32), self.hint_label, self.format_label)
            self.wrap_labels(card_width - px(54), self.details_label)
            self.wrap_labels(event.width - px(40), self.preview)
            self.wrap_labels(event.width - px(96), self.status_label)
        if self.side_by_side != wide:
            self.side_by_side = wide
            self.connection.grid(row=0, column=0, columnspan=1 if wide else 2, sticky="new",
                                 padx=(0, px(7) if wide else 0))
            self.firmware.grid(row=0 if wide else 1, column=1 if wide else 0, columnspan=1 if wide else 2,
                               sticky="new", padx=(px(7) if wide else 0, 0), pady=(0 if wide else px(14), 0))
        self.fit_body()

    def fit_body(self, _=None):
        width = self.body_view.winfo_width()
        region = f"0 0 {width} {self.panel.winfo_reqheight()}"
        if self.body_view.cget("scrollregion") != region:
            self.body_view.configure(scrollregion=region)

    def wrap_labels(self, width, *labels):
        width = max(self.appearance.px(80), width)
        for label in labels:
            label.configure(wraplength=width)

    def contains_body(self, widget):
        return str(widget).startswith(str(self.body_view) + ".") or widget == self.body_view

    def scroll_body(self, event):
        if not self.contains_body(event.widget) or self.body_view.yview() == (0.0, 1.0):
            return
        if event.num in (4, 5):
            step = -3 if event.num == 4 else 3
        else:
            delta = event.delta
            if not delta:
                return
            step = -delta if sys.platform == "darwin" else (-1 if delta > 0 else 1) * max(1, abs(delta) // 40)
        self.body_view.yview_scroll(step, "units")

    def reveal_focus(self, event):
        widget = event.widget
        if not self.contains_body(widget) or not widget.winfo_ismapped():
            return
        view = self.body_view
        top = widget.winfo_rooty() - view.winfo_rooty()
        bottom = top + widget.winfo_height()
        height = view.winfo_height()
        content_height = self.panel.winfo_reqheight()
        if top < 0:
            view.yview_moveto(max(0, (view.canvasy(0) + top) / content_height))
        elif bottom > height:
            view.yview_moveto((view.canvasy(0) + bottom - height) / content_height)

    def file_changed(self, *_):
        self.file_entry.xview_moveto(1)
        self.controls()

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
            self.log_area.grid()
            self.log.see("end")
        else:
            focused = self.root.focus_get()
            if focused and str(focused).startswith(str(self.log_area) + "."):
                self.toggle.focus_set()
            self.log_area.grid_remove()
        self.toggle.configure(text="Свернуть" if self.journal_open else "Развернуть")
        self.update_preview()

    def update_preview(self):
        if self.journal_open or not self.log_lines.get():
            self.preview.grid_remove()
        else:
            self.preview.grid()

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
        if self.details.get():
            self.details_label.grid()
        else:
            self.details_label.grid_remove()

    def controls(self):
        transport = self.method.get()
        is_uf2 = pathlib.Path(self.file.get()).suffix.lower() == ".uf2"
        nrf = transport == DFU or self.disk is not None
        self.hint.set(HINTS[transport])
        self.target_label.configure(text="Загрузчик" if transport == DFU else "Адрес платы" if transport == TCP else "Устройство")
        self.formats.set("Файл прошивки  " + ", ".join(file_formats(transport, self.disk is not None)))
        self.method.configure(state="disabled" if self.busy else "readonly")
        self.address.configure(state="disabled" if self.busy or self.disk else "readonly" if transport in (USB, DFU) else "normal")
        self.scan.configure(state="disabled" if self.busy or transport == TCP else "normal",
                            text={USB: "Найти USB-устройства", BLE: "Найти Bluetooth-устройства", DFU: "Найти загрузчик"}.get(transport, "Найти устройства"))
        if transport == TCP:
            self.scan.grid_remove()
        else:
            self.scan.grid()
        self.info.configure(state="disabled" if self.busy or transport == DFU else "normal")
        if transport == DFU:
            self.info.grid_remove()
        else:
            self.info.grid()
        self.disk_button.configure(state="normal" if transport == USB and not self.busy else "disabled")
        self.disk_button.configure(text="Закрыть диск UF2" if self.disk else "Диск UF2…")
        if transport == USB:
            self.disk_button.grid()
        else:
            self.disk_button.grid_remove()
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
            visible = transport == USB and not (self.disk or is_uf2)
            enabled = not self.busy and visible
            self.buttons[name].configure(state="normal" if enabled else "disabled")
            if visible:
                self.buttons[name].grid()
            else:
                self.buttons[name].grid_remove()
        if transport == DFU:
            self.buttons["export"].grid()
            self.buttons["export"].configure(state="normal" if not self.busy and is_uf2 else "disabled")
        else:
            self.buttons["export"].grid_remove()

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
        self.percent.set("")
        self.progress.configure(value=0)
        self.progress.grid()
        self.log.configure(state="normal")
        self.log.delete("1.0", "end")
        self.log.configure(state="disabled")
        self.log_lines.set("")
        self.update_preview()
        disk = self.disk[0] if self.disk else None
        threading.Thread(target=self.worker, args=(op, transport, target, path, board, disk), daemon=True).start()

    def worker(self, op, transport, target, path, board, disk):
        stream = LogStream(self.events)

        def progress(done, total):
            self.events.put(("progress", (done, total)))

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
                    directory = pathlib.Path.home() / ".ota-app" / "firmware"
                    self.events.put(("file", str(firmware_catalog.download(target, directory, progress=progress))))
                elif op == "export":
                    nrf_flash.export_dfu_zip(path, target)
                elif op == "update" and disk:
                    nrf_flash.write_uf2(path, disk, board, progress=progress)
                elif op == "update" and transport == DFU:
                    asyncio.run(nrf_flash.install_dfu(target, path, board, progress=progress))
                elif op in ("usb", "new"):
                    args = Namespace(port=target, image=pathlib.Path(path), check=False, new_node=op == "new")
                    usb_flash.install(args, progress=progress)
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
                        asyncio.run(ota_app.main(["--" + flag, target, path], progress=progress))
            self.events.put(("done", (True, op)))
        except (Exception, SystemExit) as exc:
            self.events.put(("log", f"\n{exc or type(exc).__name__}\n"))
            self.events.put(("done", (False, op)))

    def poll(self):
        while not self.events.empty():
            kind, value = self.events.get_nowait()
            if kind == "log":
                following = self.log.yview()[1] >= 0.999
                self.log.configure(state="normal")
                self.log.insert("end", value)
                if following:
                    self.log.see("end")
                self.log.configure(state="disabled")
                last_lines = self.log.get("end-5lines", "end-1c").splitlines()
                visible_lines = [line.strip()[:170] for line in last_lines if line.strip()]
                self.log_lines.set("\n".join(visible_lines[-2:]))
                self.update_preview()
            elif kind == "progress":
                self.show_progress(*value)
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
                self.finish_progress(success)
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

    def show_progress(self, done, total):
        if not total or not self.busy:
            return
        percent = max(0, min(99, int(100 * done / total)))
        self.progress.configure(value=percent)
        self.percent.set(f"{percent}%")
        self.status.set("Завершение операции…" if done >= total else "Передача файла…")

    def finish_progress(self, success):
        if not self.percent.get():
            self.progress.grid_remove()
        elif success:
            self.progress.configure(value=100)
            self.percent.set("100%")

    def show_catalog(self, catalog):
        assets = catalog["assets"]
        if not assets:
            self.notice("В выпусках GitHub нет файлов для этой платы, роли и сборки.")
            return
        dialog = tk.Toplevel(self.root)
        dialog.title("Прошивки из GitHub")
        dialog.transient(self.root)
        dialog.configure(background=SCREEN)
        px = self.appearance.px
        width = min(px(740), self.root.winfo_screenwidth() - px(48))
        max_height = self.root.winfo_screenheight() - px(96)

        def resize():
            dialog.update_idletasks()
            dialog.geometry(f"{max(width, dialog.winfo_width()) if dialog.winfo_ismapped() else width}x"
                            f"{min(dialog.winfo_reqheight(), max_height)}")

        summary = catalog["device"]["summary"]
        self.appearance.header(dialog, "Прошивки из GitHub", "MeshCore").pack(fill="x")
        frame = ttk.Frame(dialog, style="Screen.TFrame", padding=px(16))
        frame.pack(fill="both", expand=True)
        ttk.Label(frame, text=summary, wraplength=width - px(34), style="Screen.TLabel").pack(anchor="w", pady=(0, px(10)))
        recommended = catalog["recommended"]

        def download(index):
            asset = assets[index]
            if asset["issue"] and not messagebox.askyesno("Скачать из архива", asset["issue"] + ". Скачать файл?", parent=dialog):
                return
            dialog.destroy()
            self.launch("download", self.method.get(), asset, "")

        def file_kind(asset):
            return asset["format"] + (f" · для сборки №{asset['base']}" if asset["base"] else "")

        def file_size(asset):
            size = asset["size"]
            return f"{size / 1024:.1f} КиБ" if size >= 1024 else f"{size} Б"

        if recommended is None:
            ttk.Label(frame, text="Подходящих обновлений для этой сборки нет.",
                      style="Screen.TLabel", wraplength=width - px(34)).pack(anchor="w", pady=(0, px(6)))
        else:
            choice = assets[recommended]
            featured = ttk.Frame(frame, style="Recommendation.TFrame", padding=px(14))
            featured.pack(fill="x")
            featured.columnconfigure(0, weight=1)
            ttk.Label(featured, text="Подходит для вашего устройства", style="RecommendTitle.TLabel").grid(
                row=0, column=0, sticky="w")
            ttk.Label(featured, text=choice["tag"], style="RecommendBuild.TLabel").grid(
                row=1, column=0, sticky="w", pady=(px(4), 0))
            details = f"{file_kind(choice)} · {file_size(choice)}"
            if choice["prerelease"]:
                details += " · предварительный выпуск"
            ttk.Label(featured, text=details, style="Recommend.TLabel", wraplength=width - px(72)).grid(
                row=2, column=0, sticky="w", pady=(px(4), px(10)))
            ttk.Button(featured, text="Скачать", style="RecommendAction.TButton",
                       command=lambda: download(recommended)).grid(row=3, column=0, sticky="ew")

        footer = ttk.Frame(frame, style="Screen.TFrame")
        footer.pack(side="bottom", fill="x", pady=(px(10), 0))
        ttk.Button(footer, text="Закрыть", style="Page.TButton", command=dialog.destroy).pack(side="right")
        other_assets = [(index, asset) for index, asset in enumerate(assets) if index != recommended]
        if other_assets:
            archive = ttk.Frame(frame, style="Screen.TFrame")
            archive.columnconfigure(0, weight=1)
            archive.rowconfigure(0, weight=1)
            columns = ("release", "format", "size", "status")
            tree = ttk.Treeview(archive, columns=columns, show="headings", selectmode="browse", style="Mesh.Treeview",
                                height=4 if self.root.winfo_screenheight() < px(650) else 6)
            tree.grid(row=0, column=0, sticky="nsew")
            scrollbar = ttk.Scrollbar(archive, orient="vertical", style="Mesh.Vertical.TScrollbar", command=tree.yview)
            scrollbar.grid(row=0, column=1, sticky="ns")
            across = ttk.Scrollbar(archive, orient="horizontal", command=tree.xview)
            across.grid(row=1, column=0, sticky="ew")
            tree.configure(yscrollcommand=scrollbar.set, xscrollcommand=across.set)
            for name, label, col_width in zip(columns, ("Выпуск", "Формат", "Размер", "Состояние"), (150, 155, 80, 275)):
                tree.heading(name, text=label)
                tree.column(name, width=px(col_width), minwidth=px(col_width), stretch=name == "status")
            for index, asset in other_assets:
                state = asset["issue"] or ("Подходит · предварительный" if asset["prerelease"] else "Подходит")
                tree.insert("", "end", iid=str(index), values=(asset["tag"], file_kind(asset), file_size(asset), state))

            def download_selected(_=None):
                selection = tree.selection()
                if selection:
                    download(int(selection[0]))

            archive_download = ttk.Button(footer, text="Скачать выбранное", style="Page.TButton", command=download_selected)
            archive_download.state(["disabled"])
            tree.bind("<<TreeviewSelect>>", lambda _: archive_download.state(["!disabled"] if tree.selection() else ["disabled"]))
            tree.bind("<Return>", download_selected)
            tree.bind("<Double-1>", lambda event: tree.identify_row(event.y) and download_selected())

            def toggle_archive():
                if archive.winfo_manager():
                    archive.pack_forget()
                    archive_download.pack_forget()
                    toggle.configure(text=f"Другие файлы ({len(other_assets)})")
                else:
                    archive.pack(fill="both", expand=True, pady=(px(10), 0))
                    archive_download.pack(side="right")
                    if recommended is not None:
                        toggle.configure(text="Скрыть список")
                resize()

            if recommended is None:
                toggle_archive()
            else:
                toggle = ttk.Button(footer, text=f"Другие файлы ({len(other_assets)})", style="Page.TButton",
                                    command=toggle_archive)
                toggle.pack(side="left")

        dialog.bind("<Escape>", lambda _: dialog.destroy())
        dialog.minsize(min(px(420), width), px(200))
        resize()
        dialog.update_idletasks()
        dialog.grab_set()


if __name__ == "__main__":
    multiprocessing.freeze_support()
    root = tk.Tk()
    ui = Flasher(root)
    if "--smoke-test" in sys.argv:
        assert ui.method.get() == USB and set(ui.buttons) == {"update", "usb", "new", "export"}
        assert (usb_flash.ROOT / "t3s3.csv").is_file()
        ui.layout_cards(Namespace(width=ui.appearance.px(490)))
        assert ui.side_by_side is False and ui.firmware.grid_info()["row"] == 1
        ui.layout_cards(Namespace(width=ui.appearance.px(900)))
        assert ui.side_by_side is True and ui.firmware.grid_info()["column"] == 1
        ui.busy = True
        ui.progress.grid()
        for done, expected in ((0, "0%"), (25, "25%"), (100, "99%")):
            ui.events.put(("progress", (done, 100)))
            ui.poll()
            assert ui.percent.get() == expected
        ui.events.put(("done", (True, "download")))
        ui.poll()
        assert ui.percent.get() == "100%" and float(ui.progress["value"]) == 100
        ui.percent.set("")
        ui.finish_progress(success=True)
        assert not ui.progress.winfo_manager()
        ui.method.current(2)
        ui.changed()
        assert ".bin" not in file_formats(ui.method.get())
        ui.toggle_journal()
        assert ui.log_area.winfo_manager() == "grid"
        ui.toggle_journal()
        assert not ui.log_area.winfo_manager()
        ui.method.current(3)
        ui.changed()
        assert file_formats(ui.method.get()) == (".uf2",)
        ui.show_catalog({"device": {"summary": "Heltec T114 · Компаньон"}, "recommended": 0,
                         "assets": [{"tag": "v0.9.0-r194", "format": ".uf2", "base": 0,
                                     "size": 549888, "issue": "", "prerelease": False}]})
        dialog = root.grab_current()
        assert isinstance(dialog, tk.Toplevel)
        dialog.destroy()
        root.destroy()
    else:
        root.mainloop()
