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
