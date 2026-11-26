param([switch]$SmokeTest)

$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Windows.Forms, System.Drawing
[Windows.Forms.Application]::EnableVisualStyles()

$appDirectory = if (Test-Path -LiteralPath (Join-Path $PSScriptRoot 'ommesh.ico')) { $PSScriptRoot } else { Split-Path -Parent $PSScriptRoot }
$exe = Join-Path $appDirectory 'engine\python.exe'
$form = New-Object Windows.Forms.Form
$form.Icon = [Drawing.Icon]::new((Join-Path $appDirectory 'ommesh.ico'))
$form.Text = 'ota-app 1.3 · MeshCore'
$form.ClientSize = [Drawing.Size]::new(950, 590)
$form.MinimumSize = [Drawing.Size]::new(966, 629)
$form.StartPosition = 'CenterScreen'
$form.Font = [Drawing.Font]::new('Segoe UI', 10)
$accent = [Drawing.Color]::FromArgb(0, 104, 83)
$accentLight = [Drawing.Color]::FromArgb(223, 239, 233)
$newline = [Environment]::NewLine

function Add-Control($type, $caption, $x, $y, $width, $height, $parent = $form) {
    $control = New-Object "Windows.Forms.$type"
    $control.Text = $caption
    $control.SetBounds($x, $y, $width, $height)
    $parent.Controls.Add($control)
    return $control
}

$null = Add-Control Label 'Подключение' 20 22 130 26
$method = Add-Control ComboBox '' 155 18 239 28
$method.DropDownStyle = 'DropDownList'
$method.Items.AddRange(@('USB','Обновление по Bluetooth','Wi-Fi / TCP','Загрузчик nRF52 (UF2)'))
$address = Add-Control ComboBox '' 406 18 346 28
$address.DropDownWidth = 470
$address.Anchor = 'Top, Left, Right'
$find = Add-Control Button 'Найти устройства' 764 16 166 32
$find.Anchor = 'Top, Right'
$addressLabel = Add-Control Label '' 406 50 348 26
$addressLabel.Anchor = 'Top, Left, Right'
$help = Add-Control Button 'ИНСТРУКЦИЯ' 764 51 166 32
$help.Anchor = 'Top, Right'
$pinLabel = Add-Control Label 'PIN Bluetooth' 20 78 130 26
$pin = Add-Control TextBox '' 155 75 90 28
$pin.UseSystemPasswordChar = $true
$pin.MaxLength = 6
$modelLabel = Add-Control Label 'Модель nRF52' 20 78 130 26
$model = Add-Control ComboBox '' 155 75 239 28
$model.DropDownStyle = 'DropDownList'
$boards = [ordered]@{
    'Выберите модель' = ''
    'Heltec T096' = 'heltec-t096'
    'Heltec T114' = 'heltec-t114'
    'Pro Micro (FakeTec)' = 'promicro'
    'LilyGO T-Echo' = 't-echo'
}
$model.Items.AddRange([object[]]@($boards.Keys))
$model.SelectedIndex = 0
$diskButton = Add-Control Button 'USB-диск UF2…' 155 74 239 31
$info = Add-Control Button 'Проверить устройство' 764 86 166 32
$info.Anchor = 'Top, Right'
$deviceDetails = Add-Control Label 'Подключите устройство и проверьте его.' 20 121 910 41
$deviceDetails.Anchor = 'Top, Left, Right'
$deviceDetails.BackColor = $accentLight

$null = Add-Control Label 'Файл прошивки' 20 175 130 25
$firmware = Add-Control TextBox '' 155 172 512 27
$firmware.Anchor = 'Top, Left, Right'
$firmware.ReadOnly = $true
$clearFile = Add-Control Button '×' 675 171 34 30
$clearFile.Anchor = 'Top, Right'
$clearFile.AccessibleName = 'Снять выбор файла'
$browse = Add-Control Button 'Выбрать файл' 717 170 109 32
$browse.Anchor = 'Top, Right'
$catalog = Add-Control Button 'Из GitHub' 834 170 96 32
$catalog.Anchor = 'Top, Right'
$update = Add-Control Button 'Обновить прошивку' 20 215 182 34
$usbFlash = Add-Control Button 'Прошить через USB' 211 215 161 34
$newNode = Add-Control Button 'Новая нода со сбросом…' 381 215 209 34
$exportDfu = Add-Control Button 'Сохранить DFU ZIP' 211 215 171 34

$progress = Add-Control ProgressBar '' 20 268 910 4
$progress.Anchor = 'Top, Left, Right'
$progress.Style = 'Marquee'
$progress.MarqueeAnimationSpeed = 25
$progress.Visible = $false
$journalHeading = Add-Control Label 'Журнал' 20 283 90 24
$journalHeading.Font = [Drawing.Font]::new('Segoe UI Semibold', 10)
$journalToggle = Add-Control Button 'Свернуть' 810 278 120 30
$journalToggle.Anchor = 'Top, Right'
$status = Add-Control Label 'Готово к работе' 115 283 688 24
$status.Anchor = 'Top, Left, Right'
$status.AutoEllipsis = $true
$log = Add-Control TextBox '' 20 317 910 253
$log.Multiline = $true
$log.ReadOnly = $true
$log.ScrollBars = 'Both'
$log.WordWrap = $false
$log.Anchor = 'Top, Bottom, Left, Right'
$log.Font = [Drawing.Font]::new('Consolas', 10)

