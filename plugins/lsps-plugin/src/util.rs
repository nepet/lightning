use core::fmt;

use cln_rpc::primitives::PublicKey;

/// Checks if the feature bit is set in the provided bitmap.
/// Returns true if the `feature_bit` is set in the `bitmap`. Returns false if
/// the `feature_bit` is unset or our ouf bounds.
///
/// # Arguments
///
/// * `bitmap`: A slice of bytes representing the feature bitmap.
/// * `feature_bit`: The 0-based index of the bit to check across the bitmap.
///
pub fn is_feature_bit_set(bitmap: &[u8], feature_bit: usize) -> bool {
    let byte_index = feature_bit >> 3; // Equivalent to feature_bit / 8
    let bit_index = feature_bit & 7; // Equivalent to feature_bit % 8

    if let Some(&target_byte) = bitmap.get(byte_index) {
        let mask = 1 << bit_index;
        (target_byte & mask) != 0
    } else {
        false
    }
}

/// Returns a single feature_bit in hex representation, least-significant bit
/// first.
///
/// # Arguments
///
/// * `feature_bit`: The 0-based index of the bit to check across the bitmap.
///
pub fn feature_bit_to_hex(feature_bit: usize) -> String {
    let byte_index = feature_bit >> 3; // Equivalent to feature_bit / 8
    let mask = 1 << (feature_bit & 7); // Equivalent to feature_bit % 8
    let mut map = vec![0u8; byte_index + 1];
    map[0] |= mask; // least-significant bit first ordering.
    hex::encode(&map)
}

/// Errors that can occur when unwrapping payload data
#[derive(Debug, Clone, PartialEq)]
pub enum UnwrapError {
    /// Buffer is too small to contain valid wrapped data
    BufferTooSmall { expected_min: usize, actual: usize },
    /// The public key bytes are invalid
    InvalidPublicKey(String),
}

impl fmt::Display for UnwrapError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            UnwrapError::BufferTooSmall {
                expected_min,
                actual,
            } => {
                write!(
                    f,
                    "Buffer too small: expected at least {} bytes, got {}",
                    expected_min, actual
                )
            }
            UnwrapError::InvalidPublicKey(e) => {
                write!(f, "Invalid public key: {}", e)
            }
        }
    }
}

impl std::error::Error for UnwrapError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            _ => None,
        }
    }
}

/// Wraps a payload with a peer ID for internal LSPS message transmission.
pub fn wrap_payload_with_peer_id(payload: &[u8], peer_id: PublicKey) -> Vec<u8> {
    let pubkey_bytes = peer_id.serialize();

    let mut result = Vec::with_capacity(33 + payload.len());
    result.extend_from_slice(&pubkey_bytes);
    result.extend_from_slice(payload);
    result
}

/// Safely unwraps payload data and a peer ID
pub fn try_unwrap_payload_with_peer_id(data: &[u8]) -> Result<(&[u8], PublicKey), UnwrapError> {
    const MIN_SIZE: usize = 33;

    if data.len() < MIN_SIZE {
        return Err(UnwrapError::BufferTooSmall {
            expected_min: MIN_SIZE,
            actual: data.len(),
        });
    }

    let pubkey = PublicKey::from_slice(&data[0..33])
        .map_err(|e| UnwrapError::InvalidPublicKey(e.to_string()))?;

    let payload = &data[33..];
    Ok((payload, pubkey))
}

/// Unwraps payload data and peer ID, panicking on error
///
/// This is a convenience function for cases where one knows the data is valid.
pub fn unwrap_payload_with_peer_id(data: &[u8]) -> (&[u8], PublicKey) {
    try_unwrap_payload_with_peer_id(data).expect("Failed to unwrap payload with peer")
}

#[cfg(test)]
mod tests {
    use super::*;

