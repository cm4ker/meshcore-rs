$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Windows.Forms, System.Drawing
[Windows.Forms.Application]::EnableVisualStyles()

$exe = Join-Path $PSScriptRoot 'engine\python.exe'
$form = New-Object Windows.Forms.Form
$form.Icon = [Drawing.Icon]::new((Join-Path $PSScriptRoot "ommesh.ico"))
$form.Text = 'Обновление MeshCore'
$form.ClientSize = [Drawing.Size]::new(800, 520)
$form.MinimumSize = [Drawing.Size]::new(816, 559)
$form.StartPosition = 'CenterScreen'
$form.Font = [Drawing.Font]::new('Segoe UI', 10)

function Add-Control($type, $caption, $x, $y, $width, $height) {
    $control = New-Object "Windows.Forms.$type"
    $control.Text = $caption
    $control.SetBounds($x, $y, $width, $height)
    $form.Controls.Add($control)
    return $control
}

$null = Add-Control Label 'Подключение' 20 20 135 25
$method = Add-Control ComboBox '' 160 19 132 28
$method.DropDownStyle = 'DropDownList'
$null = $method.Items.AddRange(@('USB', 'Bluetooth', 'Wi-Fi / TCP'))
$address = Add-Control ComboBox '' 310 19 310 28
$address.DropDownStyle = 'DropDown'
$addressLabel = Add-Control Label '' 310 50 385 23
$find = Add-Control Button 'Найти устройства' 632 17 148 32
$null = Add-Control Label 'PIN Bluetooth' 20 76 135 25
$pin = Add-Control TextBox '' 160 75 132 28
$pin.UseSystemPasswordChar = $true
$pin.MaxLength = 6
$null = Add-Control Label 'При необходимости. PIN показывает устройство.' 310 76 390 25
$null = Add-Control Label 'Файл прошивки' 20 119 135 25
$firmware = Add-Control TextBox '' 160 118 460 28
$firmware.ReadOnly = $true
$browse = Add-Control Button 'Выбрать файл' 632 116 148 32
$info = Add-Control Button 'Версия устройства' 20 165 144 34
$update = Add-Control Button 'Обновить прошивку' 172 165 154 34
$usbFlash = Add-Control Button 'Прошить через USB' 334 165 152 34
$newNode = Add-Control Button 'Новая нода со сбросом…' 494 165 185 34
$help = Add-Control Button 'Как начать' 687 165 93 34
$status = Add-Control Label 'Готово к работе' 20 214 760 26
$log = Add-Control TextBox '' 20 246 760 254
$log.Multiline = $true
$log.ReadOnly = $true
$log.ScrollBars = 'Both'
$log.WordWrap = $false
$log.Anchor = 'Top, Bottom, Left, Right'
$log.Font = [Drawing.Font]::new('Consolas', 10)

$ports = @{ Values = @() }
$tips = New-Object Windows.Forms.ToolTip
$tips.AutoPopDelay = 15000
$tips.SetToolTip($address, 'USB: выберите COM-порт из списка. Bluetooth: выберите найденное устройство. Wi-Fi: введите адрес и порт.')
$tips.SetToolTip($find, 'В режиме USB показывает COM-порты Windows. Для поиска устройств Bluetooth выберите Bluetooth. Для репитера сначала откройте окно обновления на устройстве.')
$tips.SetToolTip($pin, 'Введите шестизначный PIN, который показывает устройство при открытии окна Bluetooth-обновления.')
$tips.SetToolTip($browse, 'Выберите файл .mcfw, .mcfd или .bin для вашей модели.')
$tips.SetToolTip($newNode, 'Прошивка ESP по USB с удалением ключа, настроек, контактов и каналов обеих ролей.')
$tips.SetToolTip($usbFlash, 'Установить выбранный образ приложения через загрузчик ESP. Подходит для установки более раннего билда.')
$tips.SetToolTip($info, 'Читает сведения о подключённом устройстве и его текущей прошивке.')
$tips.SetToolTip($update, 'Запрашивает подтверждение перед записью выбранной прошивки.')

function Get-PortNumber([string]$label) {
    if ($label -match '\((COM[0-9]+)\)$') { return $Matches[1] }
    return $label.Trim()
}

