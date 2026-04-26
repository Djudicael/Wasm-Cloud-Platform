//! Cryptographic utility functions for the Wasm Cloud Platform.
//!
//! These functions are designed for security-sensitive operations where
//! timing side-channels must be avoided.

/// Constant-time comparison of two byte slices.
///
/// Returns `true` if they are equal, `false` otherwise.
/// Takes the same amount of time regardless of where the first difference occurs,
/// preventing timing attacks that could reveal token values character-by-character.
///
/// # How it works
///
/// We iterate `max(a.len(), b.len())` times, XORing corresponding bytes.
/// For the shorter array's "missing" bytes, we substitute `0xFF` so that
/// length differences always produce a non-zero result. The length difference
/// itself is also ORed into the accumulator. The final check is a single
/// branch on the accumulated result.
///
/// # Use cases
///
/// - Bearer token comparison in admin API authentication
/// - API key validation
/// - Any secret comparison where timing leakage is a concern
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    let mut result: u8 = 0;
    let max_len = std::cmp::max(a.len(), b.len());

    for i in 0..max_len {
        let x = a.get(i).copied().unwrap_or(0xFF);
        let y = b.get(i).copied().unwrap_or(0xFF);
        result |= x ^ y;
    }

    // Length difference must also factor into the result
    result |= (a.len() != b.len()) as u8;

    result == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_constant_time_eq_equal() {
        assert!(constant_time_eq(b"hello", b"hello"));
    }

    #[test]
    fn test_constant_time_eq_not_equal() {
        assert!(!constant_time_eq(b"hello", b"world"));
    }

    #[test]
    fn test_constant_time_eq_different_lengths() {
        assert!(!constant_time_eq(b"hello", b"helloworld"));
    }

    #[test]
    fn test_constant_time_eq_empty() {
        assert!(constant_time_eq(b"", b""));
    }

    #[test]
    fn test_constant_time_eq_single_char_equal() {
        assert!(constant_time_eq(b"a", b"a"));
    }

    #[test]
    fn test_constant_time_eq_single_char_not_equal() {
        assert!(!constant_time_eq(b"a", b"b"));
    }

    #[test]
    fn test_constant_time_eq_long_equal() {
        let a = b"a1b2c3d4e5f6789012345678901234567890abcdef1234567890abcdef123456";
        let b = b"a1b2c3d4e5f6789012345678901234567890abcdef1234567890abcdef123456";
        assert!(constant_time_eq(a, b));
    }

    #[test]
    fn test_constant_time_eq_long_diff_one_byte() {
        let a = b"a1b2c3d4e5f6789012345678901234567890abcdef1234567890abcdef123456";
        let mut b = *a;
        b[63] = b'7'; // change last byte
        assert!(!constant_time_eq(a, &b));
    }

    #[test]
    fn test_constant_time_eq_binary_data() {
        let a: &[u8] = &[0x00, 0xff, 0x80, 0x7f];
        let b: &[u8] = &[0x00, 0xff, 0x80, 0x7f];
        assert!(constant_time_eq(a, b));
    }

    #[test]
    fn test_constant_time_eq_binary_data_diff() {
        let a: &[u8] = &[0x00, 0xff, 0x80, 0x7f];
        let b: &[u8] = &[0x00, 0xff, 0x80, 0x7e];
        assert!(!constant_time_eq(a, b));
    }
}
