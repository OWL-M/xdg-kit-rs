//! The PNG chunk structure: enough to read and write the `tEXt` keys the
//! Thumbnail Managing Standard uses, and to check a file is structurally
//! a PNG. No image decoding happens here, and structural soundness never
//! promises the image decodes — the docs say so wherever it matters.

use crate::{Limits, crc32, work};

/// The eight signature bytes every PNG starts with.
const SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];

/// A structurally sound PNG's read-out: its `tEXt` keys (the dimensions
/// were checked positive on the way).
#[derive(Debug)]
pub(crate) struct Png {
    /// The dimensions `IHDR` declares.
    pub width: u32,
    pub height: u32,
    /// The `tEXt` entries in file order; a duplicated keyword reads
    /// last-wins through [`Png::key`].
    pub keys: Vec<(String, String)>,
}

impl Png {
    /// The last value of `keyword`, if any (duplicates read last-wins).
    pub(crate) fn key(&self, keyword: &str) -> Option<&str> {
        self.keys
            .iter()
            .rev()
            .inspect(|_| work::count(1))
            .find(|(named, _)| named == keyword)
            .map(|(_, value)| value.as_str())
    }
}

/// One walked chunk: its type, its data, and the whole chunk's bytes.
struct Chunk<'a> {
    kind: [u8; 4],
    data: &'a [u8],
    whole: &'a [u8],
}

/// Walks `bytes` as a PNG, calling `seen` for each chunk through `IEND`.
/// The walk checks the signature, the framing, every chunk's CRC, that
/// `IHDR` comes exactly once and first with positive dimensions, that
/// image data (`IDAT`) exists, and that `IEND` ends the file. This is
/// structural soundness, not image decoding — the docs say so wherever a
/// caller might read more into it.
fn walk<'a>(
    bytes: &'a [u8],
    limits: &Limits,
    mut seen: impl FnMut(&Chunk<'a>) -> Result<(), String>,
) -> Result<Header, String> {
    if !bytes.starts_with(&SIGNATURE) {
        return Err("not a PNG (no signature)".to_owned());
    }
    let mut at = SIGNATURE.len();
    let mut ihdr: Option<Header> = None;
    let mut saw_data = Data::None;
    let mut saw_palette = false;
    let mut chunks = 0usize;
    loop {
        // The chunk, and its framing bytes.
        work::count(1 + 8);
        chunks += 1;
        if chunks > limits.max_chunks {
            return Err(format!("more than {} chunks", limits.max_chunks));
        }
        let header = bytes
            .get(at..at.saturating_add(8))
            .filter(|header| header.len() == 8)
            .ok_or_else(|| "truncated chunk header".to_owned())?;
        let length = u32::from_be_bytes([header[0], header[1], header[2], header[3]]);
        // The spec caps a chunk at 2^31 - 1 bytes; staying within it also
        // keeps every offset below addition overflow on 32-bit targets
        // (together with the checked additions, which an attacker-chosen
        // length must not be able to wrap).
        if length > i32::MAX as u32 {
            return Err("a chunk length over the format's limit".to_owned());
        }
        let length = length as usize;
        let kind = [header[4], header[5], header[6], header[7]];
        if !kind.iter().all(u8::is_ascii_alphabetic) {
            return Err("a chunk type that is not four letters".to_owned());
        }
        let end = at
            .checked_add(8)
            .and_then(|at| at.checked_add(length))
            .and_then(|at| at.checked_add(4))
            .ok_or_else(|| "a chunk length past addressable memory".to_owned())?;
        let whole = bytes
            .get(at..end)
            .ok_or_else(|| "truncated chunk".to_owned())?;
        let data = &whole[8..8 + length];
        let stored = u32::from_be_bytes(
            whole[whole.len() - 4..]
                .try_into()
                .expect("four CRC bytes framed above"),
        );
        if crc32::of(&[&kind, data]) != stored {
            return Err(format!(
                "the CRC of a {} chunk does not match",
                String::from_utf8_lossy(&kind)
            ));
        }
        classify(
            kind,
            length,
            data,
            &mut ihdr,
            &mut saw_data,
            &mut saw_palette,
        )?;
        seen(&Chunk { kind, data, whole })?;
        at = end;
        if kind == *b"IEND" {
            if saw_data == Data::None {
                return Err("no image data (IDAT) before IEND".to_owned());
            }
            if at != bytes.len() {
                return Err("bytes after IEND".to_owned());
            }
            return Ok(ihdr.expect("set before any other chunk"));
        }
    }
}

