#!/bin/bash
set -euo pipefail

build_dir="$(mktemp -d)"
python_pkg="$build_dir/python.pkg"
curl -fsSL --retry 3 https://www.python.org/ftp/python/3.11.9/python-3.11.9-macos11.pkg -o "$python_pkg"
pkgutil --check-signature "$python_pkg" | tee "$build_dir/signature.txt"
grep -F 'Python Software Foundation' "$build_dir/signature.txt"
sudo installer -pkg "$python_pkg" -target /
/Library/Frameworks/Python.framework/Versions/3.11/bin/python3.11 -m venv "$build_dir/universal"
source "$build_dir/universal/bin/activate"

export ARCHFLAGS='-arch x86_64 -arch arm64'
export MACOSX_DEPLOYMENT_TARGET=11.0
python -m pip install -q -r requirements.txt 'pyinstaller>=6.12,<7' pillow

# Some dependencies provide architecture-specific wheels. Merge each extension
# with its counterpart from the same distribution version before freezing.
python - <<'PY'
import importlib.metadata
from pathlib import Path
import subprocess
import tempfile
import zipfile

magics = {bytes.fromhex(value) for value in (
    "cafebabe", "cafebabf", "feedface", "feedfacf", "cefaedfe", "cffaedfe",
)}
for distribution in importlib.metadata.distributions():
    missing_arm = []
    for entry in distribution.files or ():
        path = Path(distribution.locate_file(entry))
        if not path.is_file():
            continue
        with path.open("rb") as file:
            if file.read(4) not in magics:
                continue
        if "arm64" not in subprocess.check_output(["lipo", "-archs", path], text=True).split():
            missing_arm.append((path, str(entry)))
    if not missing_arm:
        continue
    with tempfile.TemporaryDirectory() as temporary:
        package = f"{distribution.metadata['Name']}=={distribution.version}"
        subprocess.run(["python", "-m", "pip", "download", "--platform", "macosx_11_0_arm64",
                        "--only-binary=:all:", "--no-deps", "-d", temporary, package], check=True)
        directory = Path(temporary)
        with zipfile.ZipFile(next(directory.glob("*.whl"))) as wheel:
            for path, relative in missing_arm:
                arm = directory / "arm"
                arm.write_bytes(wheel.read(relative))
                fat = directory / "fat"
                subprocess.run(["lipo", "-create", path, arm, "-output", fat], check=True)
                path.write_bytes(fat.read_bytes())
PY

sed 's/name="MeshCore",console/target_arch="universal2",name="MeshCore",console/' MeshCore.spec > Universal.spec
python -m PyInstaller --clean --noconfirm Universal.spec
lipo dist/MeshCore.app/Contents/MacOS/MeshCore -verify_arch arm64 x86_64
dist/MeshCore.app/Contents/MacOS/MeshCore --smoke-test
mkdir -p out
ditto -c -k --sequesterRsrc --keepParent dist/MeshCore.app out/MeshCore-macOS-Apple-Silicon.zip
