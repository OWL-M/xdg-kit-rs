//! Glob patterns as `fnmatch(3)` reads them with no flags (spec "The glob
//! files": "The format of the glob pattern is as for fnmatch(3)"), following
//! glibc where POSIX leaves room.
//!
//! `*` matches any run (also of `/` and a leading `.`), `?` one item, and
//! `\` makes the next item literal (a trailing `\` matches nothing).
//! `[...]` matches one item from a set, negated by a leading `!` or `^`; a
//! `]` right after the opening (or the negation) is a member. Members are
//! items, `\`-escaped items, ranges (`a-z`, compared by code point), classes
//! (`[:digit:]`; an unknown class matches nothing), and `[=c=]` and `[.c.]`,
//! which stand for `c` itself (C-locale collation). A `[` without a closing
//! `]` stands for itself.
//!
//! Items are characters when the name is UTF-8, and classes use Unicode
//! properties for them. A name that is not UTF-8 is matched byte by byte,
//! with ASCII classes, and only against ASCII patterns: it never matches a
//! pattern with other characters.
//!
//! A case-insensitive pattern is matched against a name lowercased per
//! character, without context (ASCII only when it is not UTF-8). Its
//! literals are lowercased the same way. A set member compares its
//! one-to-one lowercase (itself when that is not one character) with the
//! name's character: `[Σ]` matches `σ` but not `ς`, and `[İ]` (which
//! lowercases to two characters) matches nothing. A range or a class tests
//! the name's character and its one-to-one lowercase and uppercase:
//! `[Σ-Σ]` matches `ς`. `[:upper:]` and `[:lower:]` match any letter that
//! has case.

use crate::work;

/// Whether `name` matches `pattern`, case-sensitively.
#[cfg(any(test, fuzzing))]
pub(crate) fn matches(pattern: &str, name: &[u8]) -> bool {
    Pattern::new(pattern, false).matches(&Name::new(name))
}

/// Where the pattern at the start of `text` ends, in bytes: at the first
/// `:` read as an item of its own (not escaped, not in a set), else at the
/// end. A `[` that no `]` closes is itself, so a later `:` still ends the
/// pattern; one that can never match takes the rest of `text`, as
/// [`parse`] reads it. A set that a trailing `\` keeps from closing
/// (`[:\:cs`, `x[:a\:cs`) leaves its `[` itself and its `:` bare, so the
/// pattern ends there: the format's accepted ambiguity.
pub(crate) fn pattern_end(text: &str) -> usize {
    // Each byte decoded and copied twice.
    work::count(2 * text.len());
    let places: Vec<usize> = text.char_indices().map(|(at, _)| at).collect();
    let chars: Vec<char> = text.chars().collect();
    let mut sets = None;
    let mut at = 0;
    while let Some(&symbol) = chars.get(at) {
        work::count(1);
        if symbol == ':' {
            return places[at];
        }
        at += 1;
        if symbol == '\\' {
            at += 1;
        } else if symbol == '['
            && let Some((_, used)) = sets.get_or_insert_with(|| Sets::new(&chars)).bracket(at)
        {
            at += used;
        }
    }
    text.len()
}

/// A pattern parsed once, to be matched against many names.
#[derive(Debug)]
pub(crate) struct Pattern {
    chars: Vec<Item<char>>,
    /// The pattern over bytes when it is all ASCII: only then can a name
    /// that is not UTF-8 match.
    bytes: Option<Vec<Item<u8>>>,
    /// Case-insensitive: names given are lowercased.
    fold: bool,
}

impl Pattern {
    /// Parses `pattern`, using case-insensitive matching when `fold` is set.
    pub(crate) fn new(pattern: &str, fold: bool) -> Self {
        // Each byte decoded and copied, then checked for ASCII.
        work::count(2 * pattern.len());
        let chars: Vec<char> = pattern.chars().collect();
        Self {
            chars: parse(&chars, fold),
            bytes: pattern.is_ascii().then(|| parse(pattern.as_bytes(), fold)),
            fold,
        }
    }

