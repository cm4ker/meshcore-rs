# Прошивальщик MeshCore
Русский графический интерфейс для USB, BLE и TCP. Выберите порт или адрес, проверьте версию и откройте локальный файл прошивки. Кнопка «Прошить через USB» использует загрузчик ESP. «Новая нода со сбросом» удаляет ключ и пользовательские данные после подтверждения. Поддержка записи через загрузчик требует работающей meshcore-rs с совместимой двухслотовой разметкой.

| Система | Файл релиза |
| --- | --- |
| Windows x86 / x64 | MeshCore-Windows-x86.exe |
| macOS на Apple Silicon | MeshCore-macOS-Apple-Silicon.zip |
| Linux на ПК x86-64 | MeshCore-Linux-PC-x86_64.tar.gz |
| Linux на Raspberry Pi и других ARM64 | MeshCore-Linux-Raspberry-Pi-ARM64.tar.gz |
| Raspberry Pi OS 32-bit (ARMv7/ARMv8) | MeshCore-Linux-Raspberry-Pi-ARMv7-32bit.tar.gz |

На macOS распакуйте приложение. На Linux распакуйте архив и запустите ./MeshCore в графическом сеансе. Для USB нужны права пользователя на последовательный порт, для Bluetooth — работающий адаптер и системная служба Bluetooth.

Сборка macOS и Linux: Python 3.11 с Tk, затем bash build.sh ARCH в этой папке. Для Linux нужны Tk, Xvfb, xauth и binutils. Для Windows: 32-битный Python 3.11, затем windows/build.ps1 из этой папки. Графический клиент использует OTA meshcore-rs, esptool и pyserial. Иконка взята с nw.ommesh.ru.
