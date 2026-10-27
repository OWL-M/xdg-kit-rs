//! Byte-exact escaping: the URL escaping (RFC 2396, section 2) of
//! `Path` values and `directorysizes` names, and the Desktop Entry
//! spec's string escapes.

/// Whether RFC 2396 leaves `byte` as it is: letters, digits and the
/// "mark" characters. `/` is kept in paths too (a path separator, never
/// part of a name).
fn unreserved(byte: u8) -> bool {
    byte.is_ascii_alphanumeric()
        || matches!(
            byte,
            b'-' | b'_' | b'.' | b'!' | b'~' | b'*' | b'\'' | b'(' | b')'
        )
}

/// `bytes` with every byte but the unreserved ones (and `/` when
/// `keep_slash`) written as `%XX`, in uppercase hex. Never produces a
/// backslash, a control character or a byte above ASCII, so the result
/// needs no further escaping as a Desktop Entry string.
pub(crate) fn percent_encode(bytes: &[u8], keep_slash: bool) -> Vec<u8> {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = Vec::with_capacity(bytes.len());
    for &byte in bytes {
        if unreserved(byte) || (keep_slash && byte == b'/') {
            out.push(byte);
        } else {
            out.extend_from_slice(&[
                b'%',
                HEX[usize::from(byte >> 4)],
                HEX[usize::from(byte & 15)],
            ]);
        }
    }
    out
}

/// Why escaped text was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum EscapeProblem {
    /// A `%` not followed by two hex digits, at this byte offset of the
    /// value.
    Percent(usize),
    /// A backslash not starting one of the Desktop Entry spec's string
    /// escapes (`\s`, `\n`, `\t`, `\r`, `\\`), at this byte offset.
    Backslash(usize),
}

impl std::fmt::Display for EscapeProblem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Percent(at) => write!(f, "a '%' at byte {at} is not followed by two hex digits"),
            Self::Backslash(at) => write!(f, "the backslash at byte {at} starts no string escape"),
        }
    }
}

/// Decodes `%XX` escapes, byte for byte. Any other byte stands for itself:
/// writers should escape everything outside RFC 2396's unreserved set, but
/// a raw byte is unambiguous, so it is read as written.
pub(crate) fn percent_decode(text: &[u8]) -> Result<Vec<u8>, EscapeProblem> {
    let hex = |byte: u8| match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    };
    let mut out = Vec::with_capacity(text.len());
    let mut at = 0;
    while let Some(&byte) = text.get(at) {
        if byte == b'%' {
            let high = text.get(at + 1).copied().and_then(hex);
            let low = text.get(at + 2).copied().and_then(hex);
            let (Some(high), Some(low)) = (high, low) else {
                return Err(EscapeProblem::Percent(at));
            };
            out.push(high << 4 | low);
            at += 3;
        } else {
            out.push(byte);
            at += 1;
        }
    }
    Ok(out)
}

/// Undoes the Desktop Entry spec's string escapes (`\s`, `\n`, `\t`, `\r`,
/// `\\`); any other backslash is an error.
pub(crate) fn unescape_string(text: &[u8]) -> Result<Vec<u8>, EscapeProblem> {
    let mut out = Vec::with_capacity(text.len());
    let mut at = 0;
    while let Some(&byte) = text.get(at) {
        if byte == b'\\' {
            let escaped = match text.get(at + 1) {
                Some(b's') => b' ',
                Some(b'n') => b'\n',
                Some(b't') => b'\t',
                Some(b'r') => b'\r',
                Some(b'\\') => b'\\',
                _ => return Err(EscapeProblem::Backslash(at)),
            };
            out.push(escaped);
            at += 2;
        } else {
            out.push(byte);
            at += 1;
        }
    }
    Ok(out)
}