$ports = @{ Values = @() }
$state = @{
    Process = $null; Output = $null; Errors = $null; Mode = ''; Text = ''
    Drive = $null; PendingRoot = ''; DfuDevices = @{}; DeviceKey = ''; Device = $null
    JournalOpen = $true; JournalHeight = 590
}
$controls = @($method,$address,$pin,$model,$find,$browse,$info,$update,$usbFlash,$newNode,$diskButton,$exportDfu,$catalog,$clearFile)
$tips = New-Object Windows.Forms.ToolTip
$tips.AutoPopDelay = 15000
$tips.SetToolTip($pin, 'Введите шестизначный PIN, который показывает устройство.')
$tips.SetToolTip($newNode, 'Прошивка ESP по USB с удалением ключа, настроек, контактов и каналов обеих ролей.')
$tips.SetToolTip($usbFlash, 'Установить образ приложения через загрузчик ESP.')
$tips.SetToolTip($info, 'Читает модель устройства и установленную сборку.')
$tips.SetToolTip($catalog, 'Показывает выпуски для модели и роли устройства.')

function Get-Formats {
    switch ($method.SelectedIndex) {
        0 { '.mcfw'; '.mcfd'; '.bin'; '.uf2' }
        1 { '.mcfw'; '.mcfd'; '.bin' }
        2 { '.mcfw'; '.mcfd' }
        3 { '.uf2' }
    }
}

function Get-FirmwareExtension {
    return [IO.Path]::GetExtension($firmware.Text).ToLowerInvariant()
}

function Uses-Uf2Drive {
    return $method.SelectedIndex -eq 0 -and ($null -ne $state.Drive -or (Get-FirmwareExtension) -eq '.uf2')
}

function Get-Target {
    $value = $address.Text.Trim()
    switch ($method.SelectedIndex) {
        0 { return (Get-PortNumber $value) }
        2 {
            if ($value -match '^(?:[a-z0-9][a-z0-9.-]*|\[[a-f0-9:]+\])$') { return "$($value):5000" }
            return $value
        }
        default {
            if ($value -match '\(((?:[0-9a-f]{2}:){5}[0-9a-f]{2})\)$') { return $Matches[1] }
            return $value
        }
    }
}

function Get-ContextKey { return "$($method.SelectedIndex)|$(Get-Target)" }

function Get-Board {
    if ($method.SelectedIndex -eq 3) {
        $detected = $state.DfuDevices[(Get-Target)]
        if ($detected -and $detected.board) { return $detected.board }
    } elseif ($state.Drive -and $state.Drive.board) { return $state.Drive.board }
    return $boards[$model.Text]
}

function Set-Controls {
    $busy = $null -ne $state.Process
    foreach ($control in $controls) { $control.Enabled = -not $busy }
    $usb = $method.SelectedIndex -eq 0
    $dfu = $method.SelectedIndex -eq 3
    $drive = Uses-Uf2Drive
    $address.Enabled = -not $busy -and -not ($drive -and $state.Drive)
    $pinLabel.Visible = $method.SelectedIndex -eq 1
    $pin.Visible = $pinLabel.Visible
    $diskButton.Visible = $usb -and (-not $drive -or $null -ne $state.Drive -and $state.Drive.board)
    $model.Visible = $dfu -or ($drive -and $state.Drive -and -not $state.Drive.board)
    $modelLabel.Visible = $model.Visible
    $known = if ($dfu) { $state.DfuDevices[(Get-Target)] } else { $null }
    if ($known -and $known.board -in $boards.Values) {
        $model.SelectedIndex = [array]::IndexOf(@($boards.Values),$known.board)
        $model.Enabled = $false
    }
    $find.Enabled = -not $busy -and $method.SelectedIndex -ne 2
    $find.Text = if ($dfu) { 'Найти загрузчик' } else { 'Найти устройства' }
    $usbFlash.Visible = $usb -and -not $drive
    $newNode.Visible = $usbFlash.Visible
    $usbFlash.Enabled = -not $busy -and (Get-FirmwareExtension) -ne '.mcfd'
    $newNode.Enabled = $usbFlash.Enabled
    $info.Visible = -not $dfu
    $exportDfu.Visible = $dfu
    $exportDfu.Enabled = -not $busy -and (Get-FirmwareExtension) -eq '.uf2'
    $update.Text = if ($dfu) { 'Начать' } elseif ($drive) { 'Прошить UF2 по USB' } else { 'Обновить прошивку' }
    $clearFile.Enabled = -not $busy -and [bool]$firmware.Text
    $progress.Visible = $busy
    if ($drive -and $state.Drive) {
        $deviceDetails.Text = (($state.Drive.model, "UF2-загрузчик $($state.Drive.version) · $($state.Drive.softdevice)") -join $newline)
    } elseif ($dfu -and $known) {
        $deviceDetails.Text = if ($known.board) { "Загрузчик распознан · $($model.Text)" } else { 'Загрузчик обнаружен. Выберите модель платы.' }
    } elseif ($state.Device -and $state.DeviceKey -eq (Get-ContextKey)) {
        $deviceDetails.Text = $state.Device.summary
    } else {
        $deviceDetails.Text = if ($dfu) { 'Переведите плату в BLE OTA и запустите поиск.' } else { 'Подключите устройство и проверьте его.' }
    }
    $formats = @(Get-Formats)
    $tips.SetToolTip($browse,'Поддерживаемые файлы — ' + ($formats -join ', ') + '.')
}

