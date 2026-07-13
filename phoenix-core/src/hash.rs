use blake3::Hasher;

pub fn hash_bytes(data: &[u8]) -> [u8; 32] {
    *blake3::hash(data).as_bytes()
}

pub fn hash_hex(data: &[u8]) -> String {
    hex_encode(&hash_bytes(data))
}

pub fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn hex_decode(s: &str) -> Option<[u8; 32]> {
    if s.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, chunk) in s.as_bytes().chunks(2).enumerate() {
        let hi = hex_nibble(chunk.first()?)?;
        let lo = hex_nibble(chunk.get(1)?)?;
        out[i] = (hi << 4) | lo;
    }
    Some(out)
}

fn hex_nibble(c: &u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

pub struct IncrementalHasher {
    inner: Hasher,
}

impl Default for IncrementalHasher {
    fn default() -> Self {
        Self::new()
    }
}

impl IncrementalHasher {
    pub fn new() -> Self {
        Self {
            inner: Hasher::new(),
        }
    }

    pub fn update(&mut self, data: &[u8]) {
        self.inner.update(data);
    }

    pub fn finalize(self) -> [u8; 32] {
        *self.inner.finalize().as_bytes()
    }
}

/// CRC32 (IEEE) for header validation. Used by GPT.
pub fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &byte in data {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            let mask = 0u32.wrapping_sub(crc & 1);
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

/// CRC32**C** (Castagnoli) — a *different polynomial* from [`crc32`], and the one
/// VHDX requires for its header and region-table checksums.
///
/// The two are easy to confuse and fail identically-shaped: a VHDX checksummed
/// with the IEEE polynomial is structurally perfect and Windows rejects the file
/// outright, with no hint as to why. Keeping both here, adjacent and named for
/// what they are, is the cheapest way to not make that mistake twice.
pub fn crc32c(data: &[u8]) -> u32 {
    // 0x82F63B78 is the bit-reflected Castagnoli polynomial, matching the
    // reflected (LSB-first) formulation used above.
    let mut crc = 0xFFFF_FFFFu32;
    for &byte in data {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            let mask = 0u32.wrapping_sub(crc & 1);
            crc = (crc >> 1) ^ (0x82F6_3B78 & mask);
        }
    }
    !crc
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Known-answer vectors. `crc32c("123456789") == 0xE3069283` is the standard
    /// CRC-32C check value; the IEEE polynomial gives `0xCBF43926` for the same
    /// input. Pinning both proves we didn't silently wire one to the other —
    /// which would produce a VHDX Windows rejects with no diagnosis.
    #[test]
    fn crc32_variants_are_the_right_polynomials() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926, "IEEE CRC-32 check value");
        assert_eq!(
            crc32c(b"123456789"),
            0xE306_9283,
            "Castagnoli CRC-32C check value"
        );
        assert_ne!(crc32(b"123456789"), crc32c(b"123456789"));
        assert_eq!(crc32c(b""), 0);
    }

    #[test]
    fn blake3_deterministic() {
        let h1 = hash_hex(b"test");
        let h2 = hash_hex(b"test");
        assert_eq!(h1, h2);
        assert_eq!(h1.len(), 64);
    }
}
