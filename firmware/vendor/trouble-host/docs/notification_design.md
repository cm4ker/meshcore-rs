# Notification/Indication State Management Design

## Overview

This document describes how notification and indication state management will work in the callback-based GATT architecture.

## Current Architecture (To Preserve)

The current CCCD (Client Characteristic Configuration Descriptor) management is well-designed and should be largely preserved:

### Current Components

1. **`CccdTable<ENTRIES>`** - Stores CCCD values (notify/indicate flags) for a single connection
   - Array of `(handle, CCCD)` pairs
   - Methods: `should_notify()`, `should_indicate()`, `set_notify()`, `set_indicate()`

2. **`CccdTables<M, CCCD_MAX, CONN_MAX>`** - Manages CCCD tables for multiple connections
   - One `CccdTable` per connection (up to `CONN_MAX` connections)
   - Indexed by connection identity (`Identity`)
   - Handles connect/disconnect lifecycle

3. **`AttributeServer`** - Owns the `CccdTables` and provides access methods
   - `should_notify(connection, cccd_handle)` - Check if notifications enabled
   - `should_indicate(connection, cccd_handle)` - Check if indications enabled

## Integration with Callback Architecture

### Key Principle: CCCD State Remains Separate from Handlers

- **CCCD values are NOT stored in attribute storage** (current design ✓)
- **CCCD values are NOT managed by handlers** (keep existing approach ✓)
- **CccdTables remains in AttributeServer** (no change needed ✓)

### CCCD Read/Write Flow

#### CCCD Write (Client subscribes/unsubscribes)

```
Client writes to CCCD descriptor
    ↓
AttributeServer::handle_write_req()
    ↓
Identifies AttributeData::Cccd
    ↓
Parses CCCD value (notify bit, indicate bit)
    ↓
Updates CccdTables for this connection
    ↓
Returns success response
```

**No handler needed** - CCCD writes are handled specially by AttributeServer.

#### CCCD Read (Client queries subscription state)

```
Client reads CCCD descriptor
    ↓
AttributeServer::handle_read_req()
    ↓
Identifies AttributeData::Cccd
    ↓
Queries CccdTables for this connection's state
    ↓
Returns current CCCD value
```

**No handler needed** - CCCD reads are handled specially by AttributeServer.

### Notification/Indication Trigger Flow

```rust
// User code (e.g., in a periodic task)
let value = read_sensor().await;
characteristic.notify(&connection, &value).await?;
```

Internally:
```
characteristic.notify(connection, value)
    ↓
Get CCCD handle from characteristic
    ↓
Check: server.should_notify(connection, cccd_handle)?
    ↓
If subscribed:
    ↓
    Format ATT_HANDLE_VALUE_NTF PDU
    ↓
    Send to connection
```

## Changes Needed for Callback Architecture

### 1. AttributeData Changes

**Current:**
```rust
enum AttributeData<'d> {
    // ... other variants
    Cccd {
        notifications: bool,
        indications: bool,
    },
}
```

**After (callback-based):**
```rust
enum AttributeData {
    // ... other variants
    Cccd,  // Just a marker, actual state in CccdTables
}
```

The `notifications` and `indications` fields in the `AttributeData::Cccd` variant are redundant because:
- They were just temporary state during read/write operations
- Real per-connection state is in `CccdTables`
- They were written to then immediately read from `CccdTables`

### 2. CCCD Handling in AttributeServer

**Current approach (attribute_server.rs:392-408):**
```rust
fn read_attribute_data(&self, connection, offset, att, data) {
    if let AttributeData::Cccd { .. } = att.data {
        // Get value from CccdTables
        if let Some(value) = self.cccd_tables.get_value(&connection.peer_identity(), att.handle) {
            // Write back into att.data temporarily
            let _ = att.write(0, value.as_slice());
        }
    }
    att.read(offset, data)  // Then read from att.data
}
```

**New approach (no temporary storage):**
```rust
fn read_attribute_data(&self, connection, offset, att, data) -> Result<usize, AttErrorCode> {
    match &att.data {
        AttributeData::Cccd => {
            // Directly return CCCD value for this connection
            if let Some(value) = self.cccd_tables.get_value(&connection.peer_identity(), att.handle) {
                if offset > 0 {
                    return Err(AttErrorCode::INVALID_OFFSET);
                }
                data[..2].copy_from_slice(&value);
                Ok(2)
            } else {
                // Connection not found or CCCD not set
                data[0] = 0;
                data[1] = 0;
                Ok(2)
            }
        }
        AttributeData::Characteristic { handler, .. } => {
            // Invoke handler
            handler.on_read(connection, offset, data).await
        }
        // ... other variants
    }
}
```

### 3. No Handler Needed for CCCD

CCCD descriptors do NOT use the handler system because:
1. CCCD behavior is standardized by Bluetooth spec
2. State is managed per-connection in `CccdTables`
3. Special handling is required (checking bits, updating tables)

Therefore:
- `AttributeData::Cccd` remains a special case
- No `CharacteristicHandler` for CCCD
- AttributeServer handles CCCD read/write directly

## Notification/Indication API

### Current API (Preserved)
```rust
impl<T: FromGatt> Characteristic<T> {
    pub async fn notify<P: PacketPool>(
        &self,
        connection: &GattConnection<'_, '_, P>,
        value: &T,
    ) -> Result<(), Error> {
        let value = value.as_gatt();
        let server = connection.server;

        // Don't need to update server storage in new architecture
        // server.set(self.handle, value)?;  // <- Remove this

        let cccd_handle = self.cccd_handle.ok_or(Error::NotFound)?;
        let connection = connection.raw();

        if !server.should_notify(connection, cccd_handle) {
            return Ok(());  // Not subscribed, silently succeed
        }

        let uns = AttUns::Notify {
            handle: self.handle,
            data: value,
        };
        let pdu = gatt::assemble(connection, AttServer::Unsolicited(uns))?;
        connection.send(pdu).await;
        Ok(())
    }
}
```

Key change: **Remove `server.set()` call** since we no longer store values in the attribute table.

## Summary of Changes

### ✅ Keep (No changes needed)
- `CccdTable` structure
- `CccdTables` structure
- `should_notify()` / `should_indicate()` methods
- Per-connection CCCD state management
- Connect/disconnect lifecycle

### 🔄 Modify
- `AttributeData::Cccd` - Remove `notifications` and `indications` fields, make it a unit variant
- CCCD read/write in `AttributeServer` - Read directly from `CccdTables`, write directly to `CccdTables`
- `Characteristic::notify()` - Remove `server.set()` call
- `Characteristic::indicate()` - Remove `server.set()` call

### ❌ Don't Add
- No handler trait for CCCD
- No async callbacks for CCCD
- No user-customizable CCCD behavior

## Benefits

1. **Separation of concerns** - CCCD state management is separate from characteristic handlers
2. **Per-connection state** - Each connection has independent subscription preferences
3. **Spec compliance** - CCCD behavior follows Bluetooth spec exactly
4. **Simplicity** - No need for users to implement CCCD handlers
5. **Efficiency** - No unnecessary storage or copying of CCCD values