function Show-Notice($message) { $null = [Windows.Forms.MessageBox]::Show($form,$message,$form.Text) }

function Show-DetailsDialog([string]$title,[string]$message) {
    $dialog = New-Object Windows.Forms.Form
    try {
        $dialog.Text = $title
        $dialog.Font = $form.Font
        $dialog.Icon = $form.Icon
        $dialog.StartPosition = 'CenterParent'
        $dialog.ClientSize = [Drawing.Size]::new(690,405)
        $dialog.FormBorderStyle = 'FixedDialog'
        $dialog.MinimizeBox = $false
        $dialog.MaximizeBox = $false
        $content = Add-Control TextBox $message 18 16 654 333 $dialog
        $content.Multiline = $true
        $content.ReadOnly = $true
        $content.ScrollBars = 'Vertical'
        $content.Text = ($message -replace '\r?\n',$newline)
        $content.SelectionLength = 0
        $close = Add-Control Button 'Закрыть' 552 361 120 30 $dialog
        $close.DialogResult = 'Cancel'
        $dialog.CancelButton = $close
        $dialog.ActiveControl = $close
        $null = $dialog.ShowDialog($form)
    } finally { $dialog.Dispose() }
}

$help.Add_Click({
    $modeIntro = switch ($method.SelectedIndex) {
        0 { 'Подключите плату кабелем с передачей данных. Выберите COM-порт. Если порт один, программа подставит его сама.', '', 'Для установки .uf2 переведите плату в USB-загрузчик. Нажмите «USB-диск UF2…» и выберите диск платы. Выберите .uf2 для её модели и роли, затем нажмите «Прошить UF2 по USB».', '', '«Прошить через USB» записывает полный образ на поддерживаемую ESP. «Новая нода со сбросом…» дополнительно удаляет ключ, настройки, каналы и контакты.' }
        1 { 'Включите Bluetooth и откройте окно обновления на плате. Для репитера MeshCore-RS его можно открыть командой fw ble в консоли.', '', 'Нажмите «Найти устройства» и выберите плату. Если устройство показывает PIN, введите его.' }
        2 { 'Подключите компьютер и плату к одной сети. Введите адрес устройства. Если порт отличается от 5000, добавьте его через двоеточие.', '', 'Для Wi-Fi / TCP нужен подписанный .mcfw или .mcfd.' }
        3 { 'Переведите плату в BLE OTA и нажмите «Найти загрузчик». Если модель не распозналась, укажите её вручную.', '', 'Нажмите «Из GitHub» и выберите роль или откройте локальный .uf2 для вашей платы. Нажмите «Начать» и подтвердите запись. «Сохранить DFU ZIP» сохраняет пакет для отдельного прошивальщика.' }
    }
    $steps = @($modeIntro)
    if ($method.SelectedIndex -ne 3 -and -not (Uses-Uf2Drive)) {
        $steps += '', '1. Нажмите «Проверить устройство». Появятся модель, версия и номер сборки.', '2. Нажмите «Из GitHub» для подбора файла или «Выбрать файл». Дельта .mcfd должна подходить к установленной сборке.', '3. Нажмите «Обновить прошивку», сверьте файл и запустите запись. После перезапуска снова проверьте устройство.'
    }
    $steps += '', 'Кнопка «Развернуть» показывает все записи журнала. Последняя запись видна и в свёрнутом виде.'
    Show-DetailsDialog $method.Text ($steps -join $newline)
})

function Switch-Journal {
    if ($state.JournalOpen) {
        $state.JournalHeight = $form.ClientSize.Height
        $log.Visible = $false
        $form.MinimumSize = [Drawing.Size]::new(966,359)
        $form.ClientSize = [Drawing.Size]::new($form.ClientSize.Width,324)
        $journalToggle.Text = 'Развернуть'
    } else {
        $form.MinimumSize = [Drawing.Size]::new(966,629)
        $form.ClientSize = [Drawing.Size]::new($form.ClientSize.Width,$state.JournalHeight)
        $log.Visible = $true
        $journalToggle.Text = 'Свернуть'
    }
    $state.JournalOpen = -not $state.JournalOpen
}
$journalToggle.Add_Click({ Switch-Journal })

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


function Quote-Argument([string]$value) {
    return '"' + (($value -replace '(\\*)"', '$1$1\"') -replace '(\\+)$', '$1$1') + '"'
}



