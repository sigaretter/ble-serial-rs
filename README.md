# ble-serial-rs
BLE to virtual serial port bridge — primarily for WeAct CH9143 BLE/UART/USB

## CH9143 specifics

The WeAct CH9143 exposes a single GATT characteristic for both directions:

- **Service UUID**: `0000ffe0-0000-1000-8000-00805f9b34fb`
- **Characteristic UUID**: `0000ffe1-0000-1000-8000-00805f9b34fb`
  - Properties: `NOTIFY` + `WRITE_WITHOUT_RESPONSE`
  - Same UUID used for both RX (subscribe notify) and TX (write without response)

On macOS the device address is a CoreBluetooth UUID (e.g. `E687FC6B-4B89-50BB-8077-6C11D1739BA7`). On Linux it is a BDAddr MAC (`AA:BB:CC:DD:EE:FF`).

## How the bridge works

```
BLE device                     ble-serial                   host app
  (CH9143)
    │                              │                            │
    │── notify ──────────────────► │ pty.write_sender().send()  │
    │                              │ writer thread              │
    │                              │ ─► write to PTY master fd  │
    │                              │    slave fd ◄─── symlink ──┤ /tmp/ttyBLE
    │                              │                            │ open("/tmp/ttyBLE")
    │                              │ reader thread              │
    │                              │ ◄─ read from PTY master fd │
    │◄── write-without-response ── │ ◄── PTY→BLE tokio task ◄── │
```

1. `openpty()` — creates master/slave fd pair; slave path is symlinked to `/tmp/ttyBLE`
2. Master fd set to raw mode (`cfmakeraw`) so byte values pass through unchanged
3. Reader thread: blocking `File::read` on a `dup`'d master fd → `tokio::mpsc` channel → BLE write task
4. Writer thread: `std::sync::mpsc` channel ← BLE notify callback → `File::write_all` to `dup`'d master fd
5. Shutdown: `Ctrl-C` → `tokio::select!` exits → `pty.remove()` → BLE disconnect

## Build & run

```bash
cd ble-serial-rs
cargo build --release          # binary at target/release/ble-serial

# Interactive scan, then connect:
./target/release/ble-serial

# Connect directly by CoreBluetooth UUID (macOS) or BDAddr (Linux):
./target/release/ble-serial -d E687FC6B-4B89-50BB-8077-6C11D1739BA7

# Show debug logs (discovered characteristics, data flow):
./target/release/ble-serial -d <UUID> -v
```
