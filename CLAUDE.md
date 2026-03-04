# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Development Commands

All development commands must be run inside the Nix development shell:

```bash
# Enter development shell first
nix develop

# Build the project
cargo build
cargo build --release

# Run tests
cargo test

# Run the NixOS VM integration test
nix build .#canbridge-test
```

## Architecture Overview

This is a CAN-TCP bridge application that forwards CAN bus frames bidirectionally over TCP connections. The architecture handles a critical challenge with CAN sockets on Linux.

### Core Problem: Loopback Handling

The codebase uses a channel-based deduplication system to handle Linux kernel CAN loopback:

- **Why it's needed**: We use separate read/write sockets on the same interface. When the write socket sends a frame, kernel loopback delivers it to the read socket too, which would create infinite loops.
- **The solution**: Channel-based communication where the writer notifies the reader about sent frames via a channel, and the reader filters out these looped-back frames.
- **Key insight**: Simply disabling `RECV_OWN_MSGS` doesn't work because it only affects frames a socket sends to itself, not frames from other sockets on the same interface.
- **Memory management**: Uses timestamp-based expiry (100ms threshold) to safely remove old entries without race conditions

### Key Components

1. **Wire Protocol** (src/main.rs:46-111)
   - Custom binary format using postcard serialization
   - Frame format with version, flags, CAN ID, and data
   - Length-prefixed messages (u16 big-endian)

2. **Bidirectional Forwarding** (src/main.rs:263-347)
   - Separate threads for CAN→TCP and TCP→CAN directions
   - Each direction uses its own CAN socket
   - Frame deduplication using hash-based tracking

3. **Operating Modes**
   - **Server mode**: Listens on TCP, accepts connections
   - **Client mode**: Connects to TCP server, auto-reconnects

4. **CAN FD Support**
   - Handles both standard CAN and CAN FD frames
   - Socket configuration for FD frames (lines 207-227)

### NixOS Module Integration

The project includes a complete NixOS module (nix/modules/canbridge.nix) that:
- Configures systemd services for server/client modes
- Sets up virtual CAN interfaces for testing
- Manages multiple CAN interfaces with separate TCP ports

### Testing

Integration tests use NixOS VMs (nix/tests/default.nix) to verify bidirectional frame forwarding between client and server nodes with multiple CAN interfaces.