function Refresh-Ports([switch]$Force) {
    if ($method.SelectedIndex -ne 0 -or $address.DroppedDown -or (Uses-Uf2Drive)) { return }
    try { $current = @([IO.Ports.SerialPort]::GetPortNames() | Sort-Object { [int]($_ -replace '^COM','') }) }
    catch { return }
    if ($Force -or ($current -join ',') -ne ($ports.Values -join ',') -or $address.Items.Count -ne $current.Count) {
        $previous = Get-PortNumber $address.Text
        $labels = Get-PortLabels $current
        $address.Items.Clear()
        foreach ($port in $current) {
            $null = $address.Items.Add($(if ($labels[$port]) { $labels[$port] } else { $port }))
        }
        $ports.Values = $current
        $address.Text = ''
        if ($previous -in $current) { $address.SelectedIndex = [array]::IndexOf($current,$previous) }
        elseif ($current.Count -eq 1) { $address.SelectedIndex = 0 }
    }
    $addressLabel.Text = if ($current.Count) { 'Выберите COM-порт устройства' } else { 'Подключите плату по USB или выберите диск UF2.' }
}

function Set-Firmware([string]$path) {
    if ($path -and [IO.Path]::GetExtension($path).ToLowerInvariant() -notin @(Get-Formats)) {
        Show-Notice ('Допустимые форматы — ' + ((Get-Formats) -join ', ') + '.')
        return
    }
    if ($path -and [IO.Path]::GetExtension($path) -ine '.uf2' -and $state.Drive) {
        $state.Drive = $null
        $address.Text = ''
    }
    $firmware.Text = $path
    Set-Controls
    Refresh-Ports
}

$method.Add_SelectedIndexChanged({
    $state.Drive = $null
    $state.Device = $null
    $state.DfuDevices = @{}
    $model.SelectedIndex = 0
    $address.Items.Clear()
    $address.Text = ''
    $pin.Clear()
    $addressLabel.Text = @('Выберите COM-порт или USB-диск UF2','Выберите устройство или введите адрес BLE','Адрес платы, например 192.168.1.50:5000','Переведите плату в BLE OTA и найдите загрузчик')[$method.SelectedIndex]
    $extension = Get-FirmwareExtension
    if ($extension -and $extension -notin @(Get-Formats)) { $firmware.Clear() }
    Set-Controls
    Refresh-Ports
})
$address.Add_TextChanged({ if ($method.SelectedIndex -ge 0) { Set-Controls } })
$method.SelectedIndex = 0

$browse.Add_Click({
    $dialog = New-Object Windows.Forms.OpenFileDialog
    try {
        $dialog.Title = 'Выберите прошивку для вашей модели MeshCore'
        $pattern = ((Get-Formats) | ForEach-Object { '*' + $_ }) -join ';'
        $dialog.Filter = "Файлы прошивки ($pattern)|$pattern"
        if ($dialog.ShowDialog($form) -eq 'OK') { Set-Firmware $dialog.FileName }
    } finally { $dialog.Dispose() }
})
$clearFile.Add_Click({
    if ((Get-FirmwareExtension) -eq '.uf2' -and $state.Drive) {
        $state.Drive = $null
        $address.Text = ''
    }
    Set-Firmware ''
    $status.Text = 'Выбор файла снят.'
})

function Select-DfuRole {
    $dialog = New-Object Windows.Forms.Form
    try {
        $dialog.Text = 'Роль прошивки UF2'
        $dialog.Font = $form.Font
        $dialog.Icon = $form.Icon
        $dialog.StartPosition = 'CenterParent'
        $dialog.ClientSize = [Drawing.Size]::new(375,115)
        $dialog.FormBorderStyle = 'FixedDialog'
        $dialog.MinimizeBox = $false
        $dialog.MaximizeBox = $false
        $null = Add-Control Label "Выберите роль для платы $($model.Text)" 14 16 346 28 $dialog
        $choice = Add-Control Button 'Компаньон' 14 56 164 34 $dialog
        $choice.DialogResult = 'Yes'
        $choice = Add-Control Button 'Репитер' 191 56 164 34 $dialog
        $choice.DialogResult = 'No'
        $result = $dialog.ShowDialog($form)
        if ($result -eq 'Yes') { return 'companion' }
        if ($result -eq 'No') { return 'repeater' }
    } finally { $dialog.Dispose() }
    return ''
}

function Get-AssetDetails($asset) {
    $kind = switch ($asset.format) {
        '.mcfd' { "Дельта с №$($asset.base)" }
        '.uf2' { 'Образ для загрузчика' }
        default { 'Полный образ' }
    }
    $size = if ($asset.size -lt 1024) { "$($asset.size) Б" } else { "$([Math]::Round($asset.size/1024)) КиБ" }
    $details = "$kind · $($asset.format) · $size"
    if ($asset.prerelease) { $details += ' · Предварительный выпуск' }
    if ($asset.issue) { $details += $newline + $asset.issue }
    return $details
}

