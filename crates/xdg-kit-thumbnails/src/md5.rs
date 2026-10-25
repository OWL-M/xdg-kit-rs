//! MD5 (RFC 1321), as the Thumbnail Managing Standard's cache-naming
//! convention. It is nothing but a name here: MD5 carries no security,
//! and this module exists only so the crate hashes URIs to the same
//! file names every other implementation uses.

/// Per-round left-rotation amounts.
const ROTATIONS: [u32; 64] = [
    7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, //
    5, 9, 14, 20, 5, 9, 14, 20, 5, 9, 14, 20, 5, 9, 14, 20, //
    4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, //
    6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21,
];

/// The binary parts of the sines of 1..=64 (RFC 1321's T table).
const SINES: [u32; 64] = [
    0xd76a_a478,
    0xe8c7_b756,
    0x2420_70db,
    0xc1bd_ceee,
    0xf57c_0faf,
    0x4787_c62a,
    0xa830_4613,
    0xfd46_9501,
    0x6980_98d8,
    0x8b44_f7af,
    0xffff_5bb1,
    0x895c_d7be,
    0x6b90_1122,
    0xfd98_7193,
    0xa679_438e,
    0x49b4_0821,
    0xf61e_2562,
    0xc040_b340,
    0x265e_5a51,
    0xe9b6_c7aa,
    0xd62f_105d,
    0x0244_1453,
    0xd8a1_e681,
    0xe7d3_fbc8,
    0x21e1_cde6,
    0xc337_07d6,
    0xf4d5_0d87,
    0x455a_14ed,
    0xa9e3_e905,
    0xfcef_a3f8,
    0x676f_02d9,
    0x8d2a_4c8a,
    0xfffa_3942,
    0x8771_f681,
    0x6d9d_6122,
    0xfde5_380c,
    0xa4be_ea44,
    0x4bde_cfa9,
    0xf6bb_4b60,
    0xbebf_bc70,
    0x289b_7ec6,
    0xeaa1_27fa,
    0xd4ef_3085,
    0x0488_1d05,
    0xd9d4_d039,
    0xe6db_99e5,
    0x1fa2_7cf8,
    0xc4ac_5665,
    0xf429_2244,
    0x432a_ff97,
    0xab94_23a7,
    0xfc93_a039,
    0x655b_59c3,
    0x8f0c_cc92,
    0xffef_f47d,
    0x8584_5dd1,
    0x6fa8_7e4f,
    0xfe2c_e6e0,
    0xa301_4314,
    0x4e08_11a1,
    0xf753_7e82,
    0xbd3a_f235,
    0x2ad7_d2bb,
    0xeb86_d391,
];

/// The MD5 of `bytes` as 32 lowercase hex digits.
pub(crate) fn hex(bytes: &[u8]) -> String {
    // Each byte hashed.
    crate::work::count(bytes.len());
    let mut state: [u32; 4] = [0x6745_2301, 0xefcd_ab89, 0x98ba_dcfe, 0x1032_5476];
    let mut padded = bytes.to_vec();
    padded.push(0x80);
    while padded.len() % 64 != 56 {
        padded.push(0);
    }
    padded.extend_from_slice(&((bytes.len() as u64).wrapping_mul(8)).to_le_bytes());
    let (blocks, _) = padded.as_chunks::<64>();
    for block in blocks {
        let mut words = [0u32; 16];
        let (lanes, _) = block.as_chunks::<4>();
        for (word, lane) in words.iter_mut().zip(lanes) {
            *word = u32::from_le_bytes(*lane);
        }
        let [mut a, mut b, mut c, mut d] = state;
        for step in 0..64 {
            let (mixed, from) = match step / 16 {
                0 => ((b & c) | (!b & d), step),
                1 => ((d & b) | (!d & c), (5 * step + 1) % 16),
                2 => (b ^ c ^ d, (3 * step + 5) % 16),
                _ => (c ^ (b | !d), (7 * step) % 16),
            };
            let spun = a
                .wrapping_add(mixed)
                .wrapping_add(SINES[step])
                .wrapping_add(words[from]);
            (a, d, c) = (d, c, b);
            b = b.wrapping_add(spun.rotate_left(ROTATIONS[step]));
            // The sequence above rotated (a, b, c, d) -> (d, a, b, c)
            // with the new b; nothing else moves.
        }
        state[0] = state[0].wrapping_add(a);
        state[1] = state[1].wrapping_add(b);
        state[2] = state[2].wrapping_add(c);
        state[3] = state[3].wrapping_add(d);
    }
    let mut hex = String::with_capacity(32);
    for word in state {
        for byte in word.to_le_bytes() {
            use std::fmt::Write as _;
            let _ = write!(hex, "{byte:02x}");
        }
    }
    hex
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_rfc_vectors_hash_right() {
        assert_eq!(hex(b""), "d41d8cd98f00b204e9800998ecf8427e");
        assert_eq!(hex(b"a"), "0cc175b9c0f1b6a831c399e269772661");
        assert_eq!(hex(b"abc"), "900150983cd24fb0d6963f7d28e17f72");
        assert_eq!(hex(b"message digest"), "f96b697d7cb7938d525a2f31aaf161d0");
        assert_eq!(
            hex(b"The quick brown fox jumps over the lazy dog"),
            "9e107d9d372bb6826bd81d3542a419d6"
        );
        // Longer than one block, and exactly around the padding edges.
        assert_eq!(
            hex("a".repeat(55).as_bytes()),
            "ef1772b6dff9a122358552954ad0df65"
        );
        assert_eq!(
            hex("a".repeat(56).as_bytes()),
            "3b0c8ac703f828b04c6c197006d17218"
        );
        assert_eq!(
            hex("a".repeat(64).as_bytes()),
            "014842d480b571495a4a0363793f7367"
        );
    }

    #[test]
    fn the_specs_example_uri_hashes_right() {
        assert_eq!(
            hex(b"file:///home/jens/photos/me.png"),
            "c6ee772d9e49320e97ec29a7eb5b1697"
        );
    }
}
