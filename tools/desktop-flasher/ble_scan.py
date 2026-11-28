"""Shared Bluetooth discovery for the desktop MeshCore and Nordic DFU views."""

class BluetoothScanUnavailable(OSError):
    """A Bluetooth availability problem that the scan UI can show directly."""


async def discover_ble(timeout=10):
    from bleak import BleakScanner
    from bleak.exc import BleakBluetoothNotAvailableError, BleakBluetoothNotAvailableReason as Reason

    try:
        return await BleakScanner.discover(timeout=timeout, return_adv=True)
    except BleakBluetoothNotAvailableError as exc:
        messages = {
            Reason.POWERED_OFF: "Адаптер выключен. Включите Bluetooth в настройках системы и повторите поиск.",
            Reason.NO_BLUETOOTH: "Адаптер не найден. Подключите адаптер Bluetooth LE и повторите поиск.",
            Reason.NO_BLE_CENTRAL_ROLE: "Адаптер не поддерживает поиск устройств Bluetooth LE.",
            Reason.DENIED_BY_USER: "Разрешите приложению доступ к Bluetooth в настройках системы.",
            Reason.DENIED_BY_SYSTEM: "Система ограничила доступ к Bluetooth.",
        }
        message = messages.get(exc.reason, "Проверьте, что адаптер включён и доступ разрешён.")
        raise BluetoothScanUnavailable(f"Bluetooth. {message}") from exc
