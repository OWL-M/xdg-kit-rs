//! The parts of XML 1.0's grammar the reader leaves unchecked: the XML
//! declaration's fields, names, the syntax of start tags and their
//! attribute values, processing instruction targets, and `]]>` in text.
//! Each check takes the raw text of one construct, as the reader gives it.

use super::Failure;
use crate::work;

/// `[4] NameStartChar`.
fn name_start(c: char) -> bool {
    matches!(c, ':' | 'A'..='Z' | '_' | 'a'..='z'
        | '\u{C0}'..='\u{D6}' | '\u{D8}'..='\u{F6}' | '\u{F8}'..='\u{2FF}'
        | '\u{370}'..='\u{37D}' | '\u{37F}'..='\u{1FFF}' | '\u{200C}'..='\u{200D}'
        | '\u{2070}'..='\u{218F}' | '\u{2C00}'..='\u{2FEF}' | '\u{3001}'..='\u{D7FF}'
        | '\u{F900}'..='\u{FDCF}' | '\u{FDF0}'..='\u{FFFD}' | '\u{10000}'..='\u{EFFFF}')
}

/// `[4a] NameChar`.
fn name_char(c: char) -> bool {
    name_start(c)
        || matches!(c, '-' | '.' | '0'..='9' | '\u{B7}'
            | '\u{300}'..='\u{36F}' | '\u{203F}'..='\u{2040}')
}

/// `[3] S`.
fn space(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\r' | '\n')
}

/// Whether `text` is a `[5] Name`.
pub(super) fn is_name(text: &str) -> bool {
    let mut chars = text.chars();
    chars.next().is_some_and(name_start) && chars.all(name_char)
}

/// A cursor over the raw text of one construct.
struct Cursor<'a> {
    rest: &'a str,
}

impl<'a> Cursor<'a> {
    /// Skips whitespace; whether there was any.
    fn space(&mut self) -> bool {
        let trimmed = self.rest.trim_start_matches(space);
        let skipped = trimmed.len() != self.rest.len();
        self.rest = trimmed;
        skipped
    }

    /// The `Name` at the cursor, if one is there.
    fn name(&mut self) -> Option<&'a str> {
        let end = self
            .rest
            .char_indices()
            .find(|&(_, c)| !name_char(c))
            .map_or(self.rest.len(), |(at, _)| at);
        let name = &self.rest[..end];
        is_name(name).then(|| {
            self.rest = &self.rest[end..];
            name
        })
    }

    /// `[25] Eq` then a quoted value; the value without its quotes.
    fn eq_quoted(&mut self) -> Option<&'a str> {
        self.space();
        self.rest = self.rest.strip_prefix('=')?;
        self.space();
        let quote = self
            .rest
            .chars()
            .next()
            .filter(|c| matches!(c, '"' | '\''))?;
        let body = &self.rest[1..];
        let end = body.find(quote)?;
        self.rest = &body[end + 1..];
        Some(&body[..end])
    }
}

/// Checks `[23] XMLDecl` (`raw` is its text between `<?` and `?>`): a
/// version `1.` and digits, then optionally an encoding, which must be
/// UTF-8 since menu files are read as that, and a standalone `yes` or `no`,
/// in that order.
pub(super) fn declaration(raw: &str, line: usize) -> Result<(), Failure> {
    work::count(raw.len());
    let bad = |what: &str| Failure::malformed(line, format!("the XML declaration {what}"));
    let mut cursor = Cursor {
        rest: raw.strip_prefix("xml").ok_or_else(|| bad("is not one"))?,
    };
    let field = |cursor: &mut Cursor<'_>, name: &str| -> Result<Option<String>, Failure> {
        let before = cursor.rest;
        if !cursor.space() || cursor.name() != Some(name) {
            cursor.rest = before;
            return Ok(None);
        }
        let value = cursor
            .eq_quoted()
            .ok_or_else(|| bad(&format!("has a malformed {name}")))?;
        Ok(Some(value.to_owned()))
    };
    let version = field(&mut cursor, "version")?.ok_or_else(|| bad("has no version first"))?;
    let digits = version.strip_prefix("1.").unwrap_or_default();
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return Err(bad("has a version that is not 1.x"));
    }
    if let Some(encoding) = field(&mut cursor, "encoding")?
        && !encoding.eq_ignore_ascii_case("UTF-8")
    {
        return Err(bad(&format!(
            "declares the encoding {}, but menu files are read as UTF-8",
            crate::error::shown(&encoding)
        )));
    }
    if let Some(standalone) = field(&mut cursor, "standalone")?
        && !matches!(standalone.as_str(), "yes" | "no")
    {
        return Err(bad("has a standalone that is not yes or no"));
    }
    cursor.space();
    if cursor.rest.is_empty() {
        Ok(())
    } else {
        Err(bad(
            "holds something other than version, encoding and standalone, in that order",
        ))
    }
}

