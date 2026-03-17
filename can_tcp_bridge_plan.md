# CAN FD bi-directional bridge over TCP (Rust) — Implementation Plan

This document outlines a simple, portable, blocking (non-async) plan to bridge **SocketCAN** interfaces over **TCP** with **CAN FD** support and bi-directional communication:

**Computer A ⇄ vcan0 ⇄ TCP ⇄ Computer B ⇄ can0**

---

## Goals

- Read frames from a local SocketCAN interface (`vcan0` or `can0`)
- Forward them to a remote machine over TCP
- Receive remote frames over TCP and write them to the local SocketCAN interface
- Support:
  - **CAN FD** (0–64 byte payloads)
  - **Extended IDs** (29-bit), **RTR**, **Error frames** reception (as feasible)
  - **Bi-directional** traffic simultaneously
- **Portable across architectures/endian** (no raw kernel struct blobs over TCP)
- **Blocking I/O only** (no tokio/async)

---

## High-level Architecture

### One binary, two modes
Implement a single binary `canbridge` with two modes:

- **Client mode**: connects to a remote address  
  `canbridge --mode connect --addr <host:port> --iface vcan0`
- **Server mode**: listens for one incoming connection  
  `canbridge --mode listen --addr <0.0.0.0:port> --iface can0`

(For an ultra-minimal version you can hardcode values, but this plan assumes a light CLI for convenience.)

### Concurrency model (blocking)
For each established TCP connection, run two blocking loops in parallel:

1. **CAN → TCP thread**
   - `read_frame()` from SocketCAN (blocking)
   - serialize into a portable wire message
   - write to TCP stream (blocking)

2. **TCP → CAN thread**
   - read framed messages from TCP stream (blocking)
   - deserialize into a logical CAN frame representation
   - `write_frame()` to SocketCAN (blocking)

This is the simplest robust model for bi-directional operation without async.

---

## Dependencies / Crates

Recommended crates:

- `socketcan` — high-level access to SocketCAN frames (`CANSocket`, `CANFrame`)
- `serde` — derive serialization
- `postcard` — compact, portable binary serialization
- `libc` — only for socket options (`setsockopt`) to enable CAN FD and error frame reception

Optional:
- `anyhow` or `thiserror` for error handling
- `clap` for CLI parsing

---

## Wire Format

### Why a wire format?
Raw `recv()` bytes from `struct canfd_frame` are **not portable** across architectures and endianness.  
Instead, send a *logical* representation (ID/flags/data) using a defined encoding.

### TCP framing requirement
TCP is a byte stream; message boundaries are not preserved.  
Therefore, each message must be **framed**.

**Chosen framing:** `u16` big-endian length prefix.

#### On the wire
Each record on the TCP stream:

1. `len_be: u16` — payload length in bytes (big-endian)
2. `payload: [u8; len]` — postcard-serialized `WireFrameV1`

### `WireFrameV1` schema

Use fixed-width integers for predictable layout / easier interop:

```rust
use serde::{Serialize, Deserialize};
use postcard::fixint::FixintBE;

#[derive(Serialize, Deserialize)]
struct WireFrameV1 {
    version: u8,              // always 1 for this format
    flags: u8,                // bitfield (see below)
    can_id: FixintBE<u32>,    // 4-byte big-endian integer
    data: Vec<u8>,            // 0..=64
}
```

#### Flags bitfield
- bit 0: `EFF` — extended (29-bit) ID
- bit 1: `RTR` — remote transmission request
- bit 2: `ERR` — error frame indicator (received from kernel if enabled)
- bit 3: `FD`  — CAN FD frame
- bit 4: `BRS` — bit-rate switch (if available from your API path)
- bit 5: `ESI` — error-state indicator (if available)
- bit 6..7: reserved

**Note:** For best OS-independence, keep `can_id` as the numeric identifier (11/29-bit depending on `EFF`).
Avoid embedding Linux-specific `CAN_EFF_FLAG/CAN_ERR_FLAG` bits into the wire `can_id`.

---

## SocketCAN Configuration

### Enable CAN FD on the socket
Even if the interface supports CAN FD, the raw socket may need FD enabled:

- `setsockopt(fd, SOL_CAN_RAW, CAN_RAW_FD_FRAMES, 1)`

### Enable error frame reception
By default, error frames may be filtered out. Enable:

- `setsockopt(fd, SOL_CAN_RAW, CAN_RAW_ERR_FILTER, CAN_ERR_MASK)`

You can do this via `libc` using `sock.as_raw_fd()`.

