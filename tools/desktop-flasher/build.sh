#!/bin/bash
set -euo pipefail
python -m pip install -r requirements.txt "pyinstaller>=6.12,<7" pillow
python -m PyInstaller --clean --noconfirm MeshCore.spec
mkdir -p out
if [[ "$(uname -s)" == Darwin ]]; then
  dist/MeshCore.app/Contents/MacOS/MeshCore --smoke-test
  ditto -c -k --sequesterRsrc --keepParent dist/MeshCore.app out/MeshCore-macOS-Apple-Silicon.zip
else
  xvfb-run -a dist/MeshCore --smoke-test
  case "$1" in
    x86_64) platform="PC-x86_64" ;;
    aarch64) platform="Raspberry-Pi-ARM64" ;;
    armv7l) platform="Raspberry-Pi-ARMv7-32bit" ;;
    *) echo "Unsupported Linux architecture: $1" >&2; exit 1 ;;
  esac
  tar -czf "out/MeshCore-Linux-$platform.tar.gz" -C dist MeshCore
fi