    /// Whether `name` matches this pattern. A case-insensitive pattern
    /// wants the name lowercased per character.
    pub(crate) fn matches(&self, name: &Name<'_>) -> bool {
        match name {
            Name::Chars(name) => match_items(&self.chars, name, self.fold),
            Name::Bytes(name) => self
                .bytes
                .as_deref()
                .is_some_and(|pattern| match_items(pattern, name, self.fold)),
        }
    }
}

/// A name split into what patterns compare: characters when it is UTF-8,
/// else bytes.
pub(crate) enum Name<'a> {
    Chars(Vec<char>),
    Bytes(&'a [u8]),
}

impl<'a> Name<'a> {
    pub(crate) fn new(name: &'a [u8]) -> Self {
        match std::str::from_utf8(name) {
            Ok(text) => Self::Chars(text.chars().collect()),
            Err(_) => Self::Bytes(name),
        }
    }
}

/// One pattern item.
#[derive(Debug)]
enum Item<T> {
    Star,
    Any,
    Set {
        members: Vec<Member<T>>,
        negated: bool,
    },
    Literal(T),
    /// Matches nothing: a trailing `\`, or a bracket glibc gives up on.
    Never,
}

/// One member of a `[...]` set.
#[derive(Debug)]
enum Member<T> {
    One(T),
    Range(T, T),
    Class(Class),
    /// An unknown class or collating element: when reached, the set does
    /// not match, negated or not (as glibc does).
    Invalid,
}

/// The POSIX character classes.
#[derive(Debug, Clone, Copy)]
enum Class {
    Alnum,
    Alpha,
    Blank,
    Cntrl,
    Digit,
    Graph,
    Lower,
    Print,
    Punct,
    Space,
    Upper,
    Xdigit,
}

impl Class {
    fn named(name: &str) -> Option<Self> {
        Some(match name {
            "alnum" => Self::Alnum,
            "alpha" => Self::Alpha,
            "blank" => Self::Blank,
            "cntrl" => Self::Cntrl,
            "digit" => Self::Digit,
            "graph" => Self::Graph,
            "lower" => Self::Lower,
            "print" => Self::Print,
            "punct" => Self::Punct,
            "space" => Self::Space,
            "upper" => Self::Upper,
            "xdigit" => Self::Xdigit,
            _ => return None,
        })
    }
}

trait Symbol: Copy + Eq + Ord {
    fn is(self, ascii: u8) -> bool;
    /// The ASCII letter this is, if it is one.
    fn ascii(self) -> Option<char>;
    fn in_class(self, class: Class) -> bool;
    /// Lowercased, as names are for case-insensitive patterns.
    fn lowercase(self) -> impl Iterator<Item = Self>;
    /// Returns the lowercase and uppercase forms when each is one symbol;
    /// otherwise returns the original symbol for that form.
    fn lower(self) -> Self;
    fn upper(self) -> Self;
    /// A letter with case: what `[:upper:]` and `[:lower:]` match
    /// case-insensitively.
    fn has_case(self) -> bool;
}

/// The only character of `mapped`, if it has just one.
fn single(mut mapped: impl Iterator<Item = char>) -> Option<char> {
    match (mapped.next(), mapped.next()) {
        (Some(only), None) => Some(only),
        _ => None,
    }
}

impl Symbol for char {
    fn is(self, ascii: u8) -> bool {
        self == char::from(ascii)
    }

    fn ascii(self) -> Option<char> {
        self.is_ascii().then_some(self)
    }

    fn lowercase(self) -> impl Iterator<Item = Self> {
        self.to_lowercase()
    }

    fn lower(self) -> Self {
        single(self.to_lowercase()).unwrap_or(self)
    }

    fn upper(self) -> Self {
        single(self.to_uppercase()).unwrap_or(self)
    }

    fn has_case(self) -> bool {
        self.is_lowercase() || self.is_uppercase()
    }

    fn in_class(self, class: Class) -> bool {
        match class {
            Class::Alnum => self.is_alphanumeric(),
            Class::Alpha => self.is_alphabetic(),
            Class::Blank => matches!(self, ' ' | '\t'),
            Class::Cntrl => self.is_control(),
            Class::Digit => self.is_ascii_digit(),
            Class::Graph => !self.is_control() && !self.is_whitespace(),
            Class::Lower => self.is_lowercase(),
            Class::Print => !self.is_control(),
            Class::Punct => !self.is_control() && !self.is_whitespace() && !self.is_alphanumeric(),
            Class::Space => self.is_whitespace(),
            Class::Upper => self.is_uppercase(),
            Class::Xdigit => self.is_ascii_hexdigit(),
        }
    }
}

