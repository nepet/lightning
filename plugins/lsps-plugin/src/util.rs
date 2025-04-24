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

#[cfg(test)]
mod tests {
    use super::*;

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
}
