use canbridge::{
    can_to_wire, frame_hash, recv_wire, send_wire, wire_to_can, WireFrameV1, FLAG_EFF, FLAG_ERR,
    FLAG_FD, FLAG_RTR,
};
use socketcan::{CanAnyFrame, CanDataFrame, CanFdFrame, CanRemoteFrame, EmbeddedFrame, ExtendedId, Id, StandardId};
use std::io::Cursor;

// ============================================================================
// WireFrameV1 Serialization Tests
// ============================================================================

#[test]
fn test_wire_frame_roundtrip_standard() {
    let frame = WireFrameV1::new(0x123, &[0xDE, 0xAD, 0xBE, 0xEF], 0);
    let bytes = postcard::to_stdvec(&frame).unwrap();
    let decoded: WireFrameV1 = postcard::from_bytes(&bytes).unwrap();

    assert_eq!(frame.version, decoded.version);
    assert_eq!(frame.can_id, decoded.can_id);
    assert_eq!(frame.data, decoded.data);
    assert_eq!(frame.flags, decoded.flags);
}

#[test]
fn test_wire_frame_roundtrip_fd() {
    let frame = WireFrameV1::new(0x456, &[1, 2, 3, 4, 5, 6, 7, 8], FLAG_FD);
    let bytes = postcard::to_stdvec(&frame).unwrap();
    let decoded: WireFrameV1 = postcard::from_bytes(&bytes).unwrap();

    assert_eq!(frame, decoded);
    assert_eq!(decoded.flags & FLAG_FD, FLAG_FD);
}

#[test]
fn test_wire_frame_roundtrip_extended_id() {
    let frame = WireFrameV1::new(0x1FFFFFFF, &[0xCA, 0xFE], FLAG_EFF);
    let bytes = postcard::to_stdvec(&frame).unwrap();
    let decoded: WireFrameV1 = postcard::from_bytes(&bytes).unwrap();

    assert_eq!(frame, decoded);
    assert_eq!(decoded.flags & FLAG_EFF, FLAG_EFF);
    assert_eq!(decoded.can_id, 0x1FFFFFFF);
}

#[test]
fn test_wire_frame_max_payload() {
    // CAN FD max is 64 bytes
    let data: Vec<u8> = (0..64).collect();
    let frame = WireFrameV1::new(0x100, &data, FLAG_FD);
    let bytes = postcard::to_stdvec(&frame).unwrap();
    let decoded: WireFrameV1 = postcard::from_bytes(&bytes).unwrap();

    assert_eq!(frame, decoded);
    assert_eq!(decoded.data.len(), 64);
}

// ============================================================================
// send_wire/recv_wire Tests
// ============================================================================

#[test]
fn test_send_recv_wire_roundtrip() {
    let frame = WireFrameV1::new(0x123, &[0xDE, 0xAD, 0xBE, 0xEF], 0);

    // Write to buffer
    let mut buffer = Vec::new();
    send_wire(&mut buffer, &frame).unwrap();

    // Read back from buffer
    let mut cursor = Cursor::new(buffer);
    let decoded = recv_wire(&mut cursor).unwrap();

    assert_eq!(frame, decoded);
}

#[test]
fn test_recv_wire_oversized_payload_rejected() {
    // Craft a malicious payload with length > 2048
    let mut buffer = Vec::new();
    let len: u16 = 3000;
    buffer.extend_from_slice(&len.to_be_bytes());
    buffer.extend(vec![0u8; 3000]);

    let mut cursor = Cursor::new(buffer);
    let result = recv_wire(&mut cursor);

    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("too large"));
}

#[test]
fn test_multiple_frames_sequential() {
    let frames = vec![
        WireFrameV1::new(0x100, &[1], 0),
        WireFrameV1::new(0x200, &[2, 3], FLAG_FD),
        WireFrameV1::new(0x300, &[4, 5, 6], FLAG_EFF),
    ];

    // Write all frames
    let mut buffer = Vec::new();
    for frame in &frames {
        send_wire(&mut buffer, frame).unwrap();
    }

    // Read all frames back
    let mut cursor = Cursor::new(buffer);
    for expected in &frames {
        let decoded = recv_wire(&mut cursor).unwrap();
        assert_eq!(*expected, decoded);
    }
}

