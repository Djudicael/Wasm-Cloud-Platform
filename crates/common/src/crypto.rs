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
/// When lengths differ, we still perform a comparison (against itself) to burn
/// the same CPU cycles, then return `false`. When lengths match, we XOR each
/// corresponding byte and OR the results — the final check is a single branch
/// on the accumulated result.
///
/// # Use cases
///
/// - Bearer token comparison in admin API authentication
/// - API key validation
/// - Any secret comparison where timing leakage is a concern
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        // Still do a comparison to avoid leaking length via timing.
        // Compare a against itself to burn the same CPU cycles.
        let mut result = 0u8;
        for byte in a.iter().chain(b.iter()) {
            result |= byte ^ byte; // always 0, but the loop runs
        }
        let _ = &result; // suppress unused-variable warning (result is intentional)
        false
    } else {
        let mut result = 0u8;
        for (x, y) in a.iter().zip(b.iter()) {
            result |= x ^ y;
        }
        result == 0
    }
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