    // Valid test public key
    const PUBKEY: [u8; 33] = [
        0x02, 0x79, 0xbe, 0x66, 0x7e, 0xf9, 0xdc, 0xbb, 0xac, 0x55, 0xa0, 0x62, 0x95, 0xce, 0x87,
        0x0b, 0x07, 0x02, 0x9b, 0xfc, 0xdb, 0x2d, 0xce, 0x28, 0xd9, 0x59, 0xf2, 0x81, 0x5b, 0x16,
        0xf8, 0x17, 0x98,
    ];

    #[test]
    fn test_wrap_and_unwrap_roundtrip() {
        let payload = b"test lsps message";
        let peer_id = PublicKey::from_slice(&PUBKEY).unwrap();

        let wrapped = wrap_payload_with_peer_id(payload, peer_id);
        let (unwrapped_payload, unwrapped_peer_id) = unwrap_payload_with_peer_id(&wrapped);

        assert_eq!(unwrapped_payload, payload);
        assert_eq!(unwrapped_peer_id, peer_id);
    }

    #[test]
    fn test_empty_payload() {
        let payload = b"";
        let peer_id = PublicKey::from_slice(&PUBKEY).unwrap();

        let wrapped = wrap_payload_with_peer_id(payload, peer_id);
        assert_eq!(wrapped.len(), 33); // Just pubkey, no payload

        let (unwrapped_payload, unwrapped_peer_id) = unwrap_payload_with_peer_id(&wrapped);

        assert_eq!(unwrapped_payload, payload);
        assert_eq!(unwrapped_peer_id, peer_id);
    }

    #[test]
    fn test_large_payload() {
        let payload = vec![0x42; 10000];
        let peer_id = PublicKey::from_slice(&PUBKEY).unwrap();

        let wrapped = wrap_payload_with_peer_id(&payload, peer_id);
        assert_eq!(wrapped.len(), 33 + 10000);

        let (unwrapped_payload, unwrapped_peer_id) = unwrap_payload_with_peer_id(&wrapped);

        assert_eq!(unwrapped_payload, payload.as_slice());
        assert_eq!(unwrapped_peer_id, peer_id);
    }

    #[test]
    fn test_buffer_too_small() {
        let small_buffer = [0u8; 10];
        let result = try_unwrap_payload_with_peer_id(&small_buffer);

        match result {
            Err(UnwrapError::BufferTooSmall {
                expected_min: 33,
                actual: 10,
            }) => {}
            _ => panic!("Expected BufferTooSmall error"),
        }
    }

    #[test]
    fn test_exactly_pubkey_size() {
        // Buffer with exactly 33 bytes (valid pubkey, empty payload)
        let peer_id = PublicKey::from_slice(&PUBKEY).unwrap();
        let wrapped = wrap_payload_with_peer_id(b"", peer_id);
        assert_eq!(wrapped.len(), 33);

        let (payload, recovered_peer_id) = unwrap_payload_with_peer_id(&wrapped);
        assert_eq!(payload.len(), 0);
        assert_eq!(recovered_peer_id, peer_id);
    }

    #[test]
    fn test_invalid_pubkey() {
        let mut invalid_data = vec![0u8; 40];
        // Set an invalid public key (all zeros)
        invalid_data[0] = 0x02; // Valid prefix
                                // But rest remains zeros which is invalid

        let result = try_unwrap_payload_with_peer_id(&invalid_data);
        assert!(matches!(result, Err(UnwrapError::InvalidPublicKey(_))));
    }

