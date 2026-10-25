//! CRC-32 as PNG uses it (the reflected 0xEDB88320 polynomial), for the
//! chunks this crate reads and writes.

/// The byte-indexed table of the reflected polynomial.
const TABLE: [u32; 256] = {
    let mut table = [0u32; 256];
    let mut at = 0;
    while at < 256 {
        #[allow(clippy::cast_possible_truncation, reason = "at < 256")]
        let mut crc = at as u32;
        let mut bit = 0;
        while bit < 8 {
            crc = if crc & 1 == 1 {
                0xEDB8_8320 ^ (crc >> 1)
            } else {
                crc >> 1
            };
            bit += 1;
        }
        table[at] = crc;
        at += 1;
    }
    table
};

/// The CRC-32 of `parts`, each fed in order (PNG checks a chunk's type
/// and data together).
pub(crate) fn of(parts: &[&[u8]]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for part in parts {
        // Each byte fed.
        crate::work::count(part.len());
        for &byte in *part {
            let lane = usize::from(crc.to_le_bytes()[0] ^ byte);
            crc = TABLE[lane] ^ (crc >> 8);
        }
    }
    !crc
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_published_vector_checks_out() {
        assert_eq!(super::of(&[b"123456789"]), 0xCBF4_3926);
        assert_eq!(super::of(&[b"1234", b"56789"]), 0xCBF4_3926, "split feeds");
        assert_eq!(super::of(&[b""]), 0);
    }
}
