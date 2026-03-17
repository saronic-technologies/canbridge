// Re-export public items from main module for testing
// The actual implementation is in main.rs

pub use anyhow::{anyhow, Context, Result};
pub use serde::{Deserialize, Serialize};
pub use socketcan::{
    CanAnyFrame, CanDataFrame, CanFdFrame, CanRemoteFrame, EmbeddedFrame, ExtendedId, Id,
    StandardId,
};
use std::io::{Read, Write};

// Wire format flags
pub const FLAG_EFF: u8 = 0x01; // Extended frame format (29-bit ID)
pub const FLAG_RTR: u8 = 0x02; // Remote transmission request
pub const FLAG_ERR: u8 = 0x04; // Error frame
pub const FLAG_FD: u8 = 0x08; // CAN FD frame
pub const FLAG_BRS: u8 = 0x10; // Bit rate switch
pub const FLAG_ESI: u8 = 0x20; // Error state indicator

/// Wire format for CAN frames - portable across architectures
#[derive(Serialize, Deserialize, Debug, PartialEq)]
pub struct WireFrameV1 {
    pub version: u8,
    pub flags: u8,
    pub can_id: u32,
    pub data: Vec<u8>,
}

impl WireFrameV1 {
    pub fn new(can_id: u32, data: &[u8], flags: u8) -> Self {
        Self {
            version: 1,
            flags,
            can_id,
            data: data.to_vec(),
        }
    }
}

/// Send a wire frame over TCP with u16 length prefix (big-endian)
pub fn send_wire<W: Write>(stream: &mut W, frame: &WireFrameV1) -> Result<()> {
    let payload = postcard::to_stdvec(frame).context("Failed to serialize frame")?;

    if payload.len() > 2048 {
        return Err(anyhow!("Payload too large: {} bytes", payload.len()));
    }

    let len = payload.len() as u16;
    stream
        .write_all(&len.to_be_bytes())
        .context("Failed to write length")?;
    stream
        .write_all(&payload)
        .context("Failed to write payload")?;
    stream.flush().context("Failed to flush stream")?;

    Ok(())
}

/// Receive a wire frame from TCP with u16 length prefix (big-endian)
pub fn recv_wire<R: Read>(stream: &mut R) -> Result<WireFrameV1> {
    let mut len_buf = [0u8; 2];
    stream
        .read_exact(&mut len_buf)
        .context("Failed to read length")?;
    let len = u16::from_be_bytes(len_buf) as usize;

    if len > 2048 {
        return Err(anyhow!("Payload too large: {} bytes", len));
    }

    let mut payload = vec![0u8; len];
    stream
        .read_exact(&mut payload)
        .context("Failed to read payload")?;

    let frame: WireFrameV1 =
        postcard::from_bytes(&payload).context("Failed to deserialize frame")?;

    if frame.data.len() > 64 {
        return Err(anyhow!("Data too large: {} bytes", frame.data.len()));
    }

    Ok(frame)
}

/// Convert a SocketCAN frame to wire format
pub fn can_to_wire(frame: &CanAnyFrame) -> WireFrameV1 {
    match frame {
        CanAnyFrame::Normal(f) => {
            let mut flags = 0u8;
            let can_id = match f.id() {
                Id::Standard(id) => id.as_raw() as u32,
                Id::Extended(id) => {
                    flags |= FLAG_EFF;
                    id.as_raw()
                }
            };
            if f.is_remote_frame() {
                flags |= FLAG_RTR;
            }
            WireFrameV1::new(can_id, f.data(), flags)
        }
        CanAnyFrame::Fd(f) => {
            let mut flags = FLAG_FD;
            let can_id = match f.id() {
                Id::Standard(id) => id.as_raw() as u32,
                Id::Extended(id) => {
                    flags |= FLAG_EFF;
                    id.as_raw()
                }
            };
            WireFrameV1::new(can_id, f.data(), flags)
        }
        CanAnyFrame::Remote(f) => {
            let mut flags = FLAG_RTR;
            let can_id = match f.id() {
                Id::Standard(id) => id.as_raw() as u32,
                Id::Extended(id) => {
                    flags |= FLAG_EFF;
                    id.as_raw()
                }
            };
            WireFrameV1::new(can_id, &[], flags)
        }
        CanAnyFrame::Error(f) => {
            let flags = FLAG_ERR;
            // Error frames have a special ID format
            WireFrameV1::new(0, f.data(), flags)
        }
    }
}

/// Convert wire format to a SocketCAN frame
pub fn wire_to_can(wire: &WireFrameV1) -> Result<CanAnyFrame> {
    let is_extended = (wire.flags & FLAG_EFF) != 0;
    let is_rtr = (wire.flags & FLAG_RTR) != 0;
    let is_fd = (wire.flags & FLAG_FD) != 0;
    let is_err = (wire.flags & FLAG_ERR) != 0;

    // Skip error frames - they typically can't be written to the bus
    if is_err {
        return Err(anyhow!("Cannot transmit error frames"));
    }

    let id: Id = if is_extended {
        Id::Extended(
            ExtendedId::new(wire.can_id).ok_or_else(|| anyhow!("Invalid extended ID"))?,
        )
    } else {
        Id::Standard(
            StandardId::new(wire.can_id as u16)
                .ok_or_else(|| anyhow!("Invalid standard ID"))?,
        )
    };

    if is_fd {
        // CAN FD frame
        let frame =
            CanFdFrame::new(id, &wire.data).context("Failed to create CAN FD data frame")?;
        Ok(CanAnyFrame::Fd(frame))
    } else if is_rtr {
        // Remote frame
        let frame = CanRemoteFrame::new_remote(id, wire.data.len())
            .context("Failed to create remote frame")?;
        Ok(CanAnyFrame::Remote(frame))
    } else {
        // Standard data frame
        let frame =
            CanDataFrame::new(id, &wire.data).context("Failed to create data frame")?;
        Ok(CanAnyFrame::Normal(frame))
    }
}

/// Create a simple hash of a frame for deduplication.
///
/// ## Why deduplication is needed:
///
/// When using two separate sockets on the same CAN interface:
/// - `can_read`: for receiving frames from the CAN bus
/// - `can_write`: for sending frames to the CAN bus
///
/// When `can_write` sends a frame, the Linux kernel's loopback mechanism delivers
/// it to ALL sockets on that interface, including the `can_read` socket. This would
/// create an infinite loop: frame sent → looped back → forwarded to TCP → sent again.
///
/// To prevent this, a channel-based deduplication system can be used:
/// 1. Before `can_write` sends a frame, it sends the frame's hash via channel
/// 2. `can_read` maintains a HashSet of these hashes
/// 3. When `can_read` receives a frame, it checks if it matches a sent hash
/// 4. If it matches, the frame is filtered out as a loopback
///
/// This approach allows other tools (like candump) to still see all frames while
/// preventing the bridge from creating loops.
pub fn frame_hash(can_id: u32, data: &[u8]) -> u64 {
    let mut hash: u64 = can_id as u64;
    for (i, &b) in data.iter().enumerate() {
        hash ^= (b as u64) << ((i % 8) * 8);
    }
    hash
}
