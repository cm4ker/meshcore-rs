# Callback-Based GATT Characteristics Guide

## Overview

TrouBLE now supports callback-based GATT characteristics where characteristic values are not stored in the attribute table. Instead, values are computed or retrieved on-demand through user-provided async handlers.

## Why Callback-Based Characteristics?

**Problems with storage-based characteristics:**
- Must pre-allocate storage buffers for all characteristics
- Values cannot be computed dynamically
- Cannot perform async I/O during reads (sensor reads, flash reads)
- Wastes memory storing values that could be computed

**Benefits of callback-based characteristics:**
- **No storage overhead** - values generated on-demand
- **Async operations** - read from sensors, flash, databases
- **Dynamic values** - compute values based on current state
- **Flexible** - full control over read/write logic

## Architecture

### User-Event-Based Handlers

Instead of storing handler trait objects (which aren't object-safe in no_std), **your `gatt_events_task` function IS the handler**:

```
Client Read Request
    ↓
AttributeServer generates GattEvent::Read
    ↓
Your gatt_events_task receives event
    ↓
Check event.handle() to identify characteristic
    ↓
Execute your async handler code
    ↓
Call event.respond(data) with response
    ↓
Response sent to client
```

## Usage

### 1. Define Callback-Based Characteristic

```rust
use trouble_host::prelude::*;

// Create attribute table
let mut table = AttributeTable::new();

// Add service
let mut service = table.add_service(Service::new(SERVICE_UUID));

// Add callback-based characteristic (no storage buffer needed!)
let battery_level = service
    .add_characteristic_with_handler::<u8, _>(
        characteristic::BATTERY_LEVEL,
        &[CharacteristicProp::Read, CharacteristicProp::Notify],
    )
    .build();

let service_handle = service.build();
```

**Key difference:** No `&'d mut [u8]` storage buffer needed!

### 2. Handle Events in Your Event Loop

```rust
async fn gatt_events_task<P: PacketPool>(
    server: &Server,
    conn: &GattConnection<'_, '_, P>,
) -> Result<(), Error> {
    let battery_level = server.battery_service.level;

    loop {
        match conn.next().await {
            GattConnectionEvent::Disconnected { reason } => break,
            GattConnectionEvent::Gatt { event } => {
                match event {
                    // Handle callback-based characteristic reads
                    GattEvent::Read(read) if read.handle() == battery_level.handle => {
                        // Perform async operation
                        let level = read_battery_from_adc().await;

                        // Respond with data
                        read.respond(&[level])?.send().await;
                    }

                    // Handle callback-based characteristic writes
                    GattEvent::Write(write) if write.handle() == some_control.handle => {
                        let data = write.data();

                        // Perform async operation
                        write_to_flash(data).await?;

                        // Accept the write
                        write.accept()?.send().await;
                    }

                    // For storage-based characteristics, use accept()
                    _ => {
                        event.accept()?.send().await;
                    }
                }
            }
            _ => {}
        }
    }
    Ok(())
}
```

### 3. Send Notifications

Notifications work the same way as before:

```rust
async fn notify_task<P: PacketPool>(
    characteristic: &Characteristic<u8>,
    conn: &GattConnection<'_, '_, P>,
) {
    loop {
        let value = read_battery_from_adc().await;

        // Notify does NOT store the value, just sends it
        characteristic.notify(conn, &value).await.ok();

        Timer::after_secs(60).await;
    }
}
```

## Complete Example

```rust
use trouble_host::prelude::*;

// Define service structure
#[gatt_server]
struct MyServer {
    battery_service: BatteryService,
}

#[gatt_service(uuid = service::BATTERY)]
struct BatteryService {
    // Callback-based characteristic - no storage!
    #[characteristic(uuid = characteristic::BATTERY_LEVEL, read, notify)]
    level: u8,
}

async fn run<C: Controller>(controller: C) {
    let mut resources = HostResources::new();
    let stack = trouble_host::new(controller, &mut resources);
    let Host { mut peripheral, runner, .. } = stack.build();

    // Create server (no storage buffers needed)
    let server = MyServer::new_with_config(GapConfig::Peripheral(
        PeripheralConfig {
            name: "MyDevice",
            appearance: &appearance::GENERIC_COMPUTER,
        }
    )).unwrap();

    join(
        runner.run(),
        async {
            loop {
                // Advertise and accept connection
                let conn = advertise(&mut peripheral, &server).await?;

                // Handle GATT events
                gatt_events_task(&server, &conn).await.ok();
            }
        }
    ).await;
}

async fn gatt_events_task<P: PacketPool>(
    server: &MyServer,
    conn: &GattConnection<'_, '_, P>,
) -> Result<(), Error> {
    let battery_level = server.battery_service.level;

    loop {
        match conn.next().await {
            GattConnectionEvent::Disconnected { reason } => break,
            GattConnectionEvent::Gatt { event } => {
                match event {
                    GattEvent::Read(read) if read.handle() == battery_level.handle => {
                        // Read from ADC (async operation)
                        let level = adc_read_battery().await;
                        info!("Battery read: {}", level);

                        // Respond with current value
                        read.respond(&[level])?.send().await;
                    }
                    _ => {
                        // Default: accept all other events
                        event.accept()?.send().await;
                    }
                }
            }
            _ => {}
        }
    }
    Ok(())
}

// Separate task for periodic notifications
async fn notification_task<P: PacketPool>(
    server: &MyServer,
    conn: &GattConnection<'_, '_, P>,
) {
    let battery_level = server.battery_service.level;

    loop {
        Timer::after_secs(60).await;

        let level = adc_read_battery().await;
        battery_level.notify(conn, &level).await.ok();
    }
}
```

## API Reference

### ReadEvent Methods

- **`handle() -> u16`** - Get the handle being read
- **`accept() -> Result<Reply>`** - Process using AttributeServer (reads from storage)
- **`reject(err: AttErrorCode) -> Result<Reply>`** - Reject with error code
- **`respond(data: &[u8]) -> Result<Reply>`** - Provide custom response data
- **`into_payload() -> GattData`** - Get raw payload for custom processing

### WriteEvent Methods

- **`handle() -> u16`** - Get the handle being written
- **`data() -> &[u8]`** - Get the raw data being written
- **`value<T: FromGatt>(&Characteristic<T>) -> Result<T>`** - Parse typed value
- **`accept() -> Result<Reply>`** - Process using AttributeServer (writes to storage)
- **`reject(err: AttErrorCode) -> Result<Reply>`** - Reject with error code

### ServiceBuilder Methods

- **`add_characteristic_with_handler<T, U>(uuid, props) -> CharacteristicBuilder`**
  - Add a callback-based characteristic
  - No storage buffer parameter
  - Handler logic provided in `gatt_events_task`

### Characteristic Methods (unchanged)

- **`notify(conn, value) -> Result<()>`** - Send notification (doesn't store value)
- **`indicate(conn, value) -> Result<()>`** - Send indication (doesn't store value)

## Migration from Storage-Based

### Before (Storage-Based):

```rust
// Must allocate storage buffer
let mut battery_storage = [0u8; 1];

let battery_level = service
    .add_characteristic(
        characteristic::BATTERY_LEVEL,
        &[CharacteristicProp::Read, CharacteristicProp::Notify],
        10u8,  // Initial value
        &mut battery_storage,  // Storage buffer
    )
    .build();

// In event handler - no custom logic, just gets notified
GattEvent::Read(read) => {
    // Can't perform async operations here
    read.accept()?.send().await;
}

// To update value, must write to storage first
server.set(&battery_level, &new_value)?;
battery_level.notify(conn, &new_value).await?;
```

### After (Callback-Based):

```rust
// No storage buffer needed!
let battery_level = service
    .add_characteristic_with_handler::<u8, _>(
        characteristic::BATTERY_LEVEL,
        &[CharacteristicProp::Read, CharacteristicProp::Notify],
    )
    .build();

// In event handler - full async control
GattEvent::Read(read) if read.handle() == battery_level.handle => {
    // Can perform async operations!
    let value = read_from_adc().await;
    read.respond(&[value])?.send().await;
}

// To notify, just send value directly
let value = read_from_adc().await;
battery_level.notify(conn, &value).await?;
```

## Performance Considerations

### Memory

- **Storage-based**: Memory used = all characteristics * max characteristic size
- **Callback-based**: Memory used = 0 (just metadata)

Example: 10 characteristics of 20 bytes each
- Storage-based: 200 bytes
- Callback-based: 0 bytes

### CPU

- **Storage-based**: Fast (just memory copy)
- **Callback-based**: Depends on handler (async I/O may be slower but more flexible)

### When to Use Each

**Use storage-based when:**
- Value is small and static
- Value changes infrequently
- No I/O needed to get value
- Want simplest code

**Use callback-based when:**
- Value requires I/O to compute (sensor, flash, etc.)
- Value is dynamic or computed
- Want to save memory
- Need async operations during read/write

## Advanced Patterns

### Combining Storage and Callback Characteristics

```rust
let mut service = table.add_service(Service::new(SERVICE_UUID));

// Storage-based: simple, static value
let mut device_name_storage = [0u8; 20];
let device_name = service
    .add_characteristic(
        characteristic::DEVICE_NAME,
        &[CharacteristicProp::Read],
        "MyDevice",
        &mut device_name_storage,
    )
    .build();

// Callback-based: dynamic, requires I/O
let temperature = service
    .add_characteristic_with_handler::<i16, _>(
        characteristic::TEMPERATURE,
        &[CharacteristicProp::Read, CharacteristicProp::Notify],
    )
    .build();
```

### Handling Long Reads (ReadBlob)

The `respond()` method automatically handles both `ATT_READ_REQ` and `ATT_READ_BLOB_REQ`:

```rust
GattEvent::Read(read) if read.handle() == long_value.handle => {
    // For long values, client may read in chunks
    // respond() handles this automatically
    let full_value = get_long_value().await;
    read.respond(&full_value)?.send().await;
}
```

### Error Handling

```rust
GattEvent::Read(read) if read.handle() == my_char.handle => {
    match try_read_value().await {
        Ok(value) => {
            read.respond(&value)?.send().await;
        }
        Err(_) => {
            // Return ATT error code
            read.reject(AttErrorCode::UNLIKELY_ERROR)?.send().await;
        }
    }
}
```

## Troubleshooting

### "Read returns READ_NOT_PERMITTED"

This means `accept()` was called on a callback-based characteristic. Use `respond()` instead.

### "How do I update the stored value?"

Callback-based characteristics don't have stored values. The value is generated on each read. To "update" the value, change what your handler returns.

### "Can I mix callback and storage characteristics?"

Yes! Use `add_characteristic()` for storage-based and `add_characteristic_with_handler()` for callback-based in the same service.

### "Do I need to implement traits?"

No! The handler is just your code in the `gatt_events_task` match statement. Check the handle and respond accordingly.