/// Applies one chunk to the walk's structural state: the header rules,
/// the palette rules, the consecutive-image-data rule, the empty `IEND`
/// rule, and the editor's hard stop on unknown critical chunks.
fn classify(
    kind: [u8; 4],
    length: usize,
    data: &[u8],
    ihdr: &mut Option<Header>,
    saw_data: &mut Data,
    saw_palette: &mut bool,
) -> Result<(), String> {
    match (&kind, &ihdr) {
        (b"IHDR", None) => *ihdr = Some(header_fields(data)?),
        (b"IHDR", Some(_)) => return Err("a second IHDR".to_owned()),
        (_, None) => return Err("the first chunk is not a well-formed IHDR".to_owned()),
        (b"PLTE", Some(seen)) => {
            if *saw_palette {
                return Err("a second palette".to_owned());
            }
            if *saw_data != Data::None {
                return Err("a palette after the image data".to_owned());
            }
            // Grayscale types must not carry one (spec: PLTE shall not
            // appear for color types 0 and 4).
            if matches!(seen.color, 0 | 4) {
                return Err("a palette on a grayscale image".to_owned());
            }
            if length == 0 || length > 768 || !length.is_multiple_of(3) {
                return Err("a palette of an impossible length".to_owned());
            }
            // An indexed image's palette fits its bit depth: at most
            // 2^depth entries (a truecolor palette keeps the general 256
            // bound above).
            if seen.color == 3 && length / 3 > 1 << seen.depth {
                return Err(format!(
                    "{} palette entries on a {}-bit indexed image",
                    length / 3,
                    seen.depth
                ));
            }
            *saw_palette = true;
        }
        (b"IDAT", Some(seen)) => {
            // An indexed image needs its palette before the data.
            if seen.color == 3 && !*saw_palette {
                return Err("an indexed image without a palette".to_owned());
            }
            // Image data must be one consecutive run (spec: multiple
            // IDAT chunks shall be consecutive).
            if *saw_data == Data::Ended {
                return Err("image data split by another chunk".to_owned());
            }
            *saw_data = Data::Open;
        }
        (b"IEND", Some(_)) if length != 0 => {
            return Err("an IEND chunk with data".to_owned());
        }
        (b"IEND", Some(_)) => {}
        _ => {
            // An uppercase first letter marks a critical chunk; the four
            // above are the only ones the spec defines, and an editor
            // meeting an unknown one must stop — splicing metadata is
            // editing, and the chunk's implications are unknowable.
            if kind[0].is_ascii_uppercase() {
                return Err(format!(
                    "an unknown critical chunk ({})",
                    String::from_utf8_lossy(&kind)
                ));
            }
            if *saw_data == Data::Open {
                *saw_data = Data::Ended;
            }
        }
    }
    Ok(())
}

/// Where the image data stands during a walk.
#[derive(PartialEq, Eq, Clone, Copy)]
enum Data {
    None,
    Open,
    Ended,
}

/// What the walk learned from `IHDR`.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Header {
    pub width: u32,
    pub height: u32,
    color: u8,
    depth: u8,
}

