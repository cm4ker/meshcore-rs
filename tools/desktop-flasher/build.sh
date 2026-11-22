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
  tar -czf "out/MeshCore-Linux-$1.tar.gz" -C dist MeshCore
fi