impl Symbol for u8 {
    fn is(self, ascii: u8) -> bool {
        self == ascii
    }

    fn ascii(self) -> Option<char> {
        self.is_ascii().then_some(char::from(self))
    }

    fn lowercase(self) -> impl Iterator<Item = Self> {
        std::iter::once(self.to_ascii_lowercase())
    }

    fn lower(self) -> Self {
        self.to_ascii_lowercase()
    }

    fn upper(self) -> Self {
        self.to_ascii_uppercase()
    }

    fn has_case(self) -> bool {
        self.is_ascii_alphabetic()
    }

    fn in_class(self, class: Class) -> bool {
        match class {
            Class::Alnum => self.is_ascii_alphanumeric(),
            Class::Alpha => self.is_ascii_alphabetic(),
            Class::Blank => matches!(self, b' ' | b'\t'),
            Class::Cntrl => self.is_ascii_control(),
            Class::Digit => self.is_ascii_digit(),
            Class::Graph => self.is_ascii_graphic(),
            Class::Lower => self.is_ascii_lowercase(),
            Class::Print => self.is_ascii_graphic() || self == b' ',
            Class::Punct => self.is_ascii_punctuation(),
            Class::Space => self.is_ascii_whitespace() || self == 0x0b,
            Class::Upper => self.is_ascii_uppercase(),
            Class::Xdigit => self.is_ascii_hexdigit(),
        }
    }
}

/// The items of `pattern`; with `fold`, those outside sets lowercased.
fn parse<T: Symbol>(pattern: &[T], fold: bool) -> Vec<Item<T>> {
    let mut items = Vec::new();
    let mut sets = None;
    let mut at = 0;
    while let Some(&symbol) = pattern.get(at) {
        work::count(1);
        at += 1;
        let item = if symbol.is(b'*') {
            Item::Star
        } else if symbol.is(b'?') {
            Item::Any
        } else if symbol.is(b'\\') {
            match pattern.get(at) {
                Some(&next) => {
                    at += 1;
                    Item::Literal(next)
                }
                None => Item::Never,
            }
        } else if symbol.is(b'[') {
            match sets.get_or_insert_with(|| Sets::new(pattern)).bracket(at) {
                Some((item, used)) => {
                    at += used;
                    item
                }
                None => Item::Literal(symbol),
            }
        } else {
            Item::Literal(symbol)
        };
        match item {
            Item::Literal(symbol) if fold => items.extend(symbol.lowercase().map(Item::Literal)),
            item => items.push(item),
        }
    }
    items
}

/// Reads the `[...]` sets in time linear in the pattern's length,
/// regardless of how many `[` remain unclosed.
struct Sets<'a, T> {
    pattern: &'a [T],
    /// Positions visited while reading sets. Reaching a visited position
    /// would repeat a previous set's scan, which found no closing `]`.
    /// A closed set, or one that can never match, consumes the pattern past
    /// this position, so later sets start after it.
    passed: Vec<bool>,
    /// From each place, where the next `.]` starts (the pattern's length if
    /// there is none).
    dot_close: Vec<usize>,
}

impl<'a, T: Symbol> Sets<'a, T> {
    fn new(pattern: &'a [T]) -> Self {
        // Each place marked unpassed, and looked at for a `.]`.
        work::count(2 * (pattern.len() + 1));
        let mut dot_close = vec![pattern.len(); pattern.len() + 1];
        for at in (0..pattern.len().saturating_sub(1)).rev() {
            dot_close[at] = if pattern[at].is(b'.') && pattern[at + 1].is(b']') {
                at
            } else {
                dot_close[at + 1]
            };
        }
        Self {
            pattern,
            passed: vec![false; pattern.len() + 1],
            dot_close,
        }
    }