function Get-PortLabels($current) {
    $labels = @{}
    try {
        $devices = Get-CimInstance Win32_PnPEntity -Filter "Name LIKE '%(COM%)'" -ErrorAction Stop
        foreach ($device in $devices) {
            if ($device.Name -notmatch '\((COM[0-9]+)\)$') { continue }
            $port = $Matches[1]
            if ($port -notin $current) { continue }
            $labels[$port] = $device.Name
            if ($device.PNPDeviceID -notlike 'USB\*') { continue }
            try {
                $id = $device.PNPDeviceID
                $description = (Get-PnpDeviceProperty -InstanceId $id -KeyName DEVPKEY_Device_BusReportedDeviceDesc -ErrorAction Stop).Data
                if (!$description -and $id -match '&MI_') {
                    $parentId = (Get-PnpDeviceProperty -InstanceId $id -KeyName DEVPKEY_Device_Parent -ErrorAction Stop).Data
                    $description = (Get-PnpDeviceProperty -InstanceId $parentId -KeyName DEVPKEY_Device_BusReportedDeviceDesc -ErrorAction Stop).Data
                }
                if ($description -is [string] -and $description.Trim()) { $labels[$port] = "$($description.Trim()) ($port)" }
            } catch { }
        }
    } catch { }
    return $labels
}

function Refresh-Ports([switch]$Force) {
    if ($method.SelectedIndex -ne 0 -or $address.DroppedDown) { return }
    try { $current = @([IO.Ports.SerialPort]::GetPortNames() | Sort-Object { [int]($_ -replace '^COM','') }) }
    catch { return }
    if ($Force -or ($current -join ',') -ne ($ports.Values -join ',') -or $address.Items.Count -ne $current.Count) {
        $previous = Get-PortNumber $address.Text
        $labels = Get-PortLabels $current
        $address.Items.Clear()
        foreach ($port in $current) {
            $label = if ($labels[$port]) { $labels[$port] } else { $port }
            $null = $address.Items.Add($label)
        }
        $ports.Values = $current
        $address.Text = ''
        if ($previous -in $current) { $address.SelectedIndex = [array]::IndexOf($current,$previous) }
        elseif ($current.Count -eq 1) { $address.SelectedIndex = 0 }
    }
    $addressLabel.Text = if ($current.Count) { 'Выберите COM-порт устройства' } else { 'COM-порты не найдены. Подключите устройство по USB.' }
}

$address.DropDownWidth = 450
$method.Add_SelectedIndexChanged({
    $usbFlash.Enabled = $method.SelectedIndex -eq 0
    $newNode.Enabled = $usbFlash.Enabled
    $address.Items.Clear()
    $address.Text = ''
    $pin.Clear()
    $pin.Enabled = $method.SelectedIndex -eq 1
    $addressLabel.Text = @('Порт, например COM4', 'Адрес, например AA:BB:CC:DD:EE:FF', 'Адрес и порт, например 192.168.1.50:5000')[$method.SelectedIndex]
    Refresh-Ports
})
$method.SelectedIndex = 0

$browse.Add_Click({
    $dialog = New-Object Windows.Forms.OpenFileDialog
    try {
        $dialog.Title = 'Выберите прошивку для вашей модели MeshCore'
        $dialog.Filter = 'Файлы прошивки (*.mcfw;*.mcfd;*.bin)|*.mcfw;*.mcfd;*.bin'
        if ($dialog.ShowDialog($form) -eq 'OK') { $firmware.Text = $dialog.FileName }
    } finally { $dialog.Dispose() }
})

$state = @{ Process = $null; Output = $null; Errors = $null; Mode = ''; Listing = '' }
$controls = @($method, $address, $pin, $find, $browse, $info, $update, $usbFlash, $newNode)
$timer = New-Object Windows.Forms.Timer
$timer.Interval = 80
$portTimer = New-Object Windows.Forms.Timer
$portTimer.Interval = 1000
$portTimer.Add_Tick({ if (-not $state.Process) { Refresh-Ports } })

function Show-Notice($message) {
    $null = [Windows.Forms.MessageBox]::Show($form, $message, $form.Text)
}

$help.Add_Click({
    Show-Notice @'
USB. Подключите устройство кабелем с передачей данных. Если Windows видит один COM-порт, программа выберет его сама. Если портов несколько, выберите порт вашего устройства.

Bluetooth. Для репитера откройте окно обновления на устройстве. Нажмите «Найти устройства» и выберите адрес. Если требуется сопряжение, введите PIN с устройства.

Wi-Fi. Выберите Wi-Fi / TCP и введите адрес устройства с портом, например 192.168.1.50:5000.


Для создания новой ноды на уже работающей ESP выберите файл и нажмите «Новая нода со сбросом…». После подтверждения старый ключ и данные будут удалены.

Для установки другого билда на ESP выберите USB, COM-порт и файл. Нажмите «Прошить через USB» и подтвердите запись. Для .bin нужен образ приложения для этой платы.

Нажмите «Версия устройства», чтобы проверить подключение. Затем выберите подходящий вашей модели файл .mcfw, .mcfd или .bin и нажмите «Обновить прошивку». Результат появится в журнале.
'@
})

