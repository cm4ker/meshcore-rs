import asyncio
import contextlib
import io
from types import SimpleNamespace
import unittest
from unittest.mock import AsyncMock, patch

import bleak
from bleak.exc import BleakBluetoothNotAvailableError, BleakBluetoothNotAvailableReason, BleakError
from ble_scan import BluetoothScanUnavailable
from nrf_flash import scan_dfu
from ota_app import BleLink, BluetoothLinkError, NUS_SERVICE, NUS_TX, main, open_link


class BluetoothScanTest(unittest.IsolatedAsyncioTestCase):
    async def test_both_scan_modes_explain_powered_off_adapter(self):
        error = BleakBluetoothNotAvailableError("Adapter off", BleakBluetoothNotAvailableReason.POWERED_OFF)
        with (patch.object(bleak.BleakScanner, "discover", side_effect=error) as scan,
              contextlib.redirect_stdout(io.StringIO())):
            for operation in (lambda: main(["--scan-ble"]), scan_dfu):
                with self.assertRaisesRegex(BluetoothScanUnavailable, "Адаптер выключен"):
                    await operation()
        self.assertEqual(scan.await_count, 2)

    async def test_enabled_adapter_returns_device_results(self):
        device = SimpleNamespace(address="02:00:00:12:34:56", name="MeshCore-01")
        bootloader = SimpleNamespace(address="02:00:00:12:34:57", name="Nordic DFU")
        found = {
            d.address: (d, SimpleNamespace(local_name=d.name, service_uuids=[], rssi=-50))
            for d in (device, bootloader)
        }
        with (patch.object(bleak.BleakScanner, "discover", return_value=found) as scan,
              contextlib.redirect_stdout(io.StringIO()) as console):
            await main(["--scan-ble"])
            dfu = await scan_dfu()
        self.assertIn(device.address, console.getvalue())
        self.assertEqual([item["address"] for item in dfu], [bootloader.address])
        self.assertEqual(scan.await_count, 2)


class BluetoothLinkTest(unittest.IsolatedAsyncioTestCase):
    async def test_disconnection_at_notify_recreates_session(self):
        failed = SimpleNamespace(is_connected=False, connect=AsyncMock(), disconnect=AsyncMock(),
                                 start_notify=AsyncMock(side_effect=BleakError("Not connected")))
        clean = SimpleNamespace(is_connected=True, connect=AsyncMock(), disconnect=AsyncMock(),
                                start_notify=AsyncMock())
        devices = [object(), object()]
        with (patch.object(bleak.BleakScanner, "find_device_by_address", side_effect=devices) as scan,
              patch.object(bleak, "BleakClient", side_effect=[failed, clean]) as create,
              patch("ota_app.sys", SimpleNamespace(platform="win32")),
              patch("ota_app.asyncio.sleep", new=AsyncMock()),
              contextlib.redirect_stdout(io.StringIO()) as output):
            link = await BleLink.open("02:00:00:12:34:56")
        self.assertEqual(scan.call_count, 2)
        self.assertIs(create.call_args_list[0].args[0], devices[0])
        self.assertIs(create.call_args_list[1].args[0], devices[1])
        self.assertEqual(create.call_args_list[1].kwargs["winrt"], {"use_cached_services": False})
        self.assertEqual(create.call_args_list[1].kwargs["services"], (NUS_SERVICE,))
        failed.disconnect.assert_awaited_once()
        clean.disconnect.assert_not_awaited()
        self.assertIn("попытка 1/3", output.getvalue())
        characteristic, receive = clean.start_notify.await_args.args
        self.assertEqual(characteristic, NUS_TX)
        receive(None, bytearray(b"123"))
        self.assertEqual(link.frames.get_nowait(), b"123")
        failed.start_notify.await_args.args[1](None, bytearray(b"old"))
        self.assertTrue(link.frames.empty())
        await link.close()
        clean.disconnect.assert_awaited_once()

    async def test_disabled_adapter_fails_without_retrying(self):
        error = BleakBluetoothNotAvailableError("Adapter off", BleakBluetoothNotAvailableReason.POWERED_OFF)
        with patch.object(bleak.BleakScanner, "find_device_by_address", side_effect=error) as scan:
            with self.assertRaisesRegex(BluetoothLinkError, "Адаптер недоступен"):
                await BleLink.open("02:00:00:12:34:56")
        scan.assert_awaited_once()

    async def test_scan_as_part_of_setup_is_retried_but_bounded(self):
        with (patch.object(bleak.BleakScanner, "find_device_by_address", return_value=None) as scan,
              patch("ota_app.asyncio.sleep", new=AsyncMock()),
              contextlib.redirect_stdout(io.StringIO()),
              self.assertRaisesRegex(BluetoothLinkError, "Не удалось найти плату")):
            await BleLink.open("02:00:00:12:34:56")
        self.assertEqual(scan.await_count, 3)

    async def test_invalid_pin_is_rejected_before_pairing(self):
        args = SimpleNamespace(port=None, ble="02:00:00:12:34:56", pin="abc123")
        with (patch("ota_app.sys", SimpleNamespace(platform="win32")),
              patch("ota_app.pair_windows", new_callable=AsyncMock) as pair,
              self.assertRaisesRegex(BluetoothLinkError, "шестизначный PIN")):
            await open_link(args)
        pair.assert_not_awaited()


if __name__ == "__main__":
    unittest.main()
