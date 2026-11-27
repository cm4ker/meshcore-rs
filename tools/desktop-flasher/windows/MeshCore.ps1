param([switch]$SmokeTest, [string]$PreviewPath)

$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Windows.Forms, System.Drawing
[Windows.Forms.Application]::EnableVisualStyles()

$appDirectory = if (Test-Path -LiteralPath (Join-Path $PSScriptRoot 'ommesh.ico')) { $PSScriptRoot } else { Split-Path -Parent $PSScriptRoot }
$exe = Join-Path $appDirectory 'engine\python.exe'
$form = New-Object Windows.Forms.Form
$form.Icon = [Drawing.Icon]::new((Join-Path $appDirectory 'ommesh.ico'))
$form.Text = 'ota-app 1.3.1 · MeshCore'
$form.AutoScaleDimensions = [Drawing.SizeF]::new(96, 96)
$form.AutoScaleMode = 'Dpi'
$form.ClientSize = [Drawing.Size]::new(820, 690)
$form.MinimumSize = [Drawing.Size]::new(682, 470)
$form.StartPosition = 'CenterScreen'
$form.Font = [Drawing.Font]::new('Segoe UI', 10)
$palette = @{
    Screen = [Drawing.ColorTranslator]::FromHtml('#EDF4F1')
    Surface = [Drawing.Color]::White
    Brand = [Drawing.ColorTranslator]::FromHtml('#103C34')
    Subtitle = [Drawing.ColorTranslator]::FromHtml('#B8D7CF')
    Ink = [Drawing.ColorTranslator]::FromHtml('#192F29')
    Muted = [Drawing.ColorTranslator]::FromHtml('#4F645E')
    Stroke = [Drawing.ColorTranslator]::FromHtml('#D6E3DE')
    Field = [Drawing.ColorTranslator]::FromHtml('#F6F9F7')
    Accent = [Drawing.ColorTranslator]::FromHtml('#006853')
    AccentLight = [Drawing.ColorTranslator]::FromHtml('#DFEFE9')
    Disabled = [Drawing.ColorTranslator]::FromHtml('#E8EEEB')
    Error = [Drawing.ColorTranslator]::FromHtml('#B3261E')
    ErrorLight = [Drawing.ColorTranslator]::FromHtml('#F9DEDC')
}
$form.BackColor = $palette.Screen
$form.ForeColor = $palette.Ink
$sectionFont = [Drawing.Font]::new('Segoe UI Semibold', 15)
$buttonFont = [Drawing.Font]::new('Segoe UI Semibold', 10)
$smallFont = [Drawing.Font]::new('Segoe UI', 9)
$titleFont = [Drawing.Font]::new('Segoe UI Semibold', 18)
$logFont = [Drawing.Font]::new('Consolas', 10)
$newline = [Environment]::NewLine

function Set-Rounded([Windows.Forms.Control]$control, [int]$radius) {
    $control.Add_Resize({
        param($sender, $eventArgs)
        $width = $sender.ClientSize.Width
        $height = $sender.ClientSize.Height
        if ($width -lt 2 -or $height -lt 2) { return }
        $diameter = [Math]::Min(2 * $radius * $sender.DeviceDpi / 96, [Math]::Min($width, $height))
        $path = [Drawing.Drawing2D.GraphicsPath]::new()
        try {
            $path.AddArc(0, 0, $diameter, $diameter, 180, 90)
            $path.AddArc($width - $diameter, 0, $diameter, $diameter, 270, 90)
            $path.AddArc($width - $diameter, $height - $diameter, $diameter, $diameter, 0, 90)
            $path.AddArc(0, $height - $diameter, $diameter, $diameter, 90, 90)
            $path.CloseFigure()
            $oldRegion = $sender.Region
            $sender.Region = [Drawing.Region]::new($path)
            if ($oldRegion) { $oldRegion.Dispose() }
        } finally { $path.Dispose() }
    }.GetNewClosure())
}

function Set-ButtonTheme($button, [string]$appearance = 'Soft') {
    $button.FlatStyle = 'Flat'
    $button.Font = $buttonFont
    $button.Cursor = [Windows.Forms.Cursors]::Hand
    $button.FlatAppearance.BorderSize = 0
    $button.UseVisualStyleBackColor = $false
    $button.Tag = @{
        Fill = switch ($appearance) {
            'Primary' { $palette.Accent }
            'Quiet' { $palette.Surface }
            default { $palette.AccentLight }
        }
        Text = if ($appearance -eq 'Primary') { $palette.Surface } else { $palette.Accent }
        Disabled = $palette.Disabled
        Muted = $palette.Muted
    }
    $button.BackColor = $button.Tag.Fill
    $button.ForeColor = $button.Tag.Text
    $button.FlatAppearance.MouseOverBackColor = if ($appearance -eq 'Primary') { $palette.Brand } else { $palette.Stroke }
    $button.Add_EnabledChanged({
        param($sender, $eventArgs)
        $sender.BackColor = if ($sender.Enabled) { $sender.Tag.Fill } else { $sender.Tag.Disabled }
        $sender.ForeColor = if ($sender.Enabled) { $sender.Tag.Text } else { $sender.Tag.Muted }
    })
    Set-Rounded $button 8
}