function Show-Catalog($data) {
    $items = @($data.assets | Where-Object { $null -ne $_ })
    $dialog = New-Object Windows.Forms.Form
    try {
        $dialog.Text = 'Прошивки из GitHub'
        $dialog.Icon = $form.Icon
        $dialog.Font = $form.Font
        $dialog.StartPosition = 'CenterParent'
        $dialog.FormBorderStyle = 'FixedDialog'
        $dialog.ClientSize = [Drawing.Size]::new(680,248)
        $dialog.MinimizeBox = $false
        $dialog.MaximizeBox = $false
        $summary = if ($data.device.summary) { $data.device.summary } else { "Загрузчик UF2 · $($model.Text)" }
        $null = Add-Control Label $summary 18 16 644 48 $dialog
        $pick = @{ Asset = $null; Open = $false }
        $recommended = $null
        if ($null -ne $data.recommended -and [int]$data.recommended -ge 0 -and [int]$data.recommended -lt $items.Count) {
            $recommended = $items[[int]$data.recommended]
        }
        $card = Add-Control Label '' 18 68 644 77 $dialog
        if ($recommended) {
            $card.Text = "Рекомендуем · $($recommended.tag)" + $newline + (Get-AssetDetails $recommended)
            $card.BackColor = $accentLight
            $card.ForeColor = $accent
            $pick.Asset = $recommended
        } elseif (!$items.Count) {
            $card.Text = 'В каталоге нет файлов для этой модели и роли.'
        } elseif ($data.device.build -and -not ($items | Where-Object { $_.build -eq 0 -or $_.build -gt $data.device.build })) {
            $card.Text = "В каталоге нет сборки новее №$($data.device.build)."
        } else {
            $card.Text = 'Подходящего обновления нет. Откройте другие файлы, чтобы посмотреть причины.'
        }
        $others = @($items | Where-Object { $_ -ne $recommended })
        $more = Add-Control Button "Другие файлы ($($others.Count))" 18 151 188 32 $dialog
        $more.Enabled = [bool]$others.Count
        $primary = Add-Control Button 'Скачать' 226 151 170 32 $dialog
        $primary.Enabled = $null -ne $recommended
        $primary.DialogResult = 'OK'
        $close = Add-Control Button 'Закрыть' 542 198 120 32 $dialog
        $close.DialogResult = 'Cancel'
        $dialog.CancelButton = $close
        $list = Add-Control ListBox '' 18 195 644 196 $dialog
        $list.Visible = $false
        $detail = Add-Control Label '' 18 399 644 58 $dialog
        $detail.Visible = $false
        foreach ($asset in $others) {
            $badge = if ($asset.issue) { ' · архив' } elseif ($asset.prerelease) { ' · предварительный' } else { '' }
            $null = $list.Items.Add("$($asset.tag) · $($asset.format)$badge")
        }
        $list.Add_SelectedIndexChanged({
            if ($list.SelectedIndex -ge 0) {
                $pick.Asset = $others[$list.SelectedIndex]
                $detail.Text = Get-AssetDetails $pick.Asset
                $primary.Enabled = $true
            }
        })
        $more.Add_Click({
            $pick.Open = -not $pick.Open
            $list.Visible = $pick.Open
            $detail.Visible = $pick.Open
            if ($pick.Open) {
                $dialog.ClientSize = [Drawing.Size]::new(680,502)
                $close.Top = 454
                $more.Text = 'Скрыть другие файлы'
                if ($list.SelectedIndex -lt 0 -and $list.Items.Count) { $list.SelectedIndex = 0 }
                $pick.Asset = $others[$list.SelectedIndex]
                $primary.Enabled = $null -ne $pick.Asset
            } else {
                $dialog.ClientSize = [Drawing.Size]::new(680,248)
                $close.Top = 198
                $more.Text = "Другие файлы ($($others.Count))"
                $pick.Asset = $recommended
                $primary.Enabled = $null -ne $recommended
            }
        })
        if ($dialog.ShowDialog($form) -eq 'OK' -and $pick.Asset) {
            return $pick.Asset
        }
    } finally { $dialog.Dispose() }
    return $null
}

function Select-Uf2Disk {
    if ($state.Process) { return }
    $dialog = New-Object Windows.Forms.FolderBrowserDialog
    try {
        $dialog.Description = 'Выберите сам диск загрузчика UF2, например E:\'
        $dialog.ShowNewFolderButton = $false
        if ($dialog.ShowDialog($form) -ne 'OK') { return }
        $state.PendingRoot = $dialog.SelectedPath
        Start-Engine 'Uf2Info' 'nrf_flash.py' @('inspect',$state.PendingRoot)
    } finally { $dialog.Dispose() }
}