/// Validates the thirteen `IHDR` fields: positive dimensions and the
/// spec's legal combinations of bit depth, color type and methods —
/// structural checks, which still cannot promise the image decodes.
fn header_fields(data: &[u8]) -> Result<Header, String> {
    if data.len() != 13 {
        return Err("the first chunk is not a well-formed IHDR".to_owned());
    }
    let width = u32::from_be_bytes([data[0], data[1], data[2], data[3]]);
    let height = u32::from_be_bytes([data[4], data[5], data[6], data[7]]);
    if width == 0 || height == 0 {
        return Err("a dimension of zero".to_owned());
    }
    let (depth, color) = (data[8], data[9]);
    #[allow(clippy::match_same_arms, reason = "the spec's table, row by row")]
    let legal = match color {
        0 => matches!(depth, 1 | 2 | 4 | 8 | 16),
        3 => matches!(depth, 1 | 2 | 4 | 8),
        2 | 4 | 6 => matches!(depth, 8 | 16),
        _ => false,
    };
    if !legal {
        return Err(format!(
            "bit depth {depth} with color type {color} is not a PNG combination"
        ));
    }
    // Compression and filter know one method each; interlace two.
    if data[10] != 0 || data[11] != 0 || data[12] > 1 {
        return Err("an unknown compression, filter or interlace method".to_owned());
    }
    Ok(Header {
        width,
        height,
        color,
        depth,
    })
}

/// Reads `bytes` as a PNG: structure and every CRC checked as [`walk`]
/// does. The error is why it is unsound.
pub(crate) fn read(bytes: &[u8], limits: &Limits) -> Result<Png, String> {
    let mut keys = Vec::new();
    let header = walk(bytes, limits, |chunk| {
        if chunk.kind != *b"tEXt" {
            return Ok(());
        }
        // Each byte of the entry examined and copied.
        work::count(2 * chunk.data.len());
        let Some(nul) = chunk.data.iter().position(|&byte| byte == 0) else {
            return Err("a text chunk without a keyword".to_owned());
        };
        if nul == 0 || nul > 79 {
            return Err("a text keyword of an impossible length".to_owned());
        }
        // tEXt is Latin-1; keys and values here are ASCII in practice,
        // and a lossy read keeps anything else legible.
        keys.push((
            String::from_utf8_lossy(&chunk.data[..nul]).into_owned(),
            String::from_utf8_lossy(&chunk.data[nul + 1..]).into_owned(),
        ));
        Ok(())
    })?;
    Ok(Png {
        width: header.width,
        height: header.height,
        keys,
    })
}

/// One serialized chunk: length, type, data, CRC.
fn chunk(kind: [u8; 4], data: &[u8]) -> Vec<u8> {
    work::count(data.len());
    let mut out = Vec::with_capacity(12 + data.len());
    out.extend_from_slice(&u32::try_from(data.len()).unwrap_or(u32::MAX).to_be_bytes());
    out.extend_from_slice(&kind);
    out.extend_from_slice(data);
    out.extend_from_slice(&crc32::of(&[&kind, data]).to_be_bytes());
    out
}

/// A `tEXt` chunk for `keyword` and `value`.
fn text_chunk(keyword: &str, value: &str) -> Vec<u8> {
    let mut data = Vec::with_capacity(keyword.len() + 1 + value.len());
    data.extend_from_slice(keyword.as_bytes());
    data.push(0);
    data.extend_from_slice(value.as_bytes());
    chunk(*b"tEXt", &data)
}

/// Whether a `tEXt` chunk's keyword is one of `replaced`.
fn replaced_text(chunk: &Chunk<'_>, replaced: &[&str]) -> bool {
    if chunk.kind != *b"tEXt" {
        return false;
    }
    let Some(nul) = chunk.data.iter().position(|&byte| byte == 0) else {
        return false;
    };
    replaced.iter().any(|keyword| {
        work::count(1);
        keyword.as_bytes() == &chunk.data[..nul]
    })
}