function Add-Control($type, $caption, $x, $y, $width, $height, $parent, [string]$appearance = 'Soft') {
    $control = New-Object "Windows.Forms.$type"
    $control.Text = $caption
    if ($type -eq 'Button') { Set-ButtonTheme $control $appearance }
    if ($type -in @('TextBox','ListBox','ComboBox')) {
        $control.BackColor = $palette.Field
        $control.ForeColor = $palette.Ink
    }
    if ($type -in @('TextBox','ListBox')) { $control.BorderStyle = 'FixedSingle' }
    if ($type -eq 'ComboBox') { $control.FlatStyle = 'Flat' }
    $control.SetBounds($x, $y, $width, $height)
    $parent.Controls.Add($control)
    return $control
}

$shell = Add-Control TableLayoutPanel '' 0 0 820 690 $form
$shell.Dock = 'Fill'
$shell.ColumnCount = 1
$shell.RowCount = 3
$null = $shell.ColumnStyles.Add([Windows.Forms.ColumnStyle]::new('Percent', 100))
$null = $shell.RowStyles.Add([Windows.Forms.RowStyle]::new('Absolute', 72))
$null = $shell.RowStyles.Add([Windows.Forms.RowStyle]::new('Percent', 100))
$drawerRow = [Windows.Forms.RowStyle]::new('Absolute', 66)
$null = $shell.RowStyles.Add($drawerRow)

function Add-ShellPanel([int]$row, [Drawing.Color]$color) {
    $panel = [Windows.Forms.Panel]::new()
    $panel.Width = 820
    $panel.Margin = [Windows.Forms.Padding]::Empty
    $panel.Dock = 'Fill'
    $panel.BackColor = $color
    $shell.Controls.Add($panel, 0, $row)
    return $panel
}

$header = Add-ShellPanel 0 $palette.Brand
$brandIcon = Add-Control PictureBox '' 22 16 40 40 $header
$brandIcon.SizeMode = 'Zoom'
$brandImagePath = Join-Path $appDirectory 'ommesh.png'
$brandIcon.Image = if (Test-Path -LiteralPath $brandImagePath) {
    [Drawing.Image]::FromFile($brandImagePath)
} else { $form.Icon.ToBitmap() }
$brandTitle = Add-Control Label 'ota-app' 74 9 270 34 $header
$brandTitle.Font = $titleFont
$brandTitle.ForeColor = $palette.Surface
$brandSubtitle = Add-Control Label 'Прошивка MeshCore' 76 42 354 22 $header
$brandSubtitle.ForeColor = $palette.Subtitle
$brandSubtitle.Font = $smallFont

$viewport = Add-ShellPanel 1 $palette.Screen
$viewport.AutoScroll = $true
$page = Add-Control Panel '' 0 0 820 482 $viewport
$page.Dock = 'Top'

function Add-Card([int]$top, [int]$height, [string]$caption) {
    $card = Add-Control Panel '' 16 $top 788 $height $page
    $card.BackColor = $palette.Surface
    $card.Anchor = 'Top, Left, Right'
    Set-Rounded $card 14
    $title = Add-Control Label $caption 14 11 318 32 $card
    $title.Font = $sectionFont
    return $card
}

$connection = Add-Card 12 230 'Подключение'
$methodLabel = Add-Control Label 'Способ подключения' 16 45 256 18 $connection
$methodLabel.Font = $smallFont
$methodLabel.ForeColor = $palette.Muted
$targetLabel = Add-Control Label 'Устройство' 287 45 475 18 $connection
$targetLabel.Font = $smallFont
$targetLabel.ForeColor = $palette.Muted
$method = Add-Control ComboBox '' 16 66 255 29 $connection
$method.DropDownStyle = 'DropDownList'
$method.Items.AddRange(@('USB','Обновление по Bluetooth','Wi-Fi / TCP','Загрузчик nRF52 (UF2)'))
$address = Add-Control ComboBox '' 287 66 485 29 $connection
$address.Anchor = 'Top, Left, Right'
$address.DropDownWidth = 470
$addressLabel = Add-Control Label '' 287 96 485 24 $connection
$addressLabel.Anchor = 'Top, Left, Right'
$addressLabel.Font = $smallFont
$addressLabel.ForeColor = $palette.Muted
$pinLabel = Add-Control Label 'PIN Bluetooth' 16 103 115 21 $connection
$pinLabel.ForeColor = $palette.Muted
$pinLabel.Font = $smallFont
$pin = Add-Control TextBox '' 136 99 70 24 $connection
$pin.UseSystemPasswordChar = $true
$pin.MaxLength = 6
$modelLabel = Add-Control Label 'Модель платы' 16 103 104 22 $connection
$modelLabel.ForeColor = $palette.Muted
$modelLabel.Font = $smallFont
$model = Add-Control ComboBox '' 120 99 151 26 $connection
$model.DropDownWidth = 225
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
$diskButton = Add-Control Button 'USB-диск UF2…' 16 97 163 29 $connection
$deviceDetails = Add-Control Label 'Подключите устройство и проверьте его.' 16 132 756 44 $connection
$deviceDetails.Anchor = 'Top, Left, Right'
$deviceDetails.BackColor = $palette.Field
$deviceDetails.Padding = [Windows.Forms.Padding]::new(10, 3, 10, 3)
$deviceDetails.TextAlign = 'MiddleLeft'
$deviceDetails.AutoEllipsis = $true
Set-Rounded $deviceDetails 8
$find = Add-Control Button 'Найти устройства' 16 185 179 34 $connection
$help = Add-Control Button 'ИНСТРУКЦИЯ' 203 185 130 34 $connection 'Quiet'
$info = Add-Control Button 'Проверить устройство' 572 185 200 34 $connection 'Quiet'
$info.Anchor = 'Top, Right'