function Start-Engine([string]$mode,[string]$scriptName,[string[]]$arguments) {
    $start = New-Object Diagnostics.ProcessStartInfo
    $start.FileName = $exe
    $start.Arguments = (@('-X','utf8','-u',(Join-Path $appDirectory "engine\$scriptName")) + $arguments | ForEach-Object { Quote-Argument $_ }) -join ' '
    $start.UseShellExecute = $false
    $start.CreateNoWindow = $true
    $start.RedirectStandardOutput = $true
    $start.RedirectStandardError = $true
    $start.StandardOutputEncoding = [Text.Encoding]::UTF8
    $start.StandardErrorEncoding = [Text.Encoding]::UTF8
    $start.EnvironmentVariables['PYTHONIOENCODING'] = 'utf-8'
    $start.EnvironmentVariables['PYTHONUNBUFFERED'] = '1'
    try { $state.Process = [Diagnostics.Process]::Start($start) }
    catch { Show-Notice ('Не удалось запустить утилиту MeshCore. ' + $_.Exception.Message); return }
    $state.Mode = $mode
    $state.Text = ''
    $state.Output = $state.Process.StandardOutput.ReadLineAsync()
    $state.Errors = $state.Process.StandardError.ReadLineAsync()
    $log.Clear()
    $status.Text = 'Выполняется операция…'
    Set-Controls
    $timer.Start()
}

function Get-DownloadDirectory {
    $directory = Join-Path ([Environment]::GetFolderPath('LocalApplicationData')) 'ota-app\Firmware'
    $null = [IO.Directory]::CreateDirectory($directory)
    return $directory
}

function Start-Catalog($arguments) {
    if ($method.SelectedIndex -eq 3 -or (Uses-Uf2Drive)) {
        if ($method.SelectedIndex -eq 0 -and -not $state.Drive) { Select-Uf2Disk; return }
        $board = Get-Board
        if (!$board) { Show-Notice 'Выберите точную модель платы.'; return }
        $role = Select-DfuRole
        if ($role) { Start-Engine 'Catalog' 'ota_app.py' @('--catalog','--uf2-board',$board,'--uf2-role',$role) }
    } else {
        Start-Engine 'Catalog' 'ota_app.py' ($arguments + '--catalog')
    }
}

function Confirm-Flash([string]$text,[switch]$Warning) {
    $icon = if ($Warning) { 'Warning' } else { 'Question' }
    return [Windows.Forms.MessageBox]::Show($form,$text,$form.Text,'YesNo',$icon,'Button2') -eq 'Yes'
}

function Start-Ota([string]$mode) {
    if ($state.Process) { return }
    $transport = $method.SelectedIndex
    if ($mode -eq 'List') {
        switch ($transport) {
            0 {
                if (Uses-Uf2Drive) { $state.Drive = $null; $address.Text = '' }
                if ((Get-FirmwareExtension) -eq '.uf2') { Set-Firmware '' }
                Refresh-Ports -Force
                $log.Lines = [string[]]@($address.Items)
                $status.Text = "Найдено COM-портов: $($ports.Values.Count)."
            }
            1 { Start-Engine 'List' 'ota_app.py' @('--list') }
            3 { Start-Engine 'DfuList' 'nrf_flash.py' @('scan') }
        }
        return
    }
    $target = Get-Target
    $drive = Uses-Uf2Drive
    if ($mode -eq 'Info' -and $drive) { Select-Uf2Disk; return }
    if ($mode -eq 'Catalog' -and ($transport -eq 3 -or $drive)) { Start-Catalog; return }
    $flashing = $mode -in @('Update','USBFlash','NewNode','Export')
    if ($flashing) {
        if (!$firmware.Text) { $browse.PerformClick() }
        if (!$firmware.Text) { return }
        $extension = Get-FirmwareExtension
        if ($extension -notin @(Get-Formats) -or -not (Test-Path -LiteralPath $firmware.Text -PathType Leaf)) {
            Show-Notice 'Выберите существующий файл поддерживаемого формата.'
            return
        }
        $fileName = [IO.Path]::GetFileName($firmware.Text)
    }
    if ($mode -eq 'Export') {
        $dialog = New-Object Windows.Forms.SaveFileDialog
        try {
            $dialog.Filter = 'Пакет DFU (*.zip)|*.zip'
            $dialog.FileName = [IO.Path]::GetFileNameWithoutExtension($fileName)+'-dfu.zip'
            if ($dialog.ShowDialog($form) -eq 'OK') {
                Start-Engine 'Export' 'nrf_flash.py' @('export',$firmware.Text,$dialog.FileName)
            }
        } finally { $dialog.Dispose() }
        return
    }
    if ($mode -eq 'Update' -and $extension -eq '.uf2') {
        if ($transport -eq 0 -and -not $state.Drive) { Select-Uf2Disk; return }
        $board = Get-Board
        if (!$board) { Show-Notice 'Выберите точную модель платы.'; return }
        if ($transport -eq 3 -and $target -notmatch '^(?:[0-9a-f]{2}:){5}[0-9a-f]{2}$') {
            Show-Notice 'Найдите и выберите загрузчик.'
            return
        }
        $destination = if ($transport -eq 3) { $target } else { $state.Drive.root }
        if (-not (Confirm-Flash ("Установить $fileName на $($destination)?" + $newline + $newline + "Сверьте модель и роль файла."))) { return }
        if ($transport -eq 3) {
            Start-Engine 'DfuWrite' 'nrf_flash.py' @('dfu',$target,$firmware.Text,$board)
        } else {
            Start-Engine 'Uf2Write' 'nrf_flash.py' @('write',$firmware.Text,$destination,'--board',$board)
        }
        return
    }
    $patterns = @('^COM[0-9]+$','^(?:[0-9a-f]{2}:){5}[0-9a-f]{2}$','^(?:[a-z0-9][a-z0-9.-]*|\[[a-f0-9:]+\]):[0-9]{1,5}$')
    if ($transport -gt 2 -or $target -notmatch $patterns[$transport]) {
        Show-Notice ('Проверьте адрес подключения. ' + $addressLabel.Text)
        return
    }
    if ($mode -in @('USBFlash','NewNode')) {
        if ($transport -ne 0 -or $extension -notin @('.mcfw','.bin')) { Show-Notice 'Для загрузчика ESP выберите полный образ .mcfw или .bin.'; return }
        $message = "Установить $fileName на устройство $target через загрузчик ESP?" + $newline + $newline + 'Сверьте модель и роль. Оставьте USB подключённым до конца записи.'
        $arguments = @('--port',$target,$firmware.Text)
        if ($mode -eq 'NewNode') {
            $message += $newline + $newline + 'Будут удалены прежний ключ, настройки, контакты, каналы и сопряжения обеих ролей.'
            $arguments += '--new-node'
        }
        if (Confirm-Flash $message -Warning) { Start-Engine $mode 'usb_flash.py' $arguments }
        return
    }
    $arguments = @(@('--port','--ble','--tcp')[$transport],$target)
    if ($transport -eq 1 -and $pin.Text) {
        if ($pin.Text -notmatch '^[0-9]{6}$') { Show-Notice 'Введите шестизначный PIN.'; return }
        $arguments += @('--pin',$pin.Text)
    }
    switch ($mode) {
        'Info' { Start-Engine 'Info' 'ota_app.py' ($arguments + '--device-json') }
        'Catalog' { Start-Catalog $arguments }
        'Update' {
            if (Confirm-Flash ("Обновить устройство $target файлом $fileName?" + $newline + $newline + 'Оставьте плату подключённой до окончания записи.')) {
                Start-Engine 'Update' 'ota_app.py' ($arguments + $firmware.Text)
            }
        }
    }
}