// ============================================================================
// frame_hash Tests
// ============================================================================

#[test]
fn test_frame_hash_deterministic() {
    let h1 = frame_hash(0x123, &[1, 2, 3]);
    let h2 = frame_hash(0x123, &[1, 2, 3]);
    assert_eq!(h1, h2);
}

#[test]
fn test_frame_hash_different_id() {
    let h1 = frame_hash(0x123, &[1, 2, 3]);
    let h2 = frame_hash(0x456, &[1, 2, 3]);
    assert_ne!(h1, h2);
}

#[test]
fn test_frame_hash_different_data() {
    let h1 = frame_hash(0x123, &[1, 2, 3]);
    let h2 = frame_hash(0x123, &[1, 2, 4]);
    assert_ne!(h1, h2);
}

#[test]
fn test_frame_hash_empty_data() {
    let h1 = frame_hash(0x123, &[]);
    let h2 = frame_hash(0x123, &[]);
    assert_eq!(h1, h2);

    // Empty data should differ from non-zero data
    let h3 = frame_hash(0x123, &[1]);
    assert_ne!(h1, h3);
}

// ============================================================================
// can_to_wire Tests
// ============================================================================

#[test]
fn test_can_to_wire_standard_frame() {
    let id = StandardId::new(0x123).unwrap();
    let can_frame = CanDataFrame::new(Id::Standard(id), &[0xDE, 0xAD]).unwrap();
    let wire = can_to_wire(&CanAnyFrame::Normal(can_frame));

    assert_eq!(wire.can_id, 0x123);
    assert_eq!(wire.data, vec![0xDE, 0xAD]);
    assert_eq!(wire.flags & FLAG_EFF, 0); // Not extended
    assert_eq!(wire.flags & FLAG_FD, 0); // Not FD
    assert_eq!(wire.flags & FLAG_RTR, 0); // Not remote
}

#[test]
fn test_can_to_wire_extended_frame() {
    let id = ExtendedId::new(0x1ABCDEF).unwrap();
    let can_frame = CanDataFrame::new(Id::Extended(id), &[0xCA, 0xFE]).unwrap();
    let wire = can_to_wire(&CanAnyFrame::Normal(can_frame));

    assert_eq!(wire.can_id, 0x1ABCDEF);
    assert_eq!(wire.data, vec![0xCA, 0xFE]);
    assert_eq!(wire.flags & FLAG_EFF, FLAG_EFF); // Extended
    assert_eq!(wire.flags & FLAG_FD, 0); // Not FD
}

#[test]
fn test_can_to_wire_fd_frame() {
    let id = StandardId::new(0x200).unwrap();
    let data: Vec<u8> = (0..64).collect();
    let can_frame = CanFdFrame::new(Id::Standard(id), &data).unwrap();
    let wire = can_to_wire(&CanAnyFrame::Fd(can_frame));

    assert_eq!(wire.can_id, 0x200);
    assert_eq!(wire.data.len(), 64);
    assert_eq!(wire.flags & FLAG_FD, FLAG_FD); // FD flag set
}

#[test]
fn test_can_to_wire_remote_frame() {
    let id = StandardId::new(0x300).unwrap();
    let can_frame = CanRemoteFrame::new_remote(Id::Standard(id), 4).unwrap();
    let wire = can_to_wire(&CanAnyFrame::Remote(can_frame));

    assert_eq!(wire.can_id, 0x300);
    assert_eq!(wire.data.len(), 0); // Remote frames have no data
    assert_eq!(wire.flags & FLAG_RTR, FLAG_RTR); // RTR flag set
}

// ============================================================================
// wire_to_can Tests
// ============================================================================

#[test]
fn test_wire_to_can_standard_frame() {
    let wire = WireFrameV1::new(0x123, &[0xDE, 0xAD, 0xBE, 0xEF], 0);
    let can_frame = wire_to_can(&wire).unwrap();

    match can_frame {
        CanAnyFrame::Normal(f) => {
            assert_eq!(f.id(), Id::Standard(StandardId::new(0x123).unwrap()));
            assert_eq!(f.data(), &[0xDE, 0xAD, 0xBE, 0xEF]);
        }
        _ => panic!("Expected Normal frame"),
    }
}