$firmwareCard = Add-Card 254 214 'Прошивка'
$fileField = Add-Control Panel '' 16 43 708 33 $firmwareCard
$fileField.BackColor = $palette.Field
$fileField.Anchor = 'Top, Left, Right'
Set-Rounded $fileField 8
$firmware = Add-Control TextBox '' 9 8 690 23 $fileField
$firmware.Anchor = 'Top, Left, Right'
$firmware.BorderStyle = 'None'
$firmware.ReadOnly = $true
$firmware.AccessibleName = 'Файл прошивки'
$clearFile = Add-Control Button '×' 732 43 40 32 $firmwareCard 'Quiet'
$clearFile.Anchor = 'Top, Right'
$clearFile.AccessibleName = 'Снять выбор файла'
$sources = Add-Control TableLayoutPanel '' 16 81 756 35 $firmwareCard
$sources.Anchor = 'Top, Left, Right'
$sources.ColumnCount = 2
$sources.RowCount = 1
$null = $sources.ColumnStyles.Add([Windows.Forms.ColumnStyle]::new('Percent', 50))
$null = $sources.ColumnStyles.Add([Windows.Forms.ColumnStyle]::new('Percent', 50))
$browse = Add-Control Button 'Выбрать файл' 0 0 370 35 $sources
$browse.Margin = [Windows.Forms.Padding]::new(0, 0, 4, 0)
$browse.Dock = 'Fill'
$catalog = Add-Control Button 'Из GitHub' 0 0 370 35 $sources
$catalog.Margin = [Windows.Forms.Padding]::new(4, 0, 0, 0)
$catalog.Dock = 'Fill'
$update = Add-Control Button 'Обновить прошивку' 16 124 756 42 $firmwareCard 'Primary'
$update.Anchor = 'Top, Left, Right'
$usbFlash = Add-Control Button 'Прошить через USB' 16 173 171 33 $firmwareCard 'Quiet'
$newNode = Add-Control Button 'Новая нода со сбросом…' 195 173 220 33 $firmwareCard 'Quiet'
$exportDfu = Add-Control Button 'Сохранить DFU ZIP' 16 173 171 33 $firmwareCard 'Quiet'

$journal = Add-ShellPanel 2 $palette.Surface
$grip = Add-Control Panel '' 0 3 820 14 $journal
$grip.Anchor = 'Top, Left, Right'
$grip.Cursor = [Windows.Forms.Cursors]::Hand
$handle = Add-Control Panel '' 392 5 36 4 $grip
$handle.BackColor = $palette.Muted
$handle.Cursor = [Windows.Forms.Cursors]::Hand
$grip.Add_Resize({ $handle.Left = [Math]::Max(0, ($grip.ClientSize.Width - $handle.Width) / 2) })
$progressArea = Add-Control Panel '' 20 19 780 39 $journal
$progressArea.Anchor = 'Top, Left, Right'
$progressArea.Visible = $false
$progressTitle = Add-Control Label '' 0 0 675 23 $progressArea
$progressTitle.Anchor = 'Top, Left, Right'
$progressTitle.Font = $buttonFont
$progressTitle.ForeColor = $palette.Brand
$progressTitle.TextAlign = 'MiddleLeft'
$progressValue = Add-Control Label '' 675 0 105 23 $progressArea
$progressValue.Anchor = 'Top, Right'
$progressValue.Font = $buttonFont
$progressValue.ForeColor = $palette.Accent
$progressValue.TextAlign = 'MiddleRight'
$track = Add-Control Panel '' 0 26 780 9 $progressArea
$track.Anchor = 'Top, Left, Right'
$track.BackColor = $palette.Stroke
$track.AccessibleName = 'Ход операции'
$track.AccessibleRole = 'ProgressBar'
Set-Rounded $track 4
$progressFill = Add-Control Panel '' 0 0 0 9 $track
$progressFill.BackColor = $palette.Accent
$journalHeading = Add-Control Label 'Журнал' 20 26 81 27 $journal
$journalHeading.Font = $buttonFont
$journalHeading.TextAlign = 'MiddleLeft'
$journalToggle = Add-Control Button 'Развернуть' 680 23 120 34 $journal 'Quiet'
$journalToggle.Anchor = 'Top, Right'
$status = Add-Control Label 'Готово к работе' 107 26 560 27 $journal
$status.Anchor = 'Top, Left, Right'
$status.TextAlign = 'MiddleLeft'
$status.ForeColor = $palette.Muted
$status.AutoEllipsis = $true
$log = Add-Control TextBox '' 20 64 780 130 $journal
$log.Multiline = $true
$log.ReadOnly = $true
$log.ScrollBars = 'Both'
$log.WordWrap = $false
$log.Font = $logFont
$log.Visible = $false