    /// The set that starts at `start` (just after a `[`) and how many
    /// symbols it takes, or `None` when no `]` closes it (the `[` then
    /// stands for itself).
    fn bracket(&mut self, start: usize) -> Option<(Item<T>, usize)> {
        let rest = &self.pattern[start..];
        let negated = rest.first().is_some_and(|&c| c.is(b'!') || c.is(b'^'));
        let mut at = start + usize::from(negated);
        let mut members = Vec::new();
        let mut first = true;
        loop {
            work::count(1);
            if self.passed[at] {
                return None;
            }
            self.passed[at] = true;
            let &symbol = self.pattern.get(at)?;
            if symbol.is(b']') && !first {
                return Some((Item::Set { members, negated }, at + 1 - start));
            }
            first = false;
            let (low, used) = match self.element(at) {
                Element::Symbol(low, used) => (low, used),
                Element::Member(member, used) => {
                    members.push(member);
                    at += used;
                    continue;
                }
                Element::Never => return Some((Item::Never, rest.len())),
            };
            at += used;
            // A range, unless the `-` is last in the set.
            let dash = self.pattern.get(at).is_some_and(|c| c.is(b'-'));
            let after = self.pattern.get(at + 1);
            if dash && after.is_some_and(|c| !c.is(b']')) {
                match self.element(at + 1) {
                    Element::Symbol(high, used) => {
                        members.push(Member::Range(low, high));
                        at += 1 + used;
                    }
                    Element::Member(..) => {
                        // A class cannot end a range.
                        members.push(Member::Invalid);
                        at += 1;
                    }
                    Element::Never => return Some((Item::Never, rest.len())),
                }
            } else {
                members.push(Member::One(low));
            }
        }
    }

    /// The set element at `at`, which is in the pattern.
    fn element(&self, at: usize) -> Element<T> {
        let rest = &self.pattern[at..];
        let symbol = rest[0];
        if symbol.is(b'\\') {
            return match rest.get(1) {
                Some(&next) => Element::Symbol(next, 2),
                None => Element::Never,
            };
        }
        if symbol.is(b'[') {
            match rest.get(1) {
                Some(c) if c.is(b':') => {
                    // Lowercase letters (glibc stops at `z`) then `:]`; anything
                    // else makes the `[` an ordinary member.
                    let mut name = String::new();
                    for (offset, &c) in rest[2..].iter().enumerate() {
                        // The symbol read, and copied into the name.
                        work::count(2);
                        if c.is(b':') && rest.get(2 + offset + 1).is_some_and(|c| c.is(b']')) {
                            let member = Class::named(&name).map_or(Member::Invalid, Member::Class);
                            return Element::Member(member, 2 + offset + 2);
                        }
                        match c.ascii() {
                            Some(letter @ 'a'..='y') => name.push(letter),
                            _ => break,
                        }
                    }
                }
                Some(c) if c.is(b'=') => {
                    if let (Some(&inner), Some(close), Some(end)) =
                        (rest.get(2), rest.get(3), rest.get(4))
                        && close.is(b'=')
                        && end.is(b']')
                    {
                        return Element::Symbol(inner, 5);
                    }
                }
                Some(c) if c.is(b'.') => {
                    // Up to `.]`; only a single symbol names an element.
                    let close = self.dot_close[at + 2];
                    if close == self.pattern.len() {
                        return Element::Never;
                    }
                    return match close - (at + 2) {
                        1 => Element::Symbol(rest[2], 5),
                        inner => Element::Member(Member::Invalid, 2 + inner + 2),
                    };
                }
                _ => {}
            }
        }
        Element::Symbol(symbol, 1)
    }
}

enum Element<T> {
    /// One symbol (plain, escaped, `[=c=]` or `[.c.]`) and the symbols it
    /// takes.
    Symbol(T, usize),
    /// A class (or an invalid one) and the symbols it takes.
    Member(Member<T>, usize),
    /// The pattern can never match from here.
    Never,
}

/// Whether a set matches `symbol`; with `fold`, case-insensitively.
fn set_matches<T: Symbol>(members: &[Member<T>], negated: bool, symbol: T, fold: bool) -> bool {
    let variants = if fold {
        [symbol, symbol.lower(), symbol.upper()]
    } else {
        [symbol; 3]
    };
    for member in members {
        work::count(1);
        let hit = match *member {
            Member::One(one) if fold => one.lower() == symbol.lower(),
            Member::One(one) => one == symbol,
            Member::Range(low, high) => variants.iter().any(|&v| low <= v && v <= high),
            Member::Class(Class::Upper | Class::Lower) if fold => {
                variants.iter().any(|&v| v.has_case())
            }
            Member::Class(class) => variants.iter().any(|&v| v.in_class(class)),
            Member::Invalid => return false,
        };
        if hit {
            return !negated;
        }
    }
    negated
}

