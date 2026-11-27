from pathlib import Path
import sys
from PyInstaller.utils.hooks import collect_all, collect_submodules

root=Path(SPECPATH)
data=[(str(p),".") for p in root.glob("*.csv")]+[(str(root/"ommesh.png"),".")]
esp_data,esp_bins,esp_imports=collect_all("esptool")
hidden=esp_imports+collect_submodules("bleak.backends")+["firmware_catalog","nrf_flash"]
a=Analysis([str(root/"app.py")],pathex=[str(root)],binaries=esp_bins,datas=data+esp_data,
           hiddenimports=hidden,excludes=["pytest","IPython"])
pyz=PYZ(a.pure)
mac=sys.platform=="darwin"
exe=EXE(pyz,a.scripts,a.binaries,a.datas,[],name="MeshCore",console=not mac,
        icon=str(root/"ommesh.ico") if mac else None)
if mac:
    app=BUNDLE(exe,name="MeshCore.app",icon=str(root/"ommesh.ico"),
               bundle_identifier="ru.ommesh.meshcoreflasher",
               info_plist={"NSBluetoothAlwaysUsageDescription":"Поиск и обновление устройств MeshCore.",
                           "NSBluetoothPeripheralUsageDescription":"Подключение к MeshCore по Bluetooth."})