/// Checks `[40] STag` or `[44] EmptyElemTag` (`raw` is the text between
/// `<` and `>` or `/>`): a `Name`, then attributes, each after whitespace,
/// each `Name Eq AttValue`, whose value holds no `<` and uses `&` only to
/// start a reference.
pub(super) fn start_tag(raw: &str, line: usize) -> Result<(), Failure> {
    work::count(raw.len());
    let bad = |what: String| Failure::malformed(line, what);
    let mut cursor = Cursor { rest: raw };
    if cursor.name().is_none() {
        return Err(bad(format!(
            "<{}> is not an element name",
            crate::error::shown(raw.split(space).next().unwrap_or_default())
        )));
    }
    loop {
        let spaced = cursor.space();
        if cursor.rest.is_empty() {
            return Ok(());
        }
        let Some(name) = cursor.name().filter(|_| spaced) else {
            return Err(bad(
                "an attribute that is not a name after whitespace".to_owned()
            ));
        };
        let Some(value) = cursor.eq_quoted() else {
            return Err(bad(format!(
                "the attribute {} has no quoted value",
                crate::error::shown(name)
            )));
        };
        if value.contains('<') {
            return Err(bad(format!(
                "the value of attribute {} holds a `<`",
                crate::error::shown(name)
            )));
        }
        references(value).map_err(|()| {
            bad(format!(
                "the value of attribute {} holds a `&` that starts no reference",
                crate::error::shown(name)
            ))
        })?;
    }
}

/// Whether each `&` in `value` starts `[67] Reference`: `&Name;`, `&#`
/// digits `;` or `&#x` hex digits `;`.
fn references(value: &str) -> Result<(), ()> {
    let mut rest = value;
    while let Some(at) = rest.find('&') {
        let after = &rest[at + 1..];
        let end = after.find(';').ok_or(())?;
        let body = &after[..end];
        let fine = if let Some(hex) = body.strip_prefix("#x") {
            !hex.is_empty() && hex.bytes().all(|b| b.is_ascii_hexdigit())
        } else if let Some(digits) = body.strip_prefix('#') {
            !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit())
        } else {
            is_name(body)
        };
        if !fine {
            return Err(());
        }
        rest = &after[end + 1..];
    }
    Ok(())
}

/// Checks `[16] PI` (`raw` is its text between `<?` and `?>`): its target
/// a `Name` that is not `xml` in any case, then whitespace before any
/// content.
pub(super) fn instruction(raw: &str, line: usize) -> Result<(), Failure> {
    work::count(raw.len());
    let mut cursor = Cursor { rest: raw };
    let target = cursor.name();
    let fine = target.is_some_and(|target| !target.eq_ignore_ascii_case("xml"))
        && (cursor.rest.is_empty() || cursor.space());
    if fine {
        Ok(())
    } else {
        Err(Failure::malformed(
            line,
            "a processing instruction without a usable target",
        ))
    }
}

/// Checks raw character data: `]]>` cannot appear in it (`[14] CharData`).
pub(super) fn text(raw: &str, line: usize) -> Result<(), Failure> {
    work::count(raw.len());
    if raw.contains("]]>") {
        return Err(Failure::malformed(line, "`]]>` in text"));
    }
    Ok(())
}

/// Checks that `raw`, the text of a `what`, holds only characters XML
/// allows (`[2] Char`).
pub(super) fn characters(raw: &str, line: usize, what: &str) -> Result<(), Failure> {
    work::count(raw.len());
    match raw.chars().find(|&c| !super::is_xml_char(c)) {
        Some(bad) => Err(Failure::malformed(
            line,
            format!("{what} holds U+{:04X}, which XML cannot", u32::from(bad)),
        )),
        None => Ok(()),
    }
}