### Notes about error frames
- **Receiving** error frames: possible with `CAN_RAW_ERR_FILTER`
- **Forwarding** them over TCP: yes (the wire format supports `ERR` + payload)
- **Re-injecting** error frames onto another interface:
  - Some APIs or drivers may reject “constructing” error frames.
  - If `socketcan::CANFrame::new/new_fd` rejects error-frame IDs, you may:
    - drop them on transmit, or
    - add a small fallback for error-frame transmit via `libc::send()` (hybrid approach)

---

## Implementation Steps

### Step 1 — Define wire struct + framing helpers
- Define `WireFrameV1` with `FixintBE<u32>`
- Implement:
  - `send_wire(stream, &WireFrameV1)`:
    - `postcard::to_stdvec()`
    - write `u16::to_be_bytes(len)`
    - `write_all(payload)`
  - `recv_wire(stream) -> WireFrameV1`:
    - `read_exact(2)` length
    - `read_exact(len)` payload
    - `postcard::from_bytes(payload)`

Add bounds:
- Reject lengths > e.g. 2048 bytes (defensive)
- Reject `data.len() > 64`

### Step 2 — CAN → wire conversion
From `socketcan::CANFrame`:
- Extract:
  - `id() -> u32`
  - `data() -> &[u8]`
  - `is_extended()`, `is_rtr()`, `is_error()`, `is_fd()` (and BRS/ESI if exposed)
- Convert to `WireFrameV1 { version: 1, flags, can_id, data }`

### Step 3 — wire → CAN conversion
From `WireFrameV1`:
- Determine `is_fd`, `is_ext`, `is_rtr`
- Construct frame:
  - if FD: `CANFrame::new_fd(can_id, &data, is_rtr, is_ext)`
  - else: `CANFrame::new(can_id, &data, is_rtr, is_ext)`
- Write using `CANSocket::write_frame()`

Handle constructor errors:
- If error frame and constructor rejects, decide to drop/log or use hybrid `libc` send for ERR frames.

### Step 4 — Connection handling and threads
- For a given connection:
  - clone `TcpStream` (one handle for each direction)
  - open CAN socket(s) (two sockets is simplest; or share one with a mutex)
  - spawn `CAN→TCP` thread
  - run `TCP→CAN` loop in main thread (or spawn both and join)

### Step 5 — Reconnect / accept loop (recommended)
Even if “simple”, you likely want:
- server: accept next connection after disconnect
- client: retry connect after disconnect with a short sleep

---

## Testing Plan

### Local vcan sanity test (single machine)
1. Create `vcan0`:
   ```bash
   sudo modprobe vcan
   sudo ip link add dev vcan0 type vcan
   sudo ip link set up vcan0
   ```
2. Run server instance (listen) and client instance (connect) on the same machine using different ports, or use `localhost`.

3. Use `can-utils`:
   - Send:
     ```bash
     cansend vcan0 123#11223344
     ```
   - Monitor:
     ```bash
     candump vcan0
     ```

### Two-machine test (A=vcan0, B=can0)
- Machine B:
  - Bring up `can0` (example bitrate):
    ```bash
    sudo ip link set can0 up type can bitrate 500000
    ```
  - Run server: `--iface can0 --listen 0.0.0.0:5000`

- Machine A:
  - Bring up `vcan0` (as above)
  - Run client: `--iface vcan0 --connect <B_IP>:5000`

- Validate:
  - send on A (`cansend vcan0 ...`) → observe on B (`candump can0`)
  - send on B (`cansend can0 ...`) → observe on A (`candump vcan0`)

### CAN FD test
- Send a CAN FD frame (tooling varies; some `cansend` support `--fd` or interface-specific syntax).
- Verify payload lengths up to 64 bytes traverse and show correctly.

---

## Production Hardening Checklist (optional)

- Add authentication (TLS tunnel, VPN, or mTLS)
- Add message size caps & validation
- Add backpressure strategy (bounded channel or drop policy) if TCP stalls
- Add clean shutdown (signal handling)
- Add metrics (counts, drops, reconnects)
- Consider sequence numbers for diagnostics (optional)

---

## Summary

- **CAN sockets are message-oriented** (one read returns one frame), but **TCP is a byte stream** (requires framing).
- For cross-architecture portability, do **not** send raw `canfd_frame` bytes.
- Use `socketcan` to work with logical frame fields.
- Serialize via **postcard** with **fixed-width integers** and **u16 length-prefix framing**.
- Use two blocking loops (threads) for simple, correct bi-directional bridging.