/// `bytes` with `keys` as `tEXt` chunks right after `IHDR`, existing
/// entries of exactly the keys in `replaced` dropped — nothing else: the
/// unrelated `Thumb::*` fields other writers add (`Thumb::Mimetype`, the
/// image dimensions, …) stay — and no other byte touched. The error is
/// why the input is unsound (a non-PNG is refused, never installed).
pub(crate) fn splice(
    bytes: &[u8],
    keys: &[(&str, &str)],
    replaced: &[&str],
    limits: &Limits,
) -> Result<Vec<u8>, String> {
    let mut out = Vec::with_capacity(bytes.len() + 256);
    out.extend_from_slice(&SIGNATURE);
    let mut first = true;
    let _ = walk(bytes, limits, |seen| {
        if replaced_text(seen, replaced) {
            return Ok(());
        }
        work::count(seen.whole.len());
        out.extend_from_slice(seen.whole);
        if first {
            // Right after IHDR, which the walk guarantees comes first.
            for (keyword, value) in keys {
                out.extend_from_slice(&text_chunk(keyword, value));
            }
            first = false;
        }
        Ok(())
    })?;
    Ok(out)
}

/// A minimal, valid PNG (one gray pixel) carrying `keys`: the failure
/// marker the standard asks for.
pub(crate) fn minimal(keys: &[(&str, &str)]) -> Vec<u8> {
    let mut out = Vec::with_capacity(128);
    out.extend_from_slice(&SIGNATURE);
    // 1x1, bit depth 8, grayscale, default methods.
    let ihdr = [0, 0, 0, 1, 0, 0, 0, 1, 8, 0, 0, 0, 0];
    out.extend_from_slice(&chunk(*b"IHDR", &ihdr));
    for (keyword, value) in keys {
        out.extend_from_slice(&text_chunk(keyword, value));
    }
    // zlib, one stored deflate block: the filter byte and the pixel.
    let idat = [
        0x78, 0x01, 0x01, 0x02, 0x00, 0xFD, 0xFF, 0x00, 0x00, 0x00, 0x02, 0x00, 0x01,
    ];
    out.extend_from_slice(&chunk(*b"IDAT", &idat));
    out.extend_from_slice(&chunk(*b"IEND", &[]));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limits() -> Limits {
        Limits::default()
    }

    #[test]
    fn the_minimal_png_reads_back_sound_with_its_keys() {
        let marker = minimal(&[("Thumb::URI", "file:///x"), ("Thumb::MTime", "7")]);
        let png = read(&marker, &limits()).unwrap();
        assert_eq!(png.key("Thumb::URI"), Some("file:///x"));
        assert_eq!(png.key("Thumb::MTime"), Some("7"));
        assert_eq!(png.key("Thumb::Size"), None);
    }

    #[test]
    fn unsound_files_are_refused_with_reasons() {
        let good = minimal(&[]);
        let mut bad_crc = minimal(&[("Thumb::URI", "x")]);
        // The tEXt chunk's CRC is right after its data; flip one byte.
        let at = bad_crc.len() - 12 - 13 - 12 - 1; // last CRC byte of tEXt
        bad_crc[at] ^= 1;
        let cases: [(&[u8], &str); 6] = [
            (b"not a png", "no signature"),
            (&good[..20], "truncated"),
            (&bad_crc, "CRC of a tEXt chunk does not match"),
            (&[&good[..], b"junk"].concat(), "bytes after IEND"),
            (&SIGNATURE, "truncated chunk header"),
            (
                &{
                    let mut zero = minimal(&[]);
                    // Zero the IHDR width (offset 8 sig + 8 header).
                    zero[16..20].copy_from_slice(&[0, 0, 0, 0]);
                    // Fix the CRC so only the dimension is wrong.
                    let crc = crate::crc32::of(&[b"IHDR", &zero[16..29]]);
                    zero[29..33].copy_from_slice(&crc.to_be_bytes());
                    zero
                },
                "dimension of zero",
            ),
        ];
        for (bytes, expected) in cases {
            let refused = read(bytes, &limits()).unwrap_err();
            assert!(refused.contains(expected), "{refused}");
        }
        let capped = Limits {
            max_chunks: 2,
            ..Limits::default()
        };
        assert!(read(&good, &capped).unwrap_err().contains("chunks"));
    }

    #[test]
    fn image_data_is_required_and_ihdr_comes_once() {
        // IHDR + IEND with no IDAT: framed fine, but no image at all.
        let mut empty = Vec::from(&SIGNATURE[..]);
        let ihdr = [0, 0, 0, 1, 0, 0, 0, 1, 8, 0, 0, 0, 0];
        empty.extend_from_slice(&chunk(*b"IHDR", &ihdr));
        empty.extend_from_slice(&chunk(*b"IEND", &[]));
        assert!(
            read(&empty, &limits())
                .unwrap_err()
                .contains("no image data"),
            "structural, not decoding: the required chunks must exist"
        );
        let mut doubled = Vec::from(&SIGNATURE[..]);
        doubled.extend_from_slice(&chunk(*b"IHDR", &ihdr));
        doubled.extend_from_slice(&chunk(*b"IHDR", &ihdr));
        assert!(
            read(&doubled, &limits())
                .unwrap_err()
                .contains("second IHDR")
        );
    }

    #[test]
    fn splicing_replaces_thumb_keys_and_touches_nothing_else() {
        // A marker with old Thumb keys and a foreign tEXt entry.
        let mut source = minimal(&[("Thumb::URI", "old"), ("Software", "keepme")]);
        // Move the foreign chunk's bytes around? No — just splice.
        let spliced = splice(
            &source,
            &[("Thumb::URI", "file:///new"), ("Thumb::MTime", "9")],
            &["Thumb::URI", "Thumb::MTime", "Thumb::Size"],
            &limits(),
        )
        .unwrap();
        let png = read(&spliced, &limits()).unwrap();
        assert_eq!(png.key("Thumb::URI"), Some("file:///new"));
        assert_eq!(png.key("Thumb::MTime"), Some("9"));
        assert_eq!(png.key("Software"), Some("keepme"), "foreign keys stay");
        // Every non-Thumb byte of the source survives verbatim: strip the
        // Thumb chunks from both and compare.
        let strip =
            |bytes: &[u8]| splice(bytes, &[], &["Thumb::URI", "Thumb::MTime"], &limits()).unwrap();
        source = strip(&source);
        assert_eq!(source, strip(&spliced));
        // A non-PNG is refused, not installed.
        assert!(splice(b"junk", &[], &[], &limits()).is_err());
        // An unrelated Thumb field another writer added is not ours to
        // delete.
        let rich = minimal(&[("Thumb::URI", "old"), ("Thumb::Image::Width", "640")]);
        let spliced = splice(
            &rich,
            &[("Thumb::URI", "file:///new")],
            &["Thumb::URI", "Thumb::MTime", "Thumb::Size"],
            &limits(),
        )
        .unwrap();
        let png = read(&spliced, &limits()).unwrap();
        assert_eq!(png.key("Thumb::Image::Width"), Some("640"));
        assert_eq!(png.key("Thumb::URI"), Some("file:///new"));
    }

    #[test]
    fn duplicate_keywords_read_last_wins() {
        let marker = minimal(&[("Thumb::MTime", "1"), ("Thumb::MTime", "2")]);
        let png = read(&marker, &limits()).unwrap();
        assert_eq!(png.key("Thumb::MTime"), Some("2"));
    }
}