    #[test]
    fn test_basic_bit_checks() {
        // Example bitmap:
        // Byte 0: 0b10100101 (165) -> Bits 0, 2, 5, 7 set
        // Byte 1: 0b01101010 (106) -> Bits 1, 3, 5, 6 set (indices 9, 11, 13, 14)
        let bitmap: &[u8] = &[0b10100101, 0b01101010];

        // Check bits in byte 0 (indices 0-7)
        assert_eq!(is_feature_bit_set(bitmap, 0), true); // Bit 0
        assert_eq!(is_feature_bit_set(bitmap, 1), false); // Bit 1
        assert_eq!(is_feature_bit_set(bitmap, 2), true); // Bit 2
        assert_eq!(is_feature_bit_set(bitmap, 3), false); // Bit 3
        assert_eq!(is_feature_bit_set(bitmap, 4), false); // Bit 4
        assert_eq!(is_feature_bit_set(bitmap, 5), true); // Bit 5
        assert_eq!(is_feature_bit_set(bitmap, 6), false); // Bit 6
        assert_eq!(is_feature_bit_set(bitmap, 7), true); // Bit 7

        // Check bits in byte 1 (indices 8-15)
        assert_eq!(is_feature_bit_set(bitmap, 8), false); // Bit 8 (Byte 1, bit 0)
        assert_eq!(is_feature_bit_set(bitmap, 9), true); // Bit 9 (Byte 1, bit 1)
        assert_eq!(is_feature_bit_set(bitmap, 10), false); // Bit 10 (Byte 1, bit 2)
        assert_eq!(is_feature_bit_set(bitmap, 11), true); // Bit 11 (Byte 1, bit 3)
        assert_eq!(is_feature_bit_set(bitmap, 12), false); // Bit 12 (Byte 1, bit 4)
        assert_eq!(is_feature_bit_set(bitmap, 13), true); // Bit 13 (Byte 1, bit 5)
        assert_eq!(is_feature_bit_set(bitmap, 14), true); // Bit 14 (Byte 1, bit 6)
        assert_eq!(is_feature_bit_set(bitmap, 15), false); // Bit 15 (Byte 1, bit 7)
    }

    #[test]
    fn test_out_of_bounds() {
        let bitmap: &[u8] = &[0b11111111, 0b00000000]; // 16 bits total

        assert_eq!(is_feature_bit_set(bitmap, 15), false); // Last valid bit (is 0)
        assert_eq!(is_feature_bit_set(bitmap, 16), false); // Out of bounds
        assert_eq!(is_feature_bit_set(bitmap, 100), false); // Way out of bounds
    }

    #[test]
    fn test_empty_bitmap() {
        let bitmap: &[u8] = &[];
        assert_eq!(is_feature_bit_set(bitmap, 0), false);
        assert_eq!(is_feature_bit_set(bitmap, 8), false);
    }

    #[test]
    fn test_feature_to_hex_bit_0_be() {
        // Bit 0 is in Byte 0 (LE index). num_bytes=1. BE index = 1-1-0=0.
        // Expected map: [0x01]
        let feature_hex = feature_bit_to_hex(0);
        assert_eq!(feature_hex, "01");
        assert!(is_feature_bit_set(&hex::decode(feature_hex).unwrap(), 0));
    }

    #[test]
    fn test_feature_to_hex_bit_8_be() {
        // Bit 8 is in Byte 1 (LE index). num_bytes=2. BE index = 2-1-1=0.
        // Mask is 0x01 for bit 0 within its byte.
        // Expected map: [0x01, 0x00] (Byte for 8-15 first, then 0-7)
        let feature_hex = feature_bit_to_hex(8);
        let mut decoded = hex::decode(&feature_hex).unwrap();
        decoded.reverse();
        assert_eq!(feature_hex, "0100");
        assert!(is_feature_bit_set(&decoded, 8));
    }

    #[test]
    fn test_feature_to_hex_bit_27_be() {
        // Bit 27 is in Byte 3 (LE index). num_bytes=4. BE index = 4-1-3=0.
        // Mask is 0x08 for bit 3 within its byte.
        // Expected map: [0x08, 0x00, 0x00, 0x00] (Byte for 24-31 first)
        let feature_hex = feature_bit_to_hex(27);
        let mut decoded = hex::decode(&feature_hex).unwrap();
        decoded.reverse();
        assert_eq!(feature_hex, "08000000");
        assert!(is_feature_bit_set(&decoded, 27));
    }
}