$ports = @{ Values = @() }
$state = @{
    Process = $null; Output = $null; Errors = $null; Mode = ''; Text = ''
    Drive = $null; PendingRoot = ''; DfuDevices = @{}; DeviceKey = ''; Device = $null
    JournalOpen = $false; ProgressTitle = ''; ProgressPercent = -1
}
$transferModes = @('Download','Update','USBFlash','NewNode','DfuWrite','Uf2Write')
$operationTitles = @{
    Download = 'Загрузка прошивки из GitHub'
    Update = 'Обновление прошивки'
    USBFlash = 'Запись прошивки через USB'
    NewNode = 'Запись прошивки со сбросом'
    DfuWrite = 'Запись прошивки через загрузчик nRF52'
    Uf2Write = 'Запись UF2 на устройство'
    Info = 'Проверка устройства'
    Uf2Info = 'Проверка диска загрузчика UF2'
    List = 'Поиск устройств Bluetooth'
    DfuList = 'Поиск загрузчиков nRF52'
    Catalog = 'Загрузка каталога GitHub'
    Export = 'Сохранение DFU-пакета'
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
    $targetLabel.Text = if ($method.SelectedIndex -eq 2) { 'Адрес платы' } elseif ($dfu) { 'Загрузчик' } elseif ($drive) { 'Диск загрузчика' } else { 'Устройство' }
    $address.Enabled = -not $busy -and -not ($drive -and $state.Drive)
    $ble = $method.SelectedIndex -eq 1
    $pinLabel.Visible = $ble
    $pin.Visible = $ble
    $diskButton.Visible = $usb -and (-not $drive -or $null -ne $state.Drive -and $state.Drive.board)
    $showModel = $dfu -or ($drive -and $state.Drive -and -not $state.Drive.board)
    $model.Visible = $showModel
    $modelLabel.Visible = $showModel
    $known = if ($dfu) { $state.DfuDevices[(Get-Target)] } else { $null }
    if ($known -and $known.board -in $boards.Values) {
        $model.SelectedIndex = [array]::IndexOf(@($boards.Values),$known.board)
        $model.Enabled = $false
    }
    $find.Enabled = -not $busy -and $method.SelectedIndex -ne 2
    $find.Visible = $method.SelectedIndex -ne 2
    $help.Left = if ($method.SelectedIndex -eq 2) { $find.Left } else { $find.Right + $find.Left / 2 }
    $find.Text = if ($dfu) { 'Найти загрузчик' } else { 'Найти устройства' }
    $showUsbTools = $usb -and -not $drive
    $usbFlash.Visible = $showUsbTools
    $newNode.Visible = $showUsbTools
    $usbFlash.Enabled = -not $busy -and (Get-FirmwareExtension) -ne '.mcfd'
    $newNode.Enabled = $usbFlash.Enabled
    $info.Visible = -not $dfu
    $exportDfu.Visible = $dfu
    $exportDfu.Enabled = -not $busy -and (Get-FirmwareExtension) -eq '.uf2'
    $update.Text = if ($dfu) { 'Начать' } elseif ($drive) { 'Прошить UF2 по USB' } else { 'Обновить прошивку' }
    $actionsBottom = if ($showUsbTools -or $dfu) { $usbFlash.Bottom } else { $update.Bottom }
    $firmwareCard.Height = $actionsBottom + $update.Left
    $page.Height = $firmwareCard.Bottom + $connection.Top
    $clearFile.Enabled = -not $busy -and [bool]$firmware.Text
    if ($drive -and $state.Drive) {
        $deviceDetails.Text = (($state.Drive.model, "UF2-загрузчик $($state.Drive.version) · $($state.Drive.softdevice)") -join $newline)
    } elseif ($dfu -and $known) {
        $deviceDetails.Text = if ($known.board) { "Загрузчик распознан · $($model.Text)" } else { 'Загрузчик обнаружен. Выберите модель платы.' }
    } elseif ($state.Device -and $state.DeviceKey -eq (Get-ContextKey)) {
        $deviceDetails.Text = $state.Device.summary
    } else {
        $deviceDetails.Text = if ($dfu) { 'Переведите плату в BLE OTA и запустите поиск.' } else { 'Подключите устройство и проверьте его.' }
    }
    $tips.SetToolTip($deviceDetails, $deviceDetails.Text)
    $tips.SetToolTip($firmware, $firmware.Text)
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
        $dialog.AutoScaleDimensions = [Drawing.SizeF]::new(96, 96)
        $dialog.AutoScaleMode = 'Dpi'
        $dialog.BackColor = $palette.Surface
        $dialog.ClientSize = [Drawing.Size]::new(690,450)
        $dialog.FormBorderStyle = 'FixedDialog'
        $dialog.MinimizeBox = $false
        $dialog.MaximizeBox = $false
        $heading = Add-Control Label $title 18 14 654 34 $dialog
        $heading.ForeColor = $palette.Ink
        $heading.Font = $sectionFont
        $content = Add-Control TextBox $message 18 56 654 336 $dialog
        $content.Multiline = $true
        $content.ReadOnly = $true
        $content.ScrollBars = 'Vertical'
        $content.Text = ($message -replace '\r?\n',$newline)
        $content.SelectionLength = 0
        $close = Add-Control Button 'Закрыть' 552 405 120 32 $dialog 'Quiet'
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

function Set-JournalLayout {
    $height = if ($state.JournalOpen) { 213 } else { 66 }
    $offset = if ($state.ProgressTitle) { 42 } else { 0 }
    $scale = $form.DeviceDpi / 96
    $journalHeading.Top = [Math]::Round((26 + $offset) * $scale)
    $status.Top = $journalHeading.Top
    $journalToggle.Top = [Math]::Round((23 + $offset) * $scale)
    $log.Top = [Math]::Round((64 + $offset) * $scale)
    $drawerRow.Height = [Math]::Round(($height + $offset) * $scale)
    $log.Visible = $state.JournalOpen
    $journalToggle.Text = if ($state.JournalOpen) { 'Свернуть' } else { 'Развернуть' }
    $shell.PerformLayout()
}

function Switch-Journal {
    $state.JournalOpen = -not $state.JournalOpen
    Set-JournalLayout
    if ($state.JournalOpen) {
        $log.SelectionStart = $log.TextLength
        $log.ScrollToCaret()
    }
}

function Set-ProgressFill {
    $width = [Math]::Round($track.ClientSize.Width * [Math]::Max(0, $state.ProgressPercent) / 100)
    $progressFill.Size = [Drawing.Size]::new($width, $track.ClientSize.Height)
}

function Set-OperationProgress([string]$title, [int]$percent = -1, [switch]$Failed) {
    $visibilityChanged = [bool]$state.ProgressTitle -ne [bool]$title
    $state.ProgressTitle = $title
    $state.ProgressPercent = $percent
    $progressTitle.Text = $title
    $progressTitle.ForeColor = if ($Failed) { $palette.Error } else { $palette.Brand }
    $progressValue.ForeColor = if ($Failed) { $palette.Error } else { $palette.Accent }
    $progressValue.Text = if ($percent -ge 0) { "$percent%" } else { '—' }
    $track.AccessibleDescription = if ($percent -ge 0) { "$title, $percent%" } else { $title }
    $track.BackColor = if ($Failed) { $palette.ErrorLight } else { $palette.Stroke }
    $progressFill.BackColor = if ($Failed) { $palette.Error } else { $palette.Accent }
    Set-ProgressFill
    $progressArea.Visible = [bool]$title
    if ($visibilityChanged) { Set-JournalLayout }
}

$track.Add_Resize({ Set-ProgressFill })
$journalToggle.Add_Click({ Switch-Journal })
$grip.Add_Click({ Switch-Journal })
$handle.Add_Click({ Switch-Journal })
$journal.Add_Resize({
    $log.Width = [Math]::Max(1, $journal.ClientSize.Width - 2 * $log.Left)
    $log.Height = [Math]::Max(1, $journal.ClientSize.Height - $log.Top - $log.Left / 2)
})

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
    Set-OperationProgress ''
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
        if ($dialog.ShowDialog($form) -eq 'OK') {
            Set-OperationProgress ''
            Set-Firmware $dialog.FileName
        }
    } finally { $dialog.Dispose() }
})
$clearFile.Add_Click({
    if ((Get-FirmwareExtension) -eq '.uf2' -and $state.Drive) {
        $state.Drive = $null
        $address.Text = ''
    }
    Set-Firmware ''
    Set-OperationProgress ''
    $status.Text = 'Выбор файла снят.'
})