/// Whether `text` is only `[3] S`: what may stand between markup outside
/// the root element (no other whitespace, no references).
pub(super) fn is_space(text: &str) -> bool {
    text.chars().all(space)
}

/// Checks `[28] doctypedecl` (`raw` is all of it, `<!DOCTYPE` to `>`): a
/// `Name`, optionally an external ID (`SYSTEM` and a literal, or `PUBLIC`,
/// a public ID literal and a literal), optionally an internal subset. The
/// internal subset may hold whitespace, comments and processing
/// instructions only: a declaration in it could give the document entities
/// or attribute defaults, which are never applied, so one is refused.
pub(super) fn document_type(raw: &str, line: usize) -> Result<(), Failure> {
    work::count(raw.len());
    let bad =
        |what: &str| Failure::malformed(line, format!("the document type declaration {what}"));
    let body = raw
        .strip_prefix("<!DOCTYPE")
        .and_then(|body| body.strip_suffix('>'))
        .ok_or_else(|| bad("is not written `<!DOCTYPE …>`"))?;
    let mut cursor = Cursor { rest: body };
    if !cursor.space() || cursor.name().is_none() {
        return Err(bad("has no name"));
    }
    let spaced = cursor.space();
    let before = cursor.rest;
    match cursor.name() {
        Some("SYSTEM") if spaced => {
            literal(&mut cursor, false).ok_or_else(|| bad("has a malformed system literal"))?;
        }
        Some("PUBLIC") if spaced => {
            literal(&mut cursor, true).ok_or_else(|| bad("has a malformed public ID"))?;
            literal(&mut cursor, false).ok_or_else(|| bad("has a malformed system literal"))?;
        }
        Some(_) => {
            return Err(bad(
                "has something other than SYSTEM or PUBLIC after its name",
            ));
        }
        None => cursor.rest = before,
    }
    cursor.space();
    if let Some(subset) = cursor.rest.strip_prefix('[') {
        let end = subset
            .rfind(']')
            .ok_or_else(|| bad("has an unclosed internal subset"))?;
        if !is_space(&subset[end + 1..]) {
            return Err(bad("has something after its internal subset"));
        }
        internal_subset(&subset[..end], line)?;
    } else if !cursor.rest.is_empty() {
        return Err(bad("has something it cannot hold"));
    }
    Ok(())
}

/// A quoted literal after whitespace: `[12] PubidLiteral` when `public`,
/// else `[11] SystemLiteral`.
fn literal(cursor: &mut Cursor<'_>, public: bool) -> Option<()> {
    if !cursor.space() {
        return None;
    }
    let quote = cursor
        .rest
        .chars()
        .next()
        .filter(|c| matches!(c, '"' | '\''))?;
    let body = &cursor.rest[1..];
    let end = body.find(quote)?;
    let pubid = |c: char| {
        c.is_ascii_alphanumeric()
            || matches!(c, ' ' | '\r' | '\n')
            || "-'()+,./:=?;!*#@$_%".contains(c)
    };
    if public && !body[..end].chars().all(pubid) {
        return None;
    }
    cursor.rest = &body[end + 1..];
    Some(())
}

/// Checks an internal subset: only whitespace, comments and processing
/// instructions.
fn internal_subset(subset: &str, line: usize) -> Result<(), Failure> {
    let mut rest = subset;
    loop {
        rest = rest.trim_start_matches(space);
        if rest.is_empty() {
            return Ok(());
        }
        if let Some(comment) = rest.strip_prefix("<!--") {
            let end = comment.find("-->").ok_or_else(|| {
                Failure::malformed(line, "an unclosed comment in the document type")
            })?;
            if comment[..end].contains("--") || comment[..end].ends_with('-') {
                return Err(Failure::malformed(line, "a comment holding `--`"));
            }
            rest = &comment[end + 3..];
        } else if let Some(pi) = rest.strip_prefix("<?") {
            let end = pi.find("?>").ok_or_else(|| {
                Failure::malformed(
                    line,
                    "an unclosed processing instruction in the document type",
                )
            })?;
            instruction(&pi[..end], line)?;
            rest = &pi[end + 2..];
        } else {
            return Err(Failure::malformed(
                line,
                "the document type declares something (entities, attribute defaults …), which \
                 is never applied",
            ));
        }
    }
}
