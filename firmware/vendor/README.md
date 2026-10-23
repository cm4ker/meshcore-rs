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