#[test]
fn test_wire_to_can_extended_frame() {
    let wire = WireFrameV1::new(0x1ABCDEF, &[0xCA, 0xFE], FLAG_EFF);
    let can_frame = wire_to_can(&wire).unwrap();

    match can_frame {
        CanAnyFrame::Normal(f) => {
            assert_eq!(f.id(), Id::Extended(ExtendedId::new(0x1ABCDEF).unwrap()));
            assert_eq!(f.data(), &[0xCA, 0xFE]);
        }
        _ => panic!("Expected Normal frame with extended ID"),
    }
}

#[test]
fn test_wire_to_can_fd_frame() {
    let data: Vec<u8> = (0..64).collect();
    let wire = WireFrameV1::new(0x200, &data, FLAG_FD);
    let can_frame = wire_to_can(&wire).unwrap();

    match can_frame {
        CanAnyFrame::Fd(f) => {
            assert_eq!(f.id(), Id::Standard(StandardId::new(0x200).unwrap()));
            assert_eq!(f.data().len(), 64);
        }
        _ => panic!("Expected FD frame"),
    }
}

#[test]
fn test_wire_to_can_remote_frame() {
    let wire = WireFrameV1::new(0x300, &[], FLAG_RTR);
    let can_frame = wire_to_can(&wire).unwrap();

    match can_frame {
        CanAnyFrame::Remote(f) => {
            assert_eq!(f.id(), Id::Standard(StandardId::new(0x300).unwrap()));
        }
        _ => panic!("Expected Remote frame"),
    }
}

#[test]
fn test_wire_to_can_error_frame_rejected() {
    let wire = WireFrameV1::new(0, &[1, 2, 3], FLAG_ERR);
    let result = wire_to_can(&wire);

    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("error frame"));
}

#[test]
fn test_wire_to_can_invalid_standard_id() {
    // Standard IDs are 11-bit (max 0x7FF)
    let wire = WireFrameV1::new(0x800, &[1, 2, 3], 0); // Invalid: too large for standard
    let result = wire_to_can(&wire);

    assert!(result.is_err());
}

// ============================================================================
// Round-Trip Tests (can_to_wire -> wire_to_can)
// ============================================================================

#[test]
fn test_roundtrip_standard_frame() {
    let id = StandardId::new(0x123).unwrap();
    let original = CanDataFrame::new(Id::Standard(id), &[0xDE, 0xAD, 0xBE, 0xEF]).unwrap();

    let wire = can_to_wire(&CanAnyFrame::Normal(original.clone()));
    let recovered = wire_to_can(&wire).unwrap();

    match recovered {
        CanAnyFrame::Normal(f) => {
            assert_eq!(f.id(), original.id());
            assert_eq!(f.data(), original.data());
        }
        _ => panic!("Expected Normal frame"),
    }
}

#[test]
fn test_roundtrip_fd_frame_64_bytes() {
    let id = StandardId::new(0x456).unwrap();
    let data: Vec<u8> = (0..64).collect();
    let original = CanFdFrame::new(Id::Standard(id), &data).unwrap();

    let wire = can_to_wire(&CanAnyFrame::Fd(original.clone()));
    let recovered = wire_to_can(&wire).unwrap();

    match recovered {
        CanAnyFrame::Fd(f) => {
            assert_eq!(f.id(), original.id());
            assert_eq!(f.data(), original.data());
        }
        _ => panic!("Expected FD frame"),
    }
}

#[test]
fn test_roundtrip_extended_id() {
    let id = ExtendedId::new(0x1FFFFFF).unwrap();
    let original = CanDataFrame::new(Id::Extended(id), &[0xAB, 0xCD]).unwrap();

    let wire = can_to_wire(&CanAnyFrame::Normal(original.clone()));
    let recovered = wire_to_can(&wire).unwrap();

    match recovered {
        CanAnyFrame::Normal(f) => {
            assert_eq!(f.id(), original.id());
            assert_eq!(f.data(), original.data());
        }
        _ => panic!("Expected Normal frame"),
    }
}