function Select-DfuRole {
    $dialog = New-Object Windows.Forms.Form
    try {
        $dialog.Text = 'Роль прошивки UF2'
        $dialog.Font = $form.Font
        $dialog.Icon = $form.Icon
        $dialog.StartPosition = 'CenterParent'
        $dialog.AutoScaleDimensions = [Drawing.SizeF]::new(96, 96)
        $dialog.AutoScaleMode = 'Dpi'
        $dialog.BackColor = $palette.Surface
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
        $dialog.AutoScaleDimensions = [Drawing.SizeF]::new(96, 96)
        $dialog.AutoScaleMode = 'Dpi'
        $dialog.BackColor = $palette.Surface
        $dialog.FormBorderStyle = 'FixedDialog'
        $dialog.ClientSize = [Drawing.Size]::new(680,269)
        $dialog.MinimizeBox = $false
        $dialog.MaximizeBox = $false
        $summary = if ($data.device.summary) { $data.device.summary } else { "Загрузчик UF2 · $($model.Text)" }
        $heading = Add-Control Label 'Прошивки из GitHub' 18 12 644 34 $dialog
        $heading.Font = $sectionFont
        $heading.ForeColor = $palette.Ink
        $summaryLabel = Add-Control Label $summary 18 51 644 41 $dialog
        $summaryLabel.ForeColor = $palette.Muted
        $pick = @{ Asset = $null; Open = $false }
        $recommended = $null
        if ($null -ne $data.recommended -and [int]$data.recommended -ge 0 -and [int]$data.recommended -lt $items.Count) {
            $recommended = $items[[int]$data.recommended]
        }
        $card = Add-Control Panel '' 18 94 644 82 $dialog
        Set-Rounded $card 10
        $card.BackColor = $palette.Field
        $cardTitle = Add-Control Label '' 14 9 615 25 $card
        $cardTitle.Font = $buttonFont
        $cardTitle.ForeColor = $palette.Accent
        $cardDetail = Add-Control Label '' 14 37 615 40 $card
        $cardDetail.ForeColor = $palette.Ink
        if ($recommended) {
            $cardTitle.Text = "Рекомендуем · $($recommended.tag)"
            $cardDetail.Text = Get-AssetDetails $recommended
            $card.BackColor = $palette.AccentLight
            $pick.Asset = $recommended
        } elseif (!$items.Count) {
            $cardTitle.Text = 'В каталоге нет файлов для этой модели и роли.'
        } elseif ($data.device.build -and -not ($items | Where-Object { $_.build -eq 0 -or $_.build -gt $data.device.build })) {
            $cardTitle.Text = "В каталоге нет сборки новее №$($data.device.build)."
        } else {
            $cardTitle.Text = 'Подходящего обновления нет'
            $cardDetail.Text = 'Откройте другие файлы, чтобы посмотреть причины.'
        }
        $others = @($items | Where-Object { $_ -ne $recommended })
        $more = Add-Control Button "Другие файлы ($($others.Count))" 18 187 198 36 $dialog
        $more.Enabled = [bool]$others.Count
        $primary = Add-Control Button 'Скачать' 470 187 192 36 $dialog 'Primary'
        $primary.Enabled = $null -ne $recommended
        $primary.DialogResult = 'OK'
        $close = Add-Control Button 'Закрыть' 542 229 120 32 $dialog 'Quiet'
        $close.Anchor = 'Bottom, Right'
        $close.DialogResult = 'Cancel'
        $dialog.CancelButton = $close
        $list = Add-Control ListBox '' 18 236 644 181 $dialog
        $list.Visible = $false
        $detail = Add-Control Label '' 18 426 644 52 $dialog
        $detail.ForeColor = $palette.Muted
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
            $scale = $dialog.DeviceDpi / 96
            if ($pick.Open) {
                $dialog.ClientSize = [Drawing.Size]::new($dialog.ClientSize.Width, [Math]::Round(521 * $scale))
                $more.Text = 'Скрыть другие файлы'
                if ($list.SelectedIndex -lt 0 -and $list.Items.Count) { $list.SelectedIndex = 0 }
                $pick.Asset = $others[$list.SelectedIndex]
                $primary.Enabled = $null -ne $pick.Asset
            } else {
                $dialog.ClientSize = [Drawing.Size]::new($dialog.ClientSize.Width, [Math]::Round(269 * $scale))
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
    $start.EnvironmentVariables['OTA_APP_REPORT_PROGRESS'] = '1'
    Start-EngineProcess $mode $start
}

function Start-EngineProcess([string]$mode, [Diagnostics.ProcessStartInfo]$start) {
    $log.Clear()
    Set-OperationProgress $operationTitles[$mode]
    try { $state.Process = [Diagnostics.Process]::Start($start) }
    catch {
        Set-OperationProgress 'Не удалось начать операцию' -Failed
        $status.Text = 'Не удалось запустить утилиту MeshCore.'
        $log.AppendText($_.Exception.Message + $newline)
        Show-Notice ($status.Text + ' ' + $_.Exception.Message)
        return
    }
    $state.Mode = $mode
    $state.Text = ''
    $state.Output = $state.Process.StandardOutput.ReadLineAsync()
    $state.Errors = $state.Process.StandardError.ReadLineAsync()
    $status.Text = 'Подготовка…'
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
                Set-OperationProgress ''
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

function Receive-EngineLine([string]$stream, [string]$line) {
    if ($line.StartsWith('OTA_PROGRESS ', [StringComparison]::Ordinal)) {
        if ($state.Mode -in $transferModes -and $line -cmatch '\AOTA_PROGRESS ([0-9]{1,19}) ([0-9]{1,19})\z') {
            $current = [uint64]$Matches[1]
            $total = [uint64]$Matches[2]
            if ($total -gt 0 -and $current -le $total) {
                $percent = [int][Math]::Min(99, [Math]::Floor(100.0 * $current / $total))
                $title = $operationTitles[$state.Mode]
                if ($current -eq $total) {
                    $title = if ($state.Mode -eq 'Download') { 'Проверка загруженного файла' } else { 'Ожидание подтверждения записи' }
                }
                if ($percent -gt $state.ProgressPercent -or ($percent -eq $state.ProgressPercent -and $current -eq $total -and $title -ne $state.ProgressTitle)) {
                    Set-OperationProgress $title $percent
                }
            }
        }
        return
    }
    if ($stream -eq 'Output' -and $state.Mode -in @('Info','Catalog','Download','Uf2Info','DfuList','List')) {
        $state.Text += $line + $newline
    }
    if ($stream -eq 'Errors' -or $state.Mode -notin @('Info','Catalog','Download','Uf2Info','DfuList')) {
        $log.AppendText($line + $newline)
        if ($line.Trim()) { $status.Text = $line.Trim() }
    }
}

function Receive-EngineOutput {
    if (!$state.Process) { return }
    foreach ($stream in @('Output','Errors')) {
        for ($i = 0; $i -lt 150 -and $state[$stream] -and $state[$stream].IsCompleted; $i++) {
            $line = $state[$stream].GetAwaiter().GetResult()
            if ($null -eq $line) { $state[$stream] = $null; break }
            Receive-EngineLine $stream $line
            $reader = if ($stream -eq 'Output') { $state.Process.StandardOutput } else { $state.Process.StandardError }
            $state[$stream] = $reader.ReadLineAsync()
        }
    }
    if ($state.Output -or $state.Errors -or -not $state.Process.HasExited) { return }
    $timer.Stop()
    $exitCode = $state.Process.ExitCode
    $state.Process.Dispose()
    $state.Process = $null
    $mode = $state.Mode
    $transfer = $mode -in $transferModes
    try {
        if ($exitCode -ne 0) { Set-OperationProgress 'Операция прервана' $state.ProgressPercent -Failed }
        elseif (-not $transfer) { Set-OperationProgress '' }
        Finish-Engine $exitCode
        if ($exitCode -eq 0 -and $transfer) {
            $title = if ($mode -eq 'Download') { 'Загрузка завершена' } else { 'Запись завершена' }
            Set-OperationProgress $title 100
        }
    }
    catch {
        $log.AppendText($_.Exception.Message + $newline)
        $status.Text = 'Ошибка обработки результата. Подробности в журнале.'
        Set-OperationProgress 'Операция прервана' $state.ProgressPercent -Failed
    }
    Set-Controls
}
$timer.Add_Tick({ Receive-EngineOutput })

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

function Start-ProgressFixture([string]$mode, [string[]]$lines) {
    $fixture = [Diagnostics.ProcessStartInfo]::new($exe)
    $fixture.Arguments = '-u -c ' + (Quote-Argument ($lines -join "`n"))
    $fixture.UseShellExecute = $false
    $fixture.CreateNoWindow = $true
    $fixture.RedirectStandardInput = $true
    $fixture.RedirectStandardOutput = $true
    $fixture.RedirectStandardError = $true
    Start-EngineProcess $mode $fixture
}

function Wait-ProgressFixture([scriptblock]$condition, [string]$failure) {
    $wait = [Diagnostics.Stopwatch]::StartNew()
    do {
        Receive-EngineOutput
        if (& $condition) { return }
        [Threading.Thread]::Sleep(15)
    } while ($wait.Elapsed.TotalSeconds -lt 8)
    throw $failure
}

function Test-OperationProgress {
    try {
        Start-ProgressFixture 'Download' @('import json',
            'print("OTA_PROGRESS 0 100"); input()',
            'print("OTA_PROGRESS 25 100"); input()',
            'print("OTA_PROGRESS 100 100"); input()',
            'print(json.dumps({"path": "smoke.uf2"}))')
        if ($state.ProgressPercent -ne -1 -or $progressFill.Width -ne 0) { throw 'Unknown transfer must start with an empty track' }
        Wait-ProgressFixture { $state.ProgressPercent -eq 0 } 'No progress after the start of download'
        $state.Process.StandardInput.WriteLine('continue')
        Wait-ProgressFixture { $progressValue.Text -eq '25%' } 'No progress after receiving a quarter of the download'
        if ($progressFill.Width -ne [Math]::Round($track.ClientSize.Width / 4) -or $state.JournalOpen) { throw 'Progress fill must reflect the amount transferred with the journal closed' }
        $state.Process.StandardInput.WriteLine('continue')
        Wait-ProgressFixture { $state.ProgressPercent -eq 99 } 'Unverified transfer reached 100% or stopped updating'
        $state.Process.StandardInput.WriteLine('continue')
        Wait-ProgressFixture { !$state.Process } 'Transfer did not finish'
        if ($progressValue.Text -ne '100%' -or $firmware.Text -ne 'smoke.uf2' -or $log.Text -match 'OTA_PROGRESS') { throw 'Successful download or progress filtering failed' }

        Start-ProgressFixture 'DfuWrite' @('import sys', 'print("OTA_PROGRESS 100 100")', 'print("bad write", file=sys.stderr)', 'sys.exit(1)')
        Wait-ProgressFixture { !$state.Process } 'Failed flash did not finish'
        if ($state.ProgressPercent -ne 99 -or $progressValue.ForeColor -ne $palette.Error -or $log.Text -notmatch 'bad write' -or $log.Text -match 'OTA_PROGRESS') { throw 'Failure must retain the reported position and show the error' }

        Start-ProgressFixture 'Info' @('import json', 'print("OTA_PROGRESS 100 100")', 'print(json.dumps({"summary": "Test device"}))')
        if ($state.ProgressPercent -ne -1) { throw 'Checking a device inherited previous write progress' }
        Wait-ProgressFixture { !$state.Process } 'Device info did not finish'
        if ($state.Device.summary -ne 'Test device' -or $state.ProgressTitle -or $log.Text -match 'OTA_PROGRESS') { throw 'Info must preserve its data and clear the progress area' }

        Start-ProgressFixture 'List' @('print("02:00:00:12:34:56 Test device RSSI -45")')
        Wait-ProgressFixture { !$state.Process } 'Device discovery did not finish'
        if ($address.Items.Count -ne 1 -or $state.ProgressTitle) { throw 'Discovery must preserve its results and clear the progress area' }
    } finally {
        if ($state.Process) {
            $timer.Stop()
            if (!$state.Process.HasExited) { $state.Process.Kill() }
            $state.Process.Dispose()
            $state.Process = $null
        }
    }
}

try {
    if ($SmokeTest -or $PreviewPath) {
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
        $clientSize = $form.ClientSize
        Switch-Journal
        if (!$state.JournalOpen -or $journalToggle.Text -ne 'Свернуть') { throw 'Journal expansion failed' }
        Switch-Journal
        if ($state.JournalOpen -or $journalToggle.Text -ne 'Развернуть' -or $form.ClientSize -ne $clientSize) { throw 'Journal collapse failed' }
        $form.ClientSize = [Drawing.Size]::new(668, 470)
        Set-OperationProgress 'Обновление прошивки' 25
        $shell.PerformLayout()
        $connection.PerformLayout()
        if ($address.Right -gt $connection.ClientSize.Width -or $info.Left -lt $help.Right -or $fileField.Right -gt $clearFile.Left -or
            $progressArea.Right -gt $journal.ClientSize.Width -or $progressFill.Width -ne [Math]::Round($track.ClientSize.Width / 4)) {
            throw 'Windows controls overlap at minimum width.'
        }
        $form.ClientSize = $clientSize
        if ($SmokeTest -and !(Test-Path -LiteralPath $exe -PathType Leaf)) { throw 'Missing Windows engine' }
        if ($SmokeTest) { Test-OperationProgress }
        if ($PreviewPath) {
            $method.SelectedIndex = 2
            $address.Text = '192.168.1.50:5000'
            $state.DeviceKey = Get-ContextKey
            $state.Device = @{ summary = 'Heltec V4.3 OLED · v1.18.0-rs' + $newline + 'Сборка №192 · Компаньон · слот 4032 КиБ' }
            $firmware.Text = 'heltec-v4-companion-0.9.0-dev.22-r194.from-r192.mcfd'
            Set-Controls
            Set-OperationProgress 'Обновление прошивки' 70
            $status.Text = 'Передано 17 из 24 КиБ.'
            $form.StartPosition = 'Manual'
            $form.Location = [Drawing.Point]::new(-20000, -20000)
            $form.ShowInTaskbar = $false
            $form.Opacity = 0
            $form.Show()
            [Windows.Forms.Application]::DoEvents()
            $image = [Drawing.Bitmap]::new($form.Width, $form.Height)
            try {
                $form.DrawToBitmap($image, [Drawing.Rectangle]::new(0, 0, $image.Width, $image.Height))
                $image.Save([IO.Path]::GetFullPath($PreviewPath), [Drawing.Imaging.ImageFormat]::Png)
            } finally {
                $image.Dispose()
                $form.Hide()
            }
        }
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
    $brandIcon.Image.Dispose()
    $form.Dispose()
    foreach ($font in @($sectionFont, $buttonFont, $smallFont, $titleFont, $logFont)) { $font.Dispose() }
}
