"""Русский интерфейс для USB, Bluetooth и TCP на macOS и Linux."""
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
import ota_app
import usb_flash


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
        root.title("Обновление MeshCore")
        self.events = queue.Queue()
        self.busy = False
        self.labels = {}
        self.ble = {}
        panel = ttk.Frame(root, padding=14)
        panel.pack(fill="both", expand=True)
        panel.columnconfigure(2, weight=1)
        panel.rowconfigure(5, weight=1)
        ttk.Label(panel, text="Подключение").grid(row=0, column=0, sticky="w")
        self.method = ttk.Combobox(panel, values=("USB", "Bluetooth", "Wi-Fi / TCP"), width=13, state="readonly")
        self.method.grid(row=0, column=1, padx=6)
        self.method.current(0)
        self.address = ttk.Combobox(panel, width=36)
        self.address.grid(row=0, column=2, sticky="ew")
        self.scan = ttk.Button(panel, text="Найти устройства", command=lambda: self.start("scan"))
        self.scan.grid(row=0, column=3, padx=6)
        ttk.Label(panel, text="Файл прошивки").grid(row=1, column=0, sticky="w", pady=10)
        self.file = tk.StringVar()
        ttk.Entry(panel, textvariable=self.file, state="readonly").grid(row=1, column=1, columnspan=2, sticky="ew", padx=6)
        self.browse = ttk.Button(panel, text="Выбрать файл", command=self.choose)
        self.browse.grid(row=1, column=3)
        bar = ttk.Frame(panel)
        bar.grid(row=2, column=0, columnspan=4, sticky="w", pady=6)
        self.buttons = {}
        for name, label in (("info","Версия устройства"),("update","Обновить прошивку"),("usb","Прошить через USB"),("new","Новая нода со сбросом…")):
            btn = ttk.Button(bar, text=label, command=lambda op=name: self.start(op))
            btn.pack(side="left", padx=2)
            self.buttons[name] = btn
        ttk.Button(bar, text="Как начать", command=self.help).pack(side="left", padx=2)
        self.status = tk.StringVar(value="Готово к работе")
        ttk.Label(panel, textvariable=self.status).grid(row=4, column=0, columnspan=4, sticky="w", pady=6)
        self.log = ScrolledText(panel, width=84, height=15, state="disabled")
        self.log.grid(row=5, column=0, columnspan=4, sticky="nsew")
        self.method.bind("<<ComboboxSelected>>", self.changed)
        self.changed()
        root.protocol("WM_DELETE_WINDOW", self.close)
        root.after(100, self.poll)
        root.after(1200, self.refresh)

    def help(self):
        messagebox.showinfo("Как начать", "Выберите подключение и устройство. Для TCP укажите адрес:порт. Для Bluetooth откройте окно обновления на устройстве и нажмите «Найти устройства». При запросе системы подтвердите сопряжение.\n\nПроверьте связь кнопкой «Версия устройства». Для обновления выберите .mcfw, .mcfd или .bin вашей модели. USB-загрузчик и сброс доступны для поддерживаемых ESP с работающей meshcore-rs.")

    def choose(self):
        path = filedialog.askopenfilename(filetypes=[("Прошивка", "*.mcfw *.mcfd *.bin"), ("Все файлы", "*")])
        if path:
            self.file.set(path)
        return path

    def ports(self):
        current = self.labels.get(self.address.get(), self.address.get())
        self.labels = {f"{p.description} ({p.device})": p.device for p in sorted(list_ports.comports())}
        self.address["values"] = tuple(self.labels)
        selected = next((s for s,p in self.labels.items() if p == current), "")
        if not selected and len(self.labels) == 1:
            selected = next(iter(self.labels))
        self.address.set(selected)

    def changed(self, event=None):
        self.address.set("")
        self.address["values"] = ()
        if self.method.get() == "USB":
            self.ports()
        elif self.method.get() == "Bluetooth":
            self.address["values"] = tuple(self.ble)
        self.controls()

    def controls(self):
        usb = self.method.get() == "USB"
        self.method.configure(state="disabled" if self.busy else "readonly")
        self.address.configure(state="disabled" if self.busy else ("readonly" if usb else "normal"))
        self.browse.configure(state="disabled" if self.busy else "normal")
        self.scan.configure(state="disabled" if self.busy or self.method.get()=="Wi-Fi / TCP" else "normal")
        for name,btn in self.buttons.items():
            btn.configure(state="disabled" if self.busy or (name in ("usb","new") and not usb) else "normal")

    def refresh(self):
        if not self.busy and self.method.get() == "USB":
            self.ports()
        self.root.after(1500, self.refresh)

    def close(self):
        if self.busy:
            messagebox.showinfo("MeshCore", "Дождитесь завершения текущей операции.")
        else:
            self.root.destroy()

    def start(self, op):
        if self.busy:
            return
        transport = self.method.get()
        value = self.address.get().strip()
        target = self.labels.get(value,value) if transport=="USB" else self.ble.get(value,value) if transport=="Bluetooth" else value
        if op=="scan" and transport=="USB":
            self.ports()
            self.status.set(f"Портов найдено: {len(self.labels)}")
            return
        if op!="scan" and not target:
            messagebox.showinfo("MeshCore","Выберите порт или укажите адрес устройства.")
            return
        if transport=="Wi-Fi / TCP" and not re.fullmatch(r".+:[0-9]{1,5}",target):
            messagebox.showinfo("MeshCore","Укажите адрес и порт, например 192.168.1.50:5000.")
            return
        path = self.file.get()
        if op in ("update","usb","new"):
            path = path or self.choose()
            if not path:
                return
            if op in ("usb", "new") and pathlib.Path(path).suffix.lower() == ".mcfd":
                messagebox.showinfo("MCFD", "Для дельты нажмите «Обновить прошивку».")
                return
            msg = f"Установить {pathlib.Path(path).name} на {target}?"
            if op=="new":
                msg += "\n\nПрежний ключ, контакты, каналы, настройки и сопряжения обеих ролей будут удалены."
            if not messagebox.askyesno("Подтвердите прошивку",msg,default="no"):
                return
        self.busy=True
        self.controls()
        self.status.set("Выполняется операция…")
        self.log.configure(state="normal")
        self.log.delete("1.0","end")
        self.log.configure(state="disabled")
        threading.Thread(target=self.worker,args=(op,transport,target,path),daemon=True).start()

    def worker(self,op,transport,target,path):
        stream=LogStream(self.events)
        old_argv=sys.argv
        try:
            with contextlib.redirect_stdout(stream), contextlib.redirect_stderr(stream):
                if op=="scan":
                    found=asyncio.run(BleakScanner.discover(timeout=10,return_adv=True))
                    devices={}
                    for device,adv in found.values():
                        name=adv.local_name or device.name or ""
                        if name.startswith("MeshCore"):
                            devices[f"{name} ({device.address})"]=device.address
                    self.events.put(("ble",devices))
                elif op in ("usb","new"):
                    usb_flash.install(Namespace(port=target,image=pathlib.Path(path),check=False,new_node=op=="new"))
                else:
                    flag={"USB":"--port","Bluetooth":"--ble","Wi-Fi / TCP":"--tcp"}[transport]
                    sys.argv=["ota-app",flag,target,"--info" if op=="info" else path]
                    asyncio.run(ota_app.main())
            self.events.put(("done","Операция завершена."))
        except (Exception,SystemExit) as exc:
            self.events.put(("log",f"\n{exc}\n"))
            self.events.put(("done","Ошибка. Подробности в журнале."))
        finally:
            sys.argv=old_argv

    def poll(self):
        while not self.events.empty():
            kind,value=self.events.get_nowait()
            if kind=="log":
                self.log.configure(state="normal")
                self.log.insert("end",value)
                self.log.see("end")
                self.log.configure(state="disabled")
            elif kind=="ble":
                self.ble=value
                self.address["values"]=tuple(value)
                self.address.set(next(iter(value)) if len(value)==1 else "")
                self.events.put(("log",f"Устройств Bluetooth найдено: {len(value)}\n"))
            else:
                self.busy=False
                self.status.set(value)
                self.controls()
        self.root.after(100,self.poll)


if __name__=="__main__":
    multiprocessing.freeze_support()
    root=tk.Tk()
    ui=Flasher(root)
    if "--smoke-test" in sys.argv:
        assert ui.method.get()=="USB" and set(ui.buttons)=={"info","update","usb","new"}
        assert (usb_flash.ROOT/"t3s3.csv").is_file()
        root.after(100,root.destroy)
    root.mainloop()
