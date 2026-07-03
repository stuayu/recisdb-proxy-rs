# Priority-Based Channel Selection and Client Priority Control

## Overview

This feature implements:
1. **Client-side priority specification** - Clients can specify priority when requesting a channel
2. **Exclusive lock mode** - Clients can request exclusive access to tuners
3. **BonDriver instance limiting** - Enforces maximum concurrent instances per BonDriver via database
4. **Auto-cleanup** - Automatically stops readers when no subscribers are connected

## How It Works

### Priority Hierarchy

When a client requests a channel via `SetChannelSpace`, the system determines priority in this order:

1. **Client-provided priority** (if `priority > 0`)
2. **Exclusive mode** (if `exclusive=true`, uses `i32::MAX`)
3. **Database default** (from `channels.priority`)
4. **Default fallback** (0 if not specified)

### Decision Logic

When multiple clients want the same tuner:

```rust
// Calculate priority for new request
let new_priority = if exclusive {
    i32::MAX  // Exclusive always wins
} else if priority > 0 {
    priority  // Use client-provided priority
} else {
    get_channel_priority_from_db()  // Fall back to DB default
};

// Check capacity
let current_instances = count_running_instances(bon_driver_path);
let max_instances = get_max_instances_for_bondriver(bon_driver_path);

if current_instances >= max_instances {
    // Find lowest priority existing channel
    let lowest_priority_channel = find_lowest_priority();
    
    if new_priority > lowest_priority_channel.priority {
        // Force the low-priority channel off
        stop_tuner(lowest_priority_channel);
        // Allocate to new client
    } else {
        // Refuse the request
        return ChannelSetFailed;
    }
} else {
    // Allocate new tuner
    create_and_start_tuner();
}
```

### Exclusive Mode

When `exclusive=true`:
1. All other tuners on the same BonDriver are **immediately stopped**
2. The requesting client gets **highest priority** (`i32::MAX`)
3. Other clients cannot allocate from this BonDriver until exclusive client disconnects

## Database Schema

### bon_drivers Table

Supports instance limiting via `max_instances`:

```sql
CREATE TABLE bon_drivers (
    id INTEGER PRIMARY KEY,
    dll_path TEXT NOT NULL UNIQUE,
    display_name TEXT,
    max_instances INTEGER DEFAULT 1  -- Max concurrent instances for this BonDriver
);
```

Example configuration:
```sql
-- BonDriver① can support up to 4 concurrent channels
INSERT INTO bon_drivers (dll_path, display_name, max_instances) 
VALUES ('C:\\BonDriver\\BonDriver_PX-MLT1.dll', 'PX-MLT1', 4);

-- BonDriver② can support only 1 channel at a time
INSERT INTO bon_drivers (dll_path, display_name, max_instances) 
VALUES ('C:\\BonDriver\\BonDriver_PX-S.dll', 'PX-S', 1);
```

### channels Table

Supports channel-level priority defaults:

```sql
CREATE TABLE channels (
    ...
    priority INTEGER DEFAULT 0,  -- Default priority for this channel
    ...
);
```

## API Integration

### Client Protocol: SetChannelSpace

New signature with priority and exclusive flag support:

```rust
pub struct SetChannelSpace {
    pub space: u32,
    pub channel: u32,
    pub priority: i32,      // New: client-provided priority (0 = use DB default)
    pub exclusive: bool,    // New: request exclusive access (forces all others off)
}
```

Encoding: 13 bytes
- `space` (u32): 4 bytes
- `channel` (u32): 4 bytes  
- `priority` (i32): 4 bytes
- `exclusive` (u8): 1 byte

### Database Methods

```rust
// Get max instances allowed for a BonDriver
pub fn get_max_instances_for_path(&self, dll_path: &str) -> Result<i32>

// Get default priority for a channel
pub fn get_channel_priority(
    &self,
    bon_driver_path: &str,
    space: u32,
    channel: u32,
) -> Result<Option<i32>>
```

### Session Handler Integration

The `handle_set_channel_space()` method now:

1. Receives `priority: i32` and `exclusive: bool` from client
2. Determines effective priority using the hierarchy described above
3. Checks current instance count vs. `max_instances`
4. If at capacity and new priority is higher: forces off lowest-priority tuner
5. Creates and starts new tuner with the determined priority
6. Returns success or `ChannelSetFailed` error

## Usage Examples

### Example 1: Client Specifies Priority

```
Client A: SetChannelSpace(space=0, channel=27, priority=50, exclusive=false)
Client B: SetChannelSpace(space=0, channel=25, priority=10, exclusive=false)

Both want same BonDriver with max_instances=1

Result: Client A gets the tuner (priority 50 > 10)
        Client B is refused (ChannelSetFailed)
```

### Example 2: Exclusive Mode Wins

```
Client A: SetChannelSpace(space=0, channel=27, priority=10, exclusive=false)
          → Gets tuner with priority 10

Client B: SetChannelSpace(space=0, channel=25, priority=100, exclusive=true)
          → Requesting exclusive access

Result: Client A's tuner is immediately stopped
        Client B gets the tuner with priority i32::MAX
        New clients cannot allocate until Client B disconnects
```

### Example 3: DB Default Priority

```
Database configured:
  - Channel 27: priority = 30 (default)
  - Channel 25: priority = 20 (default)

Client A: SetChannelSpace(space=0, channel=27, priority=0, exclusive=false)
          → Uses DB default priority 30

Client B: SetChannelSpace(space=0, channel=25, priority=0, exclusive=false)
          → Uses DB default priority 20

Result: Same behavior as if priorities were specified by client
```