function Quote-Argument([string]$value) {
    return '"' + (($value -replace '(\\*)"', '$1$1\"') -replace '(\\+)$', '$1$1') + '"'
}

function Start-Ota($mode) {
    if ($null -ne $state.Process) { return }
    if ($mode -eq 'List' -and $method.SelectedIndex -eq 0) {
        Refresh-Ports -Force
        $log.Clear()
        foreach ($label in $address.Items) { $log.AppendText($label + [Environment]::NewLine) }
        $status.Text = if ($ports.Values.Count) { "Найдено COM-портов: $($ports.Values.Count)." } else { 'COM-порты не найдены. Подключите устройство по USB.' }
        return
    }
    $scriptName = 'engine\ota_app.py'
    $arguments = @('--list')
    if ($mode -ne 'List') {
        $target = if ($method.SelectedIndex -eq 0) { Get-PortNumber $address.Text } else { $address.Text.Trim() }
        $patterns = @('^COM[0-9]+$', '^(?:[0-9a-f]{2}:){5}[0-9a-f]{2}$', '^(?:[a-z0-9][a-z0-9.-]*|\[[a-f0-9:]+\]):[0-9]{1,5}$')
        if ($target -notmatch $patterns[$method.SelectedIndex]) {
            Show-Notice ('Проверьте адрес подключения. ' + $addressLabel.Text)
            return
        }
        $arguments = @(@('--port', '--ble', '--tcp')[$method.SelectedIndex], $target)
        if ($method.SelectedIndex -eq 1 -and $pin.Text) {
            if ($pin.Text -notmatch '^[0-9]{6}$') { Show-Notice 'Введите шестизначный PIN.'; return }
            $arguments += @('--pin', $pin.Text)
        }
        if ($mode -eq 'Info') { $arguments += '--info' }
        if ($mode -in @('Update','USBFlash','NewNode') -and -not $firmware.Text) {
            $browse.PerformClick()
            if (-not $firmware.Text) { return }
        }
        if ($mode -in @('USBFlash','NewNode')) {
            if ($method.SelectedIndex -ne 0) { return }
            if (-not $firmware.Text -or -not (Test-Path -LiteralPath $firmware.Text -PathType Leaf)) {
                Show-Notice 'Выберите файл выпуска .mcfw или образ приложения .bin для вашей модели ESP.'
                return
            }
            if ([IO.Path]::GetExtension($firmware.Text) -ieq '.mcfd') {
                Show-Notice 'Для дельты нажмите «Обновить прошивку».'
                return
            }
            $fileName = [IO.Path]::GetFileName($firmware.Text)
            $message = "Установить $fileName на устройство $target через загрузчик ESP?`r`n`r`nПроверьте модель платы и роль. Для .bin модель платы по файлу определить нельзя. Разделы с данными сохраняются, но старый билд может иначе читать настройки. Не отключайте USB до окончания."
            if ($mode -eq 'NewNode') {
                $message = "Создать новую ноду на $target с файлом $fileName?`r`n`r`nПрограмма установит выбранный билд и безвозвратно удалит прежний ключ, настройки, контакты, каналы и сопряжения обеих ролей. Проверьте порт и модель. Для сохранения прежней личности нужна резервная копия ключа.`r`n`r`nПродолжить?"
            }
            if ([Windows.Forms.MessageBox]::Show($form,$message,'Прошивка через USB','YesNo','Warning','Button2') -ne 'Yes') { return }
            $scriptName = 'engine\usb_flash.py'
            $arguments = @('--port',$target,$firmware.Text)
            if ($mode -eq 'NewNode') { $arguments += '--new-node' }
        }
        if ($mode -eq 'Update') {
            if ( -not (Test-Path -LiteralPath $firmware.Text -PathType Leaf)) {
                Show-Notice 'Выберите существующий файл прошивки.'
                return
            }
            $message = "Обновить устройство $target файлом $([IO.Path]::GetFileName($firmware.Text))?`r`n`r`nУбедитесь, что прошивка подходит вашей модели. Не отключайте питание во время обновления."
            if ([Windows.Forms.MessageBox]::Show($form, $message, $form.Text, 'YesNo', 'Question', 'Button2') -ne 'Yes') { return }
            $arguments += $firmware.Text
        }
    }

    $start = New-Object Diagnostics.ProcessStartInfo
    $start.FileName = $exe
    $start.Arguments = (@('-X', 'utf8', '-u', (Join-Path $PSScriptRoot $scriptName)) + $arguments | ForEach-Object { Quote-Argument $_ }) -join ' '
    $start.UseShellExecute = $false
    $start.CreateNoWindow = $true
    $start.RedirectStandardOutput = $true
    $start.RedirectStandardError = $true
    $start.StandardOutputEncoding = [Text.Encoding]::UTF8
    $start.StandardErrorEncoding = [Text.Encoding]::UTF8
    $start.EnvironmentVariables['PYTHONIOENCODING'] = 'utf-8'
    $start.EnvironmentVariables['PYTHONUNBUFFERED'] = '1'
    try {
        $state.Process = [Diagnostics.Process]::Start($start)
    } catch {
        Show-Notice ('Не удалось запустить утилиту MeshCore. ' + $_.Exception.Message)
        return
    }
    $state.Mode = $mode
    $state.Listing = ''
    $state.Output = $state.Process.StandardOutput.ReadLineAsync()
    $state.Errors = $state.Process.StandardError.ReadLineAsync()
    $controls | ForEach-Object { $_.Enabled = $false }
    $log.Clear()
    $status.Text = @{ NewNode = 'Прошивка и сброс данных узла…'; USBFlash = 'Установка через USB-загрузчик…'; List = 'Поиск устройств…'; Info = 'Чтение версии…'; Update = 'Идёт обновление…' }[$mode]
    $timer.Start()
}