/// Matching that backtracks only to the last `*`, never recursively.
/// After a `*`, the items up to the next one are tried at successive
/// places of the name, and no try starts before the one before it: at
/// most (name length + `*`s + 1) tries, each costing at most the longest
/// run of items after a `*` (set members included) plus one step, on top
/// of the items before the first `*`. So the work is linear in the name
/// for a given pattern, but pattern × name at worst: `*aa…ab` against
/// `aa…a`.
fn match_items<T: Symbol>(pattern: &[Item<T>], name: &[T], fold: bool) -> bool {
    let (mut p, mut n) = (0, 0);
    // Where to resume after the last `*`: (pattern index after it, name index).
    let mut resume: Option<(usize, usize)> = None;
    loop {
        work::count(1);
        if let Some(item) = pattern.get(p) {
            let advanced = match item {
                Item::Star => {
                    p += 1;
                    resume = Some((p, n));
                    continue;
                }
                Item::Any => n < name.len(),
                Item::Set { members, negated } => {
                    n < name.len() && set_matches(members, *negated, name[n], fold)
                }
                Item::Literal(symbol) => n < name.len() && name[n] == *symbol,
                Item::Never => false,
            };
            if advanced {
                p += 1;
                n += 1;
                continue;
            }
        } else if n == name.len() {
            return true;
        }
        // Mismatch: let the last `*` take one more symbol, if it can.
        match resume {
            Some((after_star, taken)) if taken < name.len() => {
                resume = Some((after_star, taken + 1));
                p = after_star;
                n = taken + 1;
            }
            _ => return false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Name, Pattern, matches, pattern_end};
    use crate::work;

    fn m(pattern: &str, name: &str) -> bool {
        matches(pattern, name.as_bytes())
    }

    #[test]
    fn stars_and_question_marks() {
        assert!(m("*.tar.gz", "a.b.tar.gz"));
        assert!(m("readme*", "readme"));
        assert!(m("*", ""));
        assert!(m("*a*b*", "xxaxxbxx"));
        assert!(!m("*a*b", "xxaxxbxxa"));
        assert!(m("?ab", "xab"));
        assert!(!m("?ab", "ab"));
        assert!(m("*", ".hidden/and/slashes"));
        assert!(m("a?c", "aéc"), "? takes one character, not one byte");
    }

    #[test]
    fn sets_ranges_negation_and_escapes() {
        assert!(m("*.[ch]", "x.h"));
        assert!(!m("*.[ch]", "x.o"));
        assert!(m("[a-c]x", "bx"));
        assert!(!m("[!a-c]x", "bx"));
        assert!(m("[^a-c]x", "dx"));
        assert!(m("[]]", "]"));
        assert!(m("[!]]", "x"));
        assert!(m("\\*", "*"));
        assert!(!m("\\*", "a"));
        assert!(m("[ab", "[ab"), "an unclosed [ stands for itself");
        assert!(m("a-[0-9][0-9]", "a-42"));
    }

    #[test]
    fn brackets_as_glibc_reads_them() {
        assert!(m("file[\\]].foo", "file].foo"), "an escaped ]");
        assert!(m("file[[:digit:]].foo", "file2.foo"));
        assert!(!m("file[[:digit:]].foo", "filex.foo"));
        assert!(m("[![:digit:]]", "x"));
        assert!(m("[[:alpha:][:digit:]]", "7"));
        assert!(m("[[:upper:]]", "É"), "Unicode classes for UTF-8 names");
        assert!(!m("[[:nope:]]", "n"), "an unknown class matches nothing");
        assert!(!m("[![:nope:]]", "n"), "negated too");
        assert!(m("[a[:nope:]]", "a"), "a member before it still matches");
        assert!(m("[[=a=]]", "a"));
        assert!(m("[[.-.]]", "-"));
        assert!(!m("[[.ab.]]", "a"), "a multi-symbol element");
        assert!(m("[Z-\\]]", "]"), "an escaped range end");
        assert!(
            !m("[a-\\]]", "]"),
            "a range whose end is below its start is empty"
        );
        assert!(m("[a-]", "-"), "a final - is a member");
        assert!(m("[[:al]", "l"), "not a class: an ordinary [");
        assert!(!m("a\\", "a\\"), "a trailing backslash matches nothing");
        assert!(matches("[[:digit:]]", b"\xff1"[1..].as_ref()));
        assert!(!matches("[[:alpha:]]x", b"\xffx"), "bytes: ASCII classes");
    }

    #[test]
    fn a_bracket_that_never_matches_takes_the_rest_of_the_pattern() {
        // `[.` with no `.]` after it: the bracket never matches, and the
        // `:` after it is part of it, as `parse` reads it.
        for text in ["[[.:", "*[a:b[.x:cs", "x[a[.:y:z"] {
            assert_eq!(pattern_end(text), text.len(), "{text}");
            assert!(!m(text, text), "{text}");
            assert!(!folded(text, text), "{text}");
        }
        // A `[` no `]` closes is itself: the next `:` still ends it.
        assert_eq!(pattern_end("*[a:cs"), 3);
        // Also when an escaped `:` keeps the set open.
        assert_eq!(pattern_end("[:\\:cs"), 1);
        assert_eq!(pattern_end("x[:a\\:cs"), 2);
    }

    #[test]
    fn names_that_are_not_utf8_match_byte_wise() {
        assert!(matches("*.txt", b"\xff\xfe.txt"));
        assert!(matches("?.txt", b"\xff.txt"));
        assert!(!matches("é", b"\xff"));
        assert!(
            !matches("é*", b"\xc3\xa9\xff"),
            "no byte-wise match of a non-ASCII pattern"
        );
        assert!(!matches("[é]?", b"\xc3\xff"));
    }

    /// Whether `name`, lowercased per character as names are, matches
    /// `pattern` case-insensitively.
    fn folded(pattern: &str, name: &str) -> bool {
        let lower: String = name.chars().flat_map(char::to_lowercase).collect();
        Pattern::new(pattern, true).matches(&Name::new(lower.as_bytes()))
    }

    #[test]
    fn case_insensitive_patterns_fold_in_the_matcher() {
        assert!(folded("*.TXT", "a.txt"));
        assert!(folded("*Σ", "aσ"));
        assert!(!folded("*Σ", "aς"), "ς is lowercase already");
        assert!(folded("[[:upper:]].foo", "a.foo"));
        assert!(folded("[[:upper:]].foo", "A.foo"));
        assert!(folded("[[:lower:]]", "É"));
        assert!(folded("[[:upper:]]", "ß"), "a letter with case");
        assert!(!folded("[[:upper:]]", "1"));
        assert!(!folded("[![:upper:]]", "a"));
        assert!(folded("[A-C].x", "b.x"));
        assert!(folded("[a-c].x", "B.x"));
        assert!(folded("[Z-a]", "z"), "a range keeps its ends as written");
        assert!(!folded("[!A-C]", "b"));
        assert!(folded("[Σ]", "σ"));
        assert!(!folded("[Σ]", "ς"), "a member folds as a literal does");
        assert!(folded("[Σ-Σ]", "ς"), "a range tests the uppercase too");
        assert!(!folded("[İ]", "İ"), "a member that lowercases to two");
        assert!(!folded("[İ]", "i"));
        assert!(folded("[[=A=]]", "a"));
        assert!(folded("\\A", "a"));
        assert!(folded("İ", "İ"), "a character that lowercases to two");
        assert!(!m("[[:upper:]]", "a"), "case-sensitive patterns as before");
        assert!(!m("[A-C]", "b"));
        // Names that are not UTF-8: ASCII folding.
        let ascii = Pattern::new("[[:upper:]]?", true);
        assert!(ascii.matches(&Name::new(b"a\xff")));
        assert!(!ascii.matches(&Name::new(b"1\xff")));
        let range = Pattern::new("[A-C]?", true);
        assert!(range.matches(&Name::new(b"b\xff")));
    }

    #[test]
    fn a_long_case_insensitive_pattern_is_parsed_and_matched_in_linear_time() {
        // Items that fold: a literal that lowercases to two characters,
        // a range and a class tested with case variants, a member. One
        // `*` first, which matches at once, or no `*` and a mismatch at
        // the end: no backtracking. Steps cost per character, so per
        // unit (29 pattern bytes in 25 characters, 9 name bytes): 58 to
        // decode and copy the pattern's bytes; 5 to read the top-level
        // characters and 50 to mark each character and look at it for
        // a `.]`; 20 in the sets (a step per member and per `]`, and
        // `upper:` read and copied, 12); 10 to match (6 items, `İ`
        // folding to two, and 4 members), about 0.26 a byte. 143 in
        // all, 3.76 a byte; the bound is twice that.
        let unit = "İ[Σ-Σ][[:upper:]][A-C][Σ]";
        let name = "i\u{307}ςabσ";
        for (pattern, tail, matched) in [("*", "", true), ("", "x", false)] {
            work::assert_linear(
                2000,
                |n| {
                    let pattern = format!("{pattern}{}{tail}", unit.repeat(n));
                    // Lowercased and split as names are, by the caller.
                    let lower = crate::database::lowercase(name.repeat(n).as_bytes());
                    let bytes = pattern.len() + lower.len();
                    let lower = Name::new(&lower);
                    let (hit, steps) = work::steps(|| Pattern::new(&pattern, true).matches(&lower));
                    assert_eq!(hit, matched);
                    (steps, bytes)
                },
                |units, _| 2 * (100 + 143 * units),
            );
        }
    }

    #[test]
    fn backtracking_after_a_star_takes_pattern_times_name_at_worst() {
        // `*` then `a` × n and `b`, against `a` × 2n: 2n + 1 tries, the
        // first n + 1 of n + 1 steps. Within the documented bound of
        // (name + `*`s + 1) tries of (run + 1) steps, and above n².
        for n in [1000, 2000] {
            let pattern = Pattern::new(&format!("*{}b", "a".repeat(n)), false);
            let name = "a".repeat(2 * n);
            let name = Name::new(name.as_bytes());
            let (hit, steps) = work::steps(|| pattern.matches(&name));
            assert!(!hit);
            assert!(n * n < steps && steps <= (2 * n + 2) * (n + 2), "{steps}");
        }
    }

    #[test]
    fn many_stars_stay_fast() {
        let name = "a".repeat(2000);
        assert!(!m("*a*a*a*a*a*a*a*a*b", &name));
    }

    /// The bound for parsing a pattern of `bytes` bytes at `per` steps a
    /// byte: twice the analysed cost. Each byte is decoded and copied and
    /// checked for ASCII (two), and in each of the two parses (characters
    /// and bytes) marked unpassed and looked at for a `.]` (two) and read
    /// (one): eight, and the steps the brackets take on top.
    fn parse_bound(per: usize, bytes: usize) -> usize {
        2 * (10 + per * bytes)
    }

    #[test]
    fn unclosed_brackets_are_parsed_in_linear_time() {
        // Unclosed brackets alone, of ranges, and with collating elements
        // or a class name each failed bracket would read again. Each with
        // its steps per byte: the first failed bracket passes each place
        // once, and each later one stops at the first place passed. Per
        // byte in each parse: two for `[` (a pass and a stop each); two
        // thirds for `[a-` (a range a step); one and a half for `[.` (a
        // collating element jumps to the end); two and a half for the
        // class name (a pass and a stop per `[`, and each letter read and
        // copied once).
        type Shape = fn(usize) -> String;
        let shapes: [(Shape, usize); 4] = [
            (|n| "[".repeat(n), 12),
            (|n| "[a-".repeat(n), 10),
            (|n| format!("[{}.]", "[.".repeat(n)), 11),
            (|n| format!("{}[:{}", "[".repeat(n), "a".repeat(n)), 13),
        ];
        for (shape, per) in shapes {
            work::assert_linear(
                5000,
                |n| {
                    let pattern = shape(n);
                    let ((), steps) = work::steps(|| drop(Pattern::new(&pattern, false)));
                    (steps, pattern.len())
                },
                |_, bytes| parse_bound(per, bytes),
            );
        }
    }
}