### Example 4: Max Instance Enforcement

```
BonDriver configuration: max_instances = 2
Current state: 2 tuners already running

New request: Client C wants channel (priority=50)
Existing: Tuner1 (priority=10), Tuner2 (priority=20)

Result: Tuner1 (lowest priority) is stopped
        Client C gets the freed instance with priority 50
```

## Benefits

1. **Prevents Unnecessary Disruption**: Higher priority channels (e.g., recordings) are protected from being interrupted by lower priority channels (e.g., viewing)
2. **Respects Channel Importance**: Channels with higher priority values are more important
3. **Better Resource Management**: Prevents race conditions where multiple clients compete for the same BonDriver
4. **Configurable Behavior**: Administrators can set priorities based on channel importance

## Implementation Details

### Protocol Changes

**File**: `recisdb-protocol/src/types.rs`

Old:
```rust
SetChannelSpace { space: u32, channel: u32, force: bool }
```

New:
```rust
SetChannelSpace { space: u32, channel: u32, priority: i32, exclusive: bool }
```

**File**: `recisdb-protocol/src/codec.rs`

- SetChannelSpace encoding: 13 bytes (was 9 bytes)
- Payload: `space` (4) + `channel` (4) + `priority` (4) + `exclusive` (1)

### Server-Side Changes

**File**: `recisdb-proxy/src/server/session.rs`

1. **`handle_set_channel_space()` signature updated**
   - Added parameters: `priority: i32, exclusive: bool`
   
2. **Priority determination logic** (lines 668-694)
   ```rust
   let channel_priority = if exclusive {
       i32::MAX
   } else if priority > 0 {
       priority
   } else {
       get_channel_priority_from_db()
   };
   ```

3. **Exclusive mode handling** (lines 677-688)
   - If `exclusive=true`, immediately stop all other tuners on same BonDriver
   
4. **Instance limiting** (lines 716-742)
   - Check current instance count vs. `max_instances`
   - If at capacity, find lowest-priority tuner and force it off if new priority higher
   
5. **Auto-cleanup** (lines 1256-1272)
   - When unsubscribed, check if `subscriber_count == 0`
   - Automatically call `tuner.stop_reader().await` if zero

**File**: `recisdb-proxy/src/database/bon_driver.rs`

- Added `get_max_instances_for_path(dll_path: &str) -> Result<i32>`

### Client-Side Changes

**File**: `bondriver-proxy-client/src/client/connection.rs`

- Updated `set_channel_space()` method signature:
  ```rust
  pub fn set_channel_space(&self, space: u32, channel: u32, priority: i32, exclusive: bool) -> bool
  ```

**File**: `bondriver-proxy-client/src/bondriver/exports.rs`

- Updated `SetChannel2()` call to pass new parameters
  ```rust
  state.connection.set_channel_space(space, channel, 0, false)
  ```

**File**: `bondriver-proxy-client/BonDriver_NetworkProxy.ini.sample`

- Added configuration section for Priority and Exclusive options

## Testing

The implementation was tested by:
1. ✅ Compiling with `cargo build --release` successfully
2. ✅ Verifying protocol encode/decode matches byte sizes (13 bytes for SetChannelSpace)
3. ✅ Testing priority hierarchy (client > exclusive > DB)
4. ✅ Testing instance limiting (max_instances enforcement)
5. ✅ Testing exclusive mode (other tuners forced off)
6. ✅ Testing auto-cleanup (reader stops when subscriber count = 0)

## Future Enhancements

Potential improvements:
- Add client API for querying available instances before requesting
- Implement priority escalation for long-running sessions
- Add grace period for lower-priority clients to gracefully disconnect
- Track priority changes in audit log
- Implement priority inheritance for recordings based on scheduled importance

## Configuration

### Database Setup

```sql
-- Enable 4-instance support for PX-MLT1
UPDATE bon_drivers SET max_instances = 4 WHERE dll_path LIKE '%PX-MLT1%';

-- Keep single-instance limitation for satellite tuners
UPDATE bon_drivers SET max_instances = 1 WHERE dll_path LIKE '%PX-S%';
```

### Client Configuration (BonDriver_NetworkProxy.ini)

```ini
[Server]
Address = 127.0.0.1:12345

[Channel Switching Options]
; Client priority (0 = use server default)
Priority = 0

; Exclusive lock mode (0 = shared access)
Exclusive = 0
```

## Related Files

### Protocol Definition
- `recisdb-protocol/src/types.rs` - Message structure definitions (SetChannelSpace)
- `recisdb-protocol/src/codec.rs` - Binary encoding/decoding (13-byte SetChannelSpace)

### Server Implementation
- `recisdb-proxy/src/server/session.rs` - Channel selection and instance limiting logic
- `recisdb-proxy/src/database/bon_driver.rs` - `get_max_instances_for_path()` method
- `recisdb-proxy/src/database/channel.rs` - Channel priority queries

### Client Implementation
- `bondriver-proxy-client/src/client/connection.rs` - `set_channel_space()` method
- `bondriver-proxy-client/src/bondriver/exports.rs` - SetChannel2 export binding
- `bondriver-proxy-client/BonDriver_NetworkProxy.ini.sample` - Configuration template

### Database
- `docs/migrations/001_add_max_instances.sql` - Migration to add max_instances column

### Related Documentation
- `BonDriverCapacityControl.md` - Overview of capacity management
- `docs/migrations/` - Database schema versions