#[cfg(test)]
mod linearity {
    use crate::{Limits, work};

    #[test]
    fn walking_many_chunks_is_linear() {
        let keys = |n: usize| {
            let names: Vec<(String, String)> = (0..n)
                .map(|at| (format!("Key{at}"), format!("value {at}")))
                .collect();
            names
        };
        work::assert_linear(
            600,
            |n| {
                let keys = keys(n);
                let refs: Vec<(&str, &str)> = keys
                    .iter()
                    .map(|(key, value)| (key.as_str(), value.as_str()))
                    .collect();
                let marker = super::minimal(&refs);
                let limits = Limits {
                    max_chunks: usize::MAX,
                    ..Limits::default()
                };
                let (png, steps) = work::steps(|| super::read(&marker, &limits).unwrap());
                assert_eq!(png.keys.len(), n);
                (steps, marker.len())
            },
            // Per chunk a step and its framing; per byte: built, CRC-fed,
            // walked, CRC-checked and copied out.
            |n, bytes| 2 * (12 * (n + 4) + 8 * bytes),
        );
    }
}

#[cfg(test)]
mod review_regressions {

    use super::*;

    fn limits() -> Limits {
        Limits::default()
    }

    /// A marker with one IHDR field byte rewritten (offset into the 13).
    pub(super) fn with_header_byte(at: usize, value: u8) -> Vec<u8> {
        let mut bytes = minimal(&[]);
        // 8 signature + 8 chunk header.
        bytes[16 + at] = value;
        let crc = crate::crc32::of(&[b"IHDR", &bytes[16..29]]);
        bytes[29..33].copy_from_slice(&crc.to_be_bytes());
        bytes
    }

