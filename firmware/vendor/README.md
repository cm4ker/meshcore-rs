# Vendored crates

## trouble-host 0.7.0

BLE host stack (embassy-rs/trouble), Apache-2.0 OR MIT, from crates.io with three changes, all
marked `meshcore-rust patch`:

- `set_fixed_pass_key()`: passkey-entry pairing can use a fixed 6-digit PIN instead of a random
  one per pairing (upstream MeshCore uses a static PIN on boards without a display).
- `set_identity_irk()`: the IRK handed to peers while privacy is off. 0.7.0 sends an all-zero
  IRK, and Windows then refuses to bond a second such device (the IRK collides with the first).
- `GAP_SERVICE_ATTRIBUTE_COUNT` is defined for `security` without `central` (0.7.0 left it
  undefined, which broke the `gatt_server` macro for peripheral-only builds).

The board crates use it through `[patch.crates-io]`.

## trouble-host 0.8.0 (`trouble-host-0.8`)

The same crate at 0.8.0 for the nRF52 board (its BLE controller, Nordic's SDC, needs bt-hci
0.10), with one change marked `meshcore-rust patch`: `set_identity_irk()`, as above. 0.8.0 already
has a fixed passkey (`set_passkey`) and the GAP attribute count. The T-Echo crate uses it through
`[patch.crates-io]`.

## embassy-nrf 0.11.0 (`embassy-nrf-0.11`)

The nRF HAL (embassy-rs/embassy), Apache-2.0 OR MIT, from crates.io with two USB fixes that
upstream made after 0.11.0, marked `meshcore-rust patch` in `src/usb/mod.rs`:

- an OUT endpoint is no longer re-armed by writing `SIZE.EPOUT` after each DMA: the USBD accepts
  the next packet by itself, and the extra write dropped one that arrived in between. Under a
  steady stream from the host (a firmware update over USB) whole 64-byte packets went missing;
- Erratum 199 ("USBD cannot receive tasks during DMA") is worked around around each DMA.

The T-Echo crate uses it through `[patch.crates-io]`.
