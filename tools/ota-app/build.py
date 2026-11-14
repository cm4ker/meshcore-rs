"""The release's ota-app program for the system this runs on: ota_app.py and everything it needs in
one file, so nobody has to install Python to update a device. CI builds it on Windows, macOS and
Linux for every release (.github/workflows/release.yml). By hand:

    pip install -r tools/ota-app/requirements.txt
    python tools/ota-app/build.py [<output directory>]      # default: tools/ota-app/dist
"""

import pathlib
import platform
import subprocess
import sys

HERE = pathlib.Path(__file__).resolve().parent


def program_name():
    """ota-app-<system>-<cpu>, e.g. ota-app-windows-x64 (Windows adds .exe)."""
    system = {"win32": "windows", "darwin": "macos"}.get(sys.platform, "linux")
    machine = platform.machine().lower()
    cpu = {"amd64": "x64", "x86_64": "x64", "arm64": "arm64", "aarch64": "arm64"}.get(machine, machine)
    return f"ota-app-{system}-{cpu}"


def main():
    out = pathlib.Path(sys.argv[1]).resolve() if len(sys.argv) > 1 else HERE / "dist"
    work = out / "build"
    name = program_name()
    subprocess.run([
        sys.executable, "-m", "PyInstaller", "--onefile", "--noconfirm", "--clean", "--name", name,
        # meshcore_ota.py, which ota_app.py takes from tools/ota-lora
        "--paths", str(HERE.parent / "ota-lora"),
        "--distpath", str(out), "--workpath", str(work), "--specpath", str(work),
        str(HERE / "ota_app.py"),
    ], check=True)
    print(out / (name + (".exe" if sys.platform == "win32" else "")))


if __name__ == "__main__":
    main()