    #[test]
    fn illegal_header_fields_are_unsound() {
        // Bit depth 3 (offset 8), color type 7 (offset 9), compression 1,
        // filter 1, interlace 2.
        for (at, value, expected) in [
            (8, 3, "not a PNG combination"),
            (9, 7, "not a PNG combination"),
            (10, 1, "compression, filter or interlace"),
            (11, 1, "compression, filter or interlace"),
            (12, 2, "compression, filter or interlace"),
        ] {
            let refused = read(&with_header_byte(at, value), &limits()).unwrap_err();
            assert!(refused.contains(expected), "{at}: {refused}");
        }
        // The legal 16-bit grayscale combination stays sound.
        assert!(read(&with_header_byte(8, 16), &limits()).is_ok());
    }

    #[test]
    fn chunk_constraints_hold() {
        // A nonempty IEND.
        let mut fat_end = minimal(&[]);
        let end_at = fat_end.len() - 12;
        fat_end.truncate(end_at);
        fat_end.extend_from_slice(&chunk(*b"IEND", b"x"));
        assert!(
            read(&fat_end, &limits())
                .unwrap_err()
                .contains("IEND chunk with data"),
            "IEND carries no data"
        );
        // Image data split by another chunk.
        let mut split_data = minimal(&[]);
        let end_at = split_data.len() - 12;
        split_data.truncate(end_at);
        split_data.extend_from_slice(&text_chunk("Comment", "between"));
        let idat = [
            0x78, 0x01, 0x01, 0x02, 0x00, 0xFD, 0xFF, 0x00, 0x00, 0x00, 0x02, 0x00, 0x01,
        ];
        split_data.extend_from_slice(&chunk(*b"IDAT", &idat));
        split_data.extend_from_slice(&chunk(*b"IEND", &[]));
        assert!(
            read(&split_data, &limits()).unwrap_err().contains("split"),
            "IDAT runs must be consecutive"
        );
        // A chunk type that is not letters.
        let mut odd_kind = minimal(&[]);
        odd_kind[12] = b'1';
        assert!(
            read(&odd_kind, &limits())
                .unwrap_err()
                .contains("four letters")
        );
    }