function Finish-Engine([int]$exitCode) {
    if ($exitCode -ne 0) {
        $status.Text = "Ошибка операции (код $exitCode). Подробности в журнале."
        return
    }
    switch ($state.Mode) {
        'Info' {
            $state.Device = ConvertFrom-Json $state.Text
            $state.DeviceKey = Get-ContextKey
            $log.AppendText(($state.Device.summary -replace '\r?\n',$newline) + $newline)
            $status.Text = 'Сведения об устройстве прочитаны.'
        }
        'Catalog' {
            $data = ConvertFrom-Json $state.Text
            if ($data.device.summary) {
                $state.Device = $data.device
                $state.DeviceKey = Get-ContextKey
            }
            $status.Text = 'Каталог выпусков загружен.'
            Set-Controls
            $asset = Show-Catalog $data
            if ($asset) {
                $encoded = [Convert]::ToBase64String([Text.Encoding]::UTF8.GetBytes((ConvertTo-Json -InputObject $asset -Depth 6 -Compress)))
                Start-Engine 'Download' 'ota_app.py' @('--download-release',$encoded,'--download-dir',(Get-DownloadDirectory))
            }
        }
        'Download' {
            $data = ConvertFrom-Json $state.Text
            Set-Firmware $data.path
            $status.Text = 'Файл GitHub загружен и выбран.'
            $log.AppendText($status.Text + $newline)
        }
        'Uf2Info' {
            $state.Drive = ConvertFrom-Json $state.Text
            if ($state.Drive.PSObject.Properties['root']) { $state.Drive.root = $state.PendingRoot }
            else { $state.Drive | Add-Member -NotePropertyName root -NotePropertyValue $state.PendingRoot }
            if ($state.Drive.board -in $boards.Values) { $model.SelectedIndex = [array]::IndexOf(@($boards.Values),$state.Drive.board) }
            $address.Items.Clear()
            $address.Text = $state.Drive.root
            $addressLabel.Text = 'Выбран диск загрузчика UF2.'
            if ((Get-FirmwareExtension) -ne '.uf2') { $firmware.Clear() }
            $status.Text = 'Диск UF2 проверен.'
            $log.AppendText("$($state.Drive.model) · UF2 $($state.Drive.version)" + $newline)
        }
        'DfuList' {
            $state.DfuDevices = @{}
            $address.Items.Clear()
            $address.Text = ''
            $model.SelectedIndex = 0
            foreach ($device in (ConvertFrom-Json $state.Text)) {
                if (!$device -or !$device.address) { continue }
                $state.DfuDevices[$device.address] = $device
                $label = "$($device.name) ($($device.address))"
                $null = $address.Items.Add($label)
                $log.AppendText($label + $newline)
            }
            if ($address.Items.Count -eq 1) { $address.SelectedIndex = 0 }
            $status.Text = "Найдено загрузчиков: $($address.Items.Count)."
        }
        'List' {
            $address.Items.Clear()
            foreach ($item in [regex]::Matches($state.Text,'(?im)^\s*((?:[0-9a-f]{2}:){5}[0-9a-f]{2})\s+(.+?)\s+RSSI\s+[-0-9]+\s*$')) {
                $null = $address.Items.Add("$($item.Groups[2].Value) ($($item.Groups[1].Value))")
            }
            if ($address.Items.Count -eq 1) { $address.SelectedIndex = 0 }
            $status.Text = "Найдено устройств Bluetooth: $($address.Items.Count)."
        }
        'Export' { $status.Text = 'DFU-пакет сохранён.' }
        default {
            $state.Device = $null
            if ($state.Mode -eq 'Uf2Write') { $state.Drive = $null; $address.Text = '' }
            Set-Firmware ''
            $status.Text = 'Утилита завершила запись. Дождитесь перезагрузки платы.'
        }
    }
}

