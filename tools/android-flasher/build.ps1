param([Parameter(Mandatory)][string]$ToolRoot,[Parameter(Mandatory)][string]$KeyStore,[Parameter(Mandatory)][string]$PasswordFile)
$ErrorActionPreference='Stop'
Push-Location $PSScriptRoot
try{
$jdk=(Get-ChildItem -LiteralPath $ToolRoot -Directory -Filter 'jdk-*'|Select -First 1).FullName
$env:JAVA_HOME=$jdk
$env:Path="$jdk\bin;$env:Path"
$android=(Resolve-Path "$ToolRoot/android-35/android.jar").Path
$bt=(Resolve-Path "$ToolRoot/android-15").Path
New-Item -Type Directory -Force build/release/classes|Out-Null
$files=@(Get-ChildItem src/*.java,"$ToolRoot/serial/usbSerialForAndroid/src/main/java" -Recurse -Filter *.java|% {'"'+$_.FullName.Replace('\','/')+'"'})
$files|Set-Content build/release/sources.txt -Encoding ascii
javac -encoding UTF-8 -source 8 -target 8 -cp "$android;${ToolRoot}/annotation.jar" -d build/release/classes '@build/release/sources.txt'
if($LASTEXITCODE){exit $LASTEXITCODE}
jar cf build/release/app.jar -C build/release/classes .
& "$bt/d8.bat" --min-api 23 --lib "$android" --classpath ${ToolRoot}/annotation.jar --output build/release build/release/app.jar
if($LASTEXITCODE){exit $LASTEXITCODE}
& "$bt/aapt2.exe" compile --dir res -o build/release/res.zip
if($LASTEXITCODE){exit $LASTEXITCODE}
& "$bt/aapt2.exe" link --manifest AndroidManifest.xml -I $android -o build/release/unsigned.apk build/release/res.zip
if($LASTEXITCODE){exit $LASTEXITCODE}
jar uf build/release/unsigned.apk -C build/release classes.dex -C . assets
if($LASTEXITCODE){exit $LASTEXITCODE}
Add-Type -AssemblyName System.IO.Compression.FileSystem
$archive=[IO.Compression.ZipFile]::OpenRead((Resolve-Path build/release/unsigned.apk))
try {
    $base=(Get-Location).Path
    foreach($file in Get-ChildItem assets -Recurse -File) {
        $name=$file.FullName.Substring($base.Length+1).Replace('\','/')
        $entry=$archive.GetEntry($name)
        if(!$entry){throw "Missing APK asset: $name"}
        $reader=$entry.Open()
        try {
            if((Get-FileHash -InputStream $reader).Hash -ne (Get-FileHash $file.FullName).Hash){throw "Invalid APK asset: $name"}
        } finally {$reader.Dispose()}
    }
} finally {$archive.Dispose()}
& "$bt/zipalign.exe" -f -p 4 build/release/unsigned.apk build/release/aligned.apk
if($LASTEXITCODE){exit $LASTEXITCODE}
& "$bt/apksigner.bat" sign --v4-signing-enabled false --ks $KeyStore --ks-pass "file:$PasswordFile" --out build/ota-app-1.2-Android.apk build/release/aligned.apk
if($LASTEXITCODE){exit $LASTEXITCODE}
& "$bt/apksigner.bat" verify --verbose --print-certs build/ota-app-1.2-Android.apk
exit $LASTEXITCODE
}finally{Pop-Location}