    #[test]
    fn an_indexed_image_needs_its_palette_in_order() {
        // Color type 3 with no PLTE at all.
        let indexed = with_header_byte(9, 3);
        assert!(
            read(&indexed, &limits())
                .unwrap_err()
                .contains("without a palette"),
            "a palette is mandatory for indexed color"
        );
        // With a palette before the data, it is sound.
        let mut good = with_header_byte(9, 3);
        let after_ihdr = 8 + 8 + 13 + 4;
        let palette = chunk(*b"PLTE", &[0, 0, 0, 255, 255, 255]);
        good.splice(after_ihdr..after_ihdr, palette.iter().copied());
        assert!(
            read(&good, &limits()).is_ok(),
            "{:?}",
            read(&good, &limits())
        );
        // An impossible palette length, a second palette, a palette on
        // grayscale, and one after the data are all unsound.
        let mut odd_len = with_header_byte(9, 3);
        odd_len.splice(
            after_ihdr..after_ihdr,
            chunk(*b"PLTE", &[0, 0]).iter().copied(),
        );
        assert!(
            read(&odd_len, &limits())
                .unwrap_err()
                .contains("impossible length")
        );
        let mut doubled = good.clone();
        doubled.splice(after_ihdr..after_ihdr, palette.iter().copied());
        assert!(
            read(&doubled, &limits())
                .unwrap_err()
                .contains("second palette")
        );
        let mut gray = minimal(&[]);
        gray.splice(after_ihdr..after_ihdr, palette.iter().copied());
        assert!(read(&gray, &limits()).unwrap_err().contains("grayscale"));
        // An indexed image with only a late palette fails at the data
        // already (no palette yet); ordering shows on truecolor, where a
        // palette is optional but still must precede the data.
        let mut late = with_header_byte(9, 2);
        let before_iend = late.len() - 12;
        late.splice(before_iend..before_iend, palette.iter().copied());
        assert!(
            read(&late, &limits())
                .unwrap_err()
                .contains("after the image data")
        );
    }

    #[test]
    fn hostile_chunk_lengths_are_errors_not_overflow() {
        // A length over the format's 2^31 - 1 cap; on 32-bit targets an
        // unchecked offset sum would wrap before slicing caught it.
        let mut hostile = Vec::from(&SIGNATURE[..]);
        hostile.extend_from_slice(&0xFFFF_FFF0u32.to_be_bytes());
        hostile.extend_from_slice(b"IHDR");
        let refused = read(&hostile, &limits()).unwrap_err();
        assert!(refused.contains("over the format's limit"), "{refused}");
    }
}

#[cfg(test)]
mod third_round_regressions {
    use super::review_regressions::with_header_byte;
    use super::*;

    fn limits() -> Limits {
        Limits::default()
    }

    #[test]
    fn an_indexed_palette_fits_its_bit_depth() {
        // Color type 3 (offset 9) at bit depth 1 (offset 8): two entries
        // at most.
        let mut one_bit = with_header_byte(9, 3);
        one_bit[16 + 8] = 1;
        let crc = crate::crc32::of(&[b"IHDR", &one_bit[16..29]]);
        one_bit[29..33].copy_from_slice(&crc.to_be_bytes());
        let after_ihdr = 8 + 8 + 13 + 4;
        let mut three = one_bit.clone();
        three.splice(
            after_ihdr..after_ihdr,
            chunk(*b"PLTE", &[0; 9]).iter().copied(),
        );
        assert!(
            read(&three, &limits())
                .unwrap_err()
                .contains("3 palette entries on a 1-bit"),
            "three entries cannot be indexed by one bit"
        );
        let mut two = one_bit;
        two.splice(
            after_ihdr..after_ihdr,
            chunk(*b"PLTE", &[0; 6]).iter().copied(),
        );
        assert!(read(&two, &limits()).is_ok(), "{:?}", read(&two, &limits()));
    }

    #[test]
    fn unknown_critical_chunks_stop_the_walk() {
        let after_ihdr = 8 + 8 + 13 + 4;
        let mut custom = minimal(&[]);
        custom.splice(
            after_ihdr..after_ihdr,
            chunk(*b"ABCD", b"whatever").iter().copied(),
        );
        assert!(
            read(&custom, &limits())
                .unwrap_err()
                .contains("unknown critical chunk (ABCD)"),
            "an editor must stop on a critical chunk it cannot understand"
        );
        // An unknown ancillary chunk (lowercase first letter) is fine.
        let mut ancillary = minimal(&[]);
        ancillary.splice(
            after_ihdr..after_ihdr,
            chunk(*b"aBCD", b"whatever").iter().copied(),
        );
        assert!(read(&ancillary, &limits()).is_ok());
        // Splicing refuses it too: nothing unknown-critical is installed.
        assert!(
            splice(&custom, &[], &[], &limits())
                .unwrap_err()
                .contains("unknown critical chunk")
        );
    }
}