$timer.Add_Tick({
    foreach ($stream in @('Output', 'Errors')) {
        for ($i = 0; $i -lt 150 -and $state[$stream] -and $state[$stream].IsCompleted; $i++) {
            $line = $state[$stream].GetAwaiter().GetResult()
            if ($null -eq $line) { $state[$stream] = $null; break }
            $log.AppendText($line + [Environment]::NewLine)
            if ($state.Mode -eq 'List' -and $stream -eq 'Output') { $state.Listing += $line + "`n" }
            $reader = if ($stream -eq 'Output') { $state.Process.StandardOutput } else { $state.Process.StandardError }
            $state[$stream] = $reader.ReadLineAsync()
        }
    }
    if ($state.Output -or $state.Errors -or -not $state.Process.HasExited) { return }
    $timer.Stop()
    $exitCode = $state.Process.ExitCode
    $state.Process.Dispose()
    $state.Process = $null
    $controls | ForEach-Object { $_.Enabled = $true }
    $usbFlash.Enabled = $method.SelectedIndex -eq 0
    $newNode.Enabled = $usbFlash.Enabled
    $pin.Enabled = $method.SelectedIndex -eq 1

    if ($exitCode -ne 0) {
        $status.Text = "Операция завершилась с ошибкой (код $exitCode). Подробности в журнале."
    } elseif ($state.Mode -eq 'List') {
        $status.Text = 'Поиск завершён. Адреса устройств указаны в журнале.'
        $pattern = if ($method.SelectedIndex -eq 0) { '\bCOM[0-9]+\b' } elseif ($method.SelectedIndex -eq 1) { '(?:[0-9a-f]{2}:){5}[0-9a-f]{2}' }
        if ($pattern) {
            $matchesFound = @([regex]::Matches($state.Listing, $pattern, 'IgnoreCase') | ForEach-Object Value | Sort-Object -Unique)
            if ($method.SelectedIndex -eq 1) {
                $previous = $address.Text
                $address.Items.Clear()
                $address.Items.AddRange([object[]]$matchesFound)
                $address.Text = $previous
            } else { Refresh-Ports }
            if ($matchesFound.Count -eq 1 -and -not $address.Text) { $address.Text = $matchesFound[0] }
        }
    } else {
        $status.Text = if ($state.Mode -eq 'Info') { 'Версия устройства указана в журнале.' } else { 'Утилита завершила обновление. Результат указан в журнале.' }
    }
})

$newNode.Add_Click({ Start-Ota 'NewNode' })
$usbFlash.Add_Click({ Start-Ota 'USBFlash' })
$find.Add_Click({ Start-Ota 'List' })
$info.Add_Click({ Start-Ota 'Info' })
$update.Add_Click({ Start-Ota 'Update' })
$form.Add_FormClosing({
    param($sender, $eventArgs)
    if ($state.Process) {
        $eventArgs.Cancel = $true
        Show-Notice 'Дождитесь завершения текущей операции.'
    }
})

if (-not (Test-Path -LiteralPath $exe -PathType Leaf)) {
    Show-Notice 'Поместите engine\python.exe в папку рядом с интерфейсом.'
} else {
    $portTimer.Start()
    $null = $form.ShowDialog()
}
$portTimer.Dispose()
$tips.Dispose()
$timer.Dispose()
$form.Dispose()