$timer = New-Object Windows.Forms.Timer
$timer.Interval = 80
$portTimer = New-Object Windows.Forms.Timer
$portTimer.Interval = 1000
$portTimer.Add_Tick({ if (-not $state.Process) { Refresh-Ports } })
$timer.Add_Tick({
    foreach ($stream in @('Output','Errors')) {
        for ($i = 0; $i -lt 150 -and $state[$stream] -and $state[$stream].IsCompleted; $i++) {
            $line = $state[$stream].GetAwaiter().GetResult()
            if ($null -eq $line) { $state[$stream] = $null; break }
            if ($stream -eq 'Output' -and $state.Mode -in @('Info','Catalog','Download','Uf2Info','DfuList','List')) {
                $state.Text += $line + $newline
            }
            if ($stream -eq 'Errors' -or $state.Mode -notin @('Info','Catalog','Download','Uf2Info','DfuList')) {
                $log.AppendText($line + $newline)
                if ($line.Trim()) { $status.Text = $line.Trim() }
            }
            $reader = if ($stream -eq 'Output') { $state.Process.StandardOutput } else { $state.Process.StandardError }
            $state[$stream] = $reader.ReadLineAsync()
        }
    }
    if ($state.Output -or $state.Errors -or -not $state.Process.HasExited) { return }
    $timer.Stop()
    $exitCode = $state.Process.ExitCode
    $state.Process.Dispose()
    $state.Process = $null
    try { Finish-Engine $exitCode }
    catch {
        $log.AppendText($_.Exception.Message + $newline)
        $status.Text = 'Ошибка обработки результата. Подробности в журнале.'
    }
    Set-Controls
})

$newNode.Add_Click({ Start-Ota 'NewNode' })
$usbFlash.Add_Click({ Start-Ota 'USBFlash' })
$find.Add_Click({ Start-Ota 'List' })
$info.Add_Click({ Start-Ota 'Info' })
$update.Add_Click({ Start-Ota 'Update' })
$catalog.Add_Click({ Start-Ota 'Catalog' })
$exportDfu.Add_Click({ Start-Ota 'Export' })
$diskButton.Add_Click({ Select-Uf2Disk })
$form.Add_FormClosing({
    param($sender,$eventArgs)
    if ($state.Process) {
        $eventArgs.Cancel = $true
        Show-Notice 'Дождитесь завершения текущей операции.'
    }
})

try {
    if ($SmokeTest) {
        if ($method.Items.Count -ne 4 -or $form.Icon -eq $null) { throw 'Windows UI failed to initialize' }
        $firmware.Text = 'test.bin'
        $method.SelectedIndex = 2
        $address.Text = '192.168.1.50'
        if ($firmware.Text -or '.bin' -in @(Get-Formats) -or (Get-Target) -ne '192.168.1.50:5000') { throw 'TCP firmware selection failed' }
        $method.SelectedIndex = 3
        if ($update.Text -ne 'Начать' -or '.uf2' -notin @(Get-Formats)) { throw 'DFU mode failed' }
        $state.Mode = 'DfuList'
        $state.Text = '[{"name":"HT-n5262-OTA","address":"02:00:00:12:34:56","board":"heltec-t114"}]'
        Finish-Engine 0
        if ((Get-Board) -ne 'heltec-t114' -or (Get-Target) -ne '02:00:00:12:34:56') { throw 'DFU selection failed' }
        Switch-Journal
        if ($state.JournalOpen -or $journalToggle.Text -ne 'Развернуть') { throw 'Journal collapse failed' }
        Switch-Journal
        if (!$state.JournalOpen -or $journalToggle.Text -ne 'Свернуть') { throw 'Journal expansion failed' }
        if (!(Test-Path -LiteralPath $exe -PathType Leaf)) { throw 'Missing Windows engine' }
        Write-Output 'Windows UI smoke passed'
    } elseif (-not (Test-Path -LiteralPath $exe -PathType Leaf)) {
        Show-Notice 'Поместите engine\python.exe в папку рядом с интерфейсом.'
    } else {
        $portTimer.Start()
        $null = $form.ShowDialog()
    }
} finally {
    $portTimer.Dispose()
    $tips.Dispose()
    $timer.Dispose()
    $form.Dispose()
}
