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

/// Whether `name` matches `pattern`.
#[cfg(any(test, fuzzing))]
pub(crate) fn matches(pattern: &str, name: &[u8]) -> bool {
    Pattern::new(pattern).matches(&Name::new(name))
}

/// A pattern parsed once, to be matched against many names.
#[derive(Debug)]
pub(crate) struct Pattern {
    chars: Vec<Item<char>>,
    /// The pattern over bytes when it is all ASCII: only then can a name
    /// that is not UTF-8 match.
    bytes: Option<Vec<Item<u8>>>,
}

impl Pattern {
    pub(crate) fn new(pattern: &str) -> Self {
        let chars: Vec<char> = pattern.chars().collect();
        Self {
            chars: parse(&chars),
            bytes: pattern.is_ascii().then(|| parse(pattern.as_bytes())),
        }
    }

    /// Whether `name` matches this pattern.
    pub(crate) fn matches(&self, name: &Name<'_>) -> bool {
        match name {
            Name::Chars(name) => match_items(&self.chars, name),
            Name::Bytes(name) => self
                .bytes
                .as_deref()
                .is_some_and(|pattern| match_items(pattern, name)),
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
}

impl Symbol for char {
    fn is(self, ascii: u8) -> bool {
        self == char::from(ascii)
    }

    fn ascii(self) -> Option<char> {
        self.is_ascii().then_some(self)
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

fn parse<T: Symbol>(pattern: &[T]) -> Vec<Item<T>> {
    let mut items = Vec::new();
    let mut at = 0;
    while let Some(&symbol) = pattern.get(at) {
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
            match bracket(&pattern[at..]) {
                Some((item, used)) => {
                    at += used;
                    item
                }
                None => Item::Literal(symbol),
            }
        } else {
            Item::Literal(symbol)
        };
        items.push(item);
    }
    items
}

/// The set that `rest` (what follows a `[`) starts and how many symbols it
/// takes, or `None` when no `]` closes it (the `[` then stands for itself).
fn bracket<T: Symbol>(rest: &[T]) -> Option<(Item<T>, usize)> {
    let negated = rest.first().is_some_and(|&c| c.is(b'!') || c.is(b'^'));
    let mut at = usize::from(negated);
    let mut members = Vec::new();
    let mut first = true;
    loop {
        let &symbol = rest.get(at)?;
        if symbol.is(b']') && !first {
            return Some((Item::Set { members, negated }, at + 1));
        }
        first = false;
        let (low, used) = match element(&rest[at..]) {
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
        let dash = rest.get(at).is_some_and(|c| c.is(b'-'));
        let after = rest.get(at + 1);
        if dash && after.is_some_and(|c| !c.is(b']')) {
            match element(&rest[at + 1..]) {
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

enum Element<T> {
    /// One symbol (plain, escaped, `[=c=]` or `[.c.]`) and the symbols it
    /// takes.
    Symbol(T, usize),
    /// A class (or an invalid one) and the symbols it takes.
    Member(Member<T>, usize),
    /// The pattern can never match from here.
    Never,
}

/// The set element at the start of `rest`, which is not empty.
fn element<T: Symbol>(rest: &[T]) -> Element<T> {
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
                let Some(close) = rest[2..]
                    .windows(2)
                    .position(|pair| pair[0].is(b'.') && pair[1].is(b']'))
                else {
                    return Element::Never;
                };
                return match close {
                    1 => Element::Symbol(rest[2], 5),
                    _ => Element::Member(Member::Invalid, 2 + close + 2),
                };
            }
            _ => {}
        }
    }
    Element::Symbol(symbol, 1)
}

fn set_matches<T: Symbol>(members: &[Member<T>], negated: bool, symbol: T) -> bool {
    for member in members {
        let hit = match *member {
            Member::One(one) => one == symbol,
            Member::Range(low, high) => low <= symbol && symbol <= high,
            Member::Class(class) => symbol.in_class(class),
            Member::Invalid => return false,
        };
        if hit {
            return !negated;
        }
    }
    negated
}

/// Matching with backtracking only at the last `*`, which keeps it linear
/// in practice and never recursive.
fn match_items<T: Symbol>(pattern: &[Item<T>], name: &[T]) -> bool {
    let (mut p, mut n) = (0, 0);
    // Where to resume after the last `*`: (pattern index after it, name index).
    let mut resume: Option<(usize, usize)> = None;
    loop {
        if let Some(item) = pattern.get(p) {
            let advanced = match item {
                Item::Star => {
                    p += 1;
                    resume = Some((p, n));
                    continue;
                }
                Item::Any => n < name.len(),
                Item::Set { members, negated } => {
                    n < name.len() && set_matches(members, *negated, name[n])
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
    use super::matches;

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

    #[test]
    fn many_stars_stay_fast() {
        let name = "a".repeat(2000);
        assert!(!m("*a*a*a*a*a*a*a*a*b", &name));
    }
}
