$ErrorActionPreference='Stop'
$ProgressPreference='SilentlyContinue'
$root=(Get-Location).Path
Invoke-WebRequest https://www.python.org/ftp/python/3.11.9/python-3.11.9-embed-win32.zip -OutFile python.zip
Expand-Archive python.zip engine
python -m pip install --no-compile --target engine -r requirements.txt
if($LASTEXITCODE){exit $LASTEXITCODE}
Copy-Item usb_flash.py,ota_app.py,meshcore_ota.py,*.csv engine
& .\engine\python.exe -X utf8 .\engine\ota_app.py --help
if($LASTEXITCODE){exit $LASTEXITCODE}
& .\engine\python.exe -c "import usb_flash,bleak.backends.winrt.client,struct; assert struct.calcsize('P')==4"
if($LASTEXITCODE){exit $LASTEXITCODE}
[IO.Compression.ZipFile]::CreateFromDirectory("$root\engine","$root\engine.zip")
$lib=(Get-ChildItem "$env:WINDIR\Microsoft.NET\assembly\GAC_MSIL\System.Management.Automation" -Recurse -Filter System.Management.Automation.dll|Select-Object -First 1).FullName
& "$env:WINDIR\Microsoft.NET\Framework\v4.0.30319\csc.exe" /nologo /target:winexe /platform:x86 /win32icon:ommesh.ico "/reference:$lib" /reference:System.Windows.Forms.dll,System.IO.Compression.dll,System.IO.Compression.FileSystem.dll /out:MeshCore-Windows-x86-x64.exe /resource:windows\MeshCore.ps1,MeshCore.ps1 /resource:ommesh.ico,ommesh.ico /resource:engine.zip,engine.zip windows\TaskbarLauncher.cs
exit $LASTEXITCODE
