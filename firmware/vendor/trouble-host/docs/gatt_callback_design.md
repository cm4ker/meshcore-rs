# GATT Callback-Based Architecture Design

## Overview

This document describes the new callback-based GATT architecture for trouble-host, replacing the current storage-based approach.

## Core Principles

1. **No value storage in AttributeTable** - Characteristic values are NOT stored in the attribute table
2. **Async dispatch functions** - Each characteristic has associated async callback functions for read/write operations
3. **Per-connection CCCD state** - Notification/indication subscriptions are maintained per-connection
4. **Lifetime elimination** - Remove `'values` lifetime from `AttributeTable` and `AttributeServer`
5. **Type safety** - Callbacks can be type-safe through generics or trait implementations

## Architecture Components

### 1. Attribute Definition

Attributes now contain:
- UUID
- Handle
- Properties (read, write, notify, indicate)
- **Callback references** instead of data storage

```rust
pub(crate) enum AttributeData {
    Service {
        uuid: Uuid,
    },
    Characteristic {
        props: CharacteristicProps,
        handler: &'static dyn CharacteristicHandler,
    },
    Declaration {
        props: CharacteristicProps,
        handle: u16,
        uuid: Uuid,
    },
    Cccd {
        // CCCD values are stored in CccdTables, not here
        // This is just a placeholder for the CCCD descriptor itself
    },
}
```

### 2. Handler Traits

Async traits for handling GATT operations:

```rust
/// Handler for characteristic read operations
pub trait ReadHandler {
    /// Handle a read request
    /// Returns the data to send back, or an error code
    async fn on_read(&self, connection: &Connection<'_, impl PacketPool>, offset: usize, data: &mut [u8]) -> Result<usize, AttErrorCode>;
}

/// Handler for characteristic write operations
pub trait WriteHandler {
    /// Handle a write request
    async fn on_write(&mut self, connection: &Connection<'_, impl PacketPool>, offset: usize, data: &[u8]) -> Result<(), AttErrorCode>;
}

/// Combined handler trait
pub trait CharacteristicHandler {
    /// Called when characteristic is read
    fn on_read(&self, connection: &Connection<'_, impl PacketPool>, offset: usize, data: &mut [u8]) -> impl Future<Output = Result<usize, AttErrorCode>>;

    /// Called when characteristic is written
    fn on_write(&self, connection: &Connection<'_, impl PacketPool>, offset: usize, data: &[u8]) -> impl Future<Output = Result<(), AttErrorCode>>;
}
```

### 3. Callback Dispatch Flow

```
Client Request (ATT PDU)
    ↓
AttributeServer::process()
    ↓
Find attribute by handle
    ↓
Extract handler from AttributeData
    ↓
Invoke handler.on_read() or handler.on_write()
    ↓ (async)
Handler executes user logic
    ↓
Return result
    ↓
AttributeServer formats ATT response
    ↓
Send response to client
```

### 4. Notification/Indication Flow

```rust
// User code triggers notification
characteristic.notify(connection, &value).await?;
    ↓
// Check CCCD state for this connection
if server.should_notify(connection, cccd_handle) {
    ↓
    // Format and send ATT notification PDU
    send_notification(connection, handle, value).await
}
```

### 5. CCCD State Management

- `CccdTables` remains largely unchanged - stores per-connection subscription state
- CCCD reads: Return current subscription state for the connection
- CCCD writes: Update subscription state for the connection
- `should_notify()` / `should_indicate()`: Query subscription state

### 6. Macro-Generated Code

The `#[gatt_server]` macro generates:

```rust
// Before (storage-based):
#[characteristic(uuid = "...", read, write, notify)]
level: u8,

// After (callback-based):
#[characteristic(uuid = "...", read, write, notify)]
level: CharacteristicHandle<u8>,

// Generated handler implementation:
struct LevelHandler {
    // User can add fields here for state
}

impl CharacteristicHandler for LevelHandler {
    async fn on_read(&self, conn: &Connection, offset: usize, data: &mut [u8]) -> Result<usize, AttErrorCode> {
        // User implements this
        todo!()
    }

    async fn on_write(&self, conn: &Connection, offset: usize, data: &[u8]) -> Result<(), AttErrorCode> {
        // User implements this
        todo!()
    }
}
```

## Key Changes from Current Architecture

### AttributeTable
- **Remove**: `'values` lifetime parameter
- **Remove**: Storage buffers (`&'d mut [u8]`)
- **Remove**: `AttributeData::Data` and `AttributeData::ReadOnlyData`
- **Add**: Callback/handler references in `AttributeData::Characteristic`

### AttributeServer
- **Remove**: `'values` lifetime parameter
- **Change**: `process()` becomes async to await handler callbacks
- **Change**: Read/write handlers invoke callbacks instead of accessing storage
- **Keep**: CCCD management (mostly unchanged)

### GattEvent Processing
- **Change**: Event processing becomes async
- **Change**: `accept()` awaits callback execution
- **Keep**: Event structure (Read/Write/Other) for user interception

## Benefits

1. **No storage overhead** - No need to allocate buffers for characteristic values
2. **Async operations** - Handlers can perform I/O (flash writes, sensor reads, etc.)
3. **Flexible state management** - User controls where/how values are stored
4. **Simplified lifetimes** - Eliminates complex `'values` lifetime
5. **Type safety** - Can use generics for type-safe handlers

## Migration Path

Users need to:
1. Change characteristic fields from values to handles
2. Implement handler traits for each characteristic
3. Remove storage buffer allocations
4. Update read/write logic to use async handlers

## Open Questions

1. Should handlers be trait objects (`&dyn`) or generic parameters?
   - Trait objects: More flexible, slight overhead
   - Generics: Zero-cost, but more complex types

2. Should user code still see `GattEvent`s or should they be hidden?
   - Visible: Allows user to intercept/reject before handler
   - Hidden: Simpler API, handler is mandatory

3. How to handle descriptor reads/writes?
   - Same callback system as characteristics
   - Or special-case common descriptors?

4. Should we provide default handler implementations?
   - For simple in-memory storage
   - For read-only constants
