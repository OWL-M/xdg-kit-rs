//! The `Exec` key (Desktop Entry spec "The Exec key"): the command line,
//! its quoting and its field codes.
//!
//! [`CommandLine::parse`] takes a value with string escapes already
//! decoded, as returned by [`DesktopEntry::exec`](crate::DesktopEntry::exec).
//! Parsing returns an [`ExecError`] for a missing or empty program, an
//! unknown field code (including inside quotes), or an unclosed quote.
//! These make the command unusable under the spec. Other violations,
//! such as an unquoted reserved character or a field code inside quotes,
//! produce an [`ExecProblem`]. [`validate`](crate::validate()) reports these,
//! but they do not prevent launching.

use std::ffi::{OsStr, OsString};
use std::fmt;

use crate::key_file::work;

/// Characters that need quoting ("Reserved characters"), besides space.
const RESERVED: &[char] = &[
    '\t', '\n', '"', '\'', '\\', '>', '<', '~', '|', '&', ';', '$', '*', '?', '#', '(', ')', '`',
];

/// One part of an unquoted argument.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Piece {
    Text(String),
    /// A field code: one of `f F u U i c k`.
    Code(char),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Arg {
    pieces: Vec<Piece>,
    /// Quoted in whole: no field codes are expanded.
    quoted: bool,
}

impl Arg {
    /// Nothing at all, as `""` gives.
    fn is_empty(&self) -> bool {
        self.pieces == [Piece::Text(String::new())]
    }
}

/// Why a command line cannot be used.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ExecError {
    /// No program, or an empty one (`""`).
    Empty,
    /// A `"` with no closing one.
    UnclosedQuote,
    /// A `\` at the very end inside a quote.
    IncompleteEscape,
    /// A `%` at the very end, or right before a closing quote.
    IncompleteFieldCode,
    /// A field code the spec does not list (`%x`).
    UnknownFieldCode(char),
}

impl fmt::Display for ExecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => write!(f, "the command line has no program"),
            Self::UnclosedQuote => write!(f, "a quote is not closed"),
            Self::IncompleteEscape => write!(f, "the command line ends in an incomplete escape"),
            Self::IncompleteFieldCode => write!(
                f,
                "the command line or a quoted argument ends in an incomplete field code"
            ),
            Self::UnknownFieldCode(code) if code.is_ascii_graphic() => {
                write!(f, "%{code} is not a field code the spec lists")
            }
            Self::UnknownFieldCode(code) => {
                write!(f, "% before {code:?} is not a field code the spec lists")
            }
        }
    }
}

impl std::error::Error for ExecError {}

/// Something in a usable command line that breaks the spec's rules.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ExecProblem {
    /// A reserved character outside quotes.
    ReservedOutsideQuotes(char),
    /// `` ` `` or `$` inside quotes without a backslash before it.
    UnescapedInQuotes(char),
    /// A backslash inside quotes before a character other than `"`,
    /// `` ` ``, `$` and `\`; the character is the one after the
    /// backslash. The backslash is kept; the character after it is read
    /// as if there were none (a `%` still starts a field code).
    InvalidEscapeInQuotes(char),
    /// A quote that does not cover its whole argument.
    PartlyQuoted,
    /// A field code inside a quoted argument (its expansion is undefined).
    /// One the spec lists is left as written; a deprecated one is removed
    /// and also reported as [`ExecProblem::Deprecated`].
    FieldCodeInQuotes,
    /// More than one of `%f %F %u %U`.
    SeveralFileCodes,
    /// `%F`, `%U` or `%i` not as an argument on its own.
    NotStandalone(char),
    /// A deprecated field code (`%d %D %n %N %v %m`), which is dropped,
    /// inside quotes too.
    Deprecated(char),
    /// `=` in the program's name.
    EqualsInProgram,
}

impl fmt::Display for ExecProblem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ReservedOutsideQuotes(c) => {
                write!(f, "reserved character {c:?} outside quotes")
            }
            Self::UnescapedInQuotes(c) => {
                write!(f, "{c:?} inside quotes must be escaped with a backslash")
            }
            Self::InvalidEscapeInQuotes(c) => write!(
                f,
                r#"a backslash before {c:?} is not an escape inside quotes; use \", \`, \$ or \\"#
            ),
            Self::PartlyQuoted => write!(f, "a quote must enclose a whole argument"),
            Self::FieldCodeInQuotes => write!(f, "field codes may not be used inside quotes"),
            Self::SeveralFileCodes => write!(
                f,
                "a command line may contain at most one of %f, %F, %u and %U"
            ),
            Self::NotStandalone(code) => {
                write!(f, "%{code} may only be used as an argument on its own")
            }
            Self::Deprecated(code) => write!(f, "%{code} is deprecated and is dropped"),
            Self::EqualsInProgram => write!(f, "the program's name may not contain '='"),
        }
    }
}

/// Which file field code a command line has.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileCode {
    /// `%f`: one local path per command.
    File,
    /// `%F`: all local paths in one command.
    Files,
    /// `%u`: one URL per command.
    Url,
    /// `%U`: all URLs in one command.
    Urls,
}

impl FileCode {
    /// Whether the program takes several targets in one run.
    #[must_use]
    pub fn takes_several(self) -> bool {
        matches!(self, Self::Files | Self::Urls)
    }

    /// Whether the program takes URLs rather than local paths.
    #[must_use]
    pub fn takes_urls(self) -> bool {
        matches!(self, Self::Url | Self::Urls)
    }
}

/// A parsed `Exec` command line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandLine {
    args: Vec<Arg>,
    problems: Vec<ExecProblem>,
}

/// What field codes expand to, for [`CommandLine::expand`].
pub(crate) struct Values<'a> {
    /// The targets for `%f %F %u %U`, already paths or URLs as the code
    /// needs; at most one for `%f` and `%u`.
    pub(crate) files: &'a [OsString],
    pub(crate) icon: Option<&'a str>,
    pub(crate) name: &'a str,
    pub(crate) location: &'a OsStr,
}

impl CommandLine {
    /// Parses `exec`, the `Exec` value with string escapes undone.
    ///
    /// # Errors
    ///
    /// When the spec says the command line must not be used.
    pub fn parse(exec: &str) -> Result<Self, ExecError> {
        let mut problems = Vec::new();
        let mut args = Vec::new();
        // A step per character read.
        let mut chars = exec.chars().inspect(|_| work::count(1)).peekable();
        loop {
            while chars.peek() == Some(&' ') {
                chars.next();
            }
            if chars.peek().is_none() {
                break;
            }
            work::count(1);
            let mut arg = Arg {
                pieces: Vec::new(),
                quoted: false,
            };
            let mut text = String::new();
            let mut started_quoted = false;
            let mut had_deprecated = false;
            while let Some(&c) = chars.peek() {
                if c == ' ' {
                    break;
                }
                chars.next();
                if c == '"' {
                    if !arg.pieces.is_empty() || !text.is_empty() {
                        problems.push(ExecProblem::PartlyQuoted);
                    } else {
                        started_quoted = true;
                    }
                    read_quoted(&mut chars, &mut text, &mut problems)?;
                    if chars.peek().is_some_and(|&next| next != ' ') {
                        problems.push(ExecProblem::PartlyQuoted);
                        started_quoted = false;
                    }
                    continue;
                }
                if c == '%' {
                    match chars.next() {
                        None => return Err(ExecError::IncompleteFieldCode),
                        Some('%') => text.push('%'),
                        Some(code @ ('f' | 'F' | 'u' | 'U' | 'i' | 'c' | 'k')) => {
                            if !text.is_empty() {
                                arg.pieces.push(Piece::Text(std::mem::take(&mut text)));
                            }
                            arg.pieces.push(Piece::Code(code));
                        }
                        Some(code @ ('d' | 'D' | 'n' | 'N' | 'v' | 'm')) => {
                            problems.push(ExecProblem::Deprecated(code));
                            had_deprecated = true;
                            continue;
                        }
                        Some(code) => return Err(ExecError::UnknownFieldCode(code)),
                    }
                    continue;
                }
                if RESERVED.contains(&c) {
                    problems.push(ExecProblem::ReservedOutsideQuotes(c));
                }
                text.push(c);
            }
            if had_deprecated && !started_quoted && arg.pieces.is_empty() && text.is_empty() {
                // An argument that was only deprecated codes is removed. A
                // quoted one stays, empty, as `""` does.
                continue;
            }
            if !text.is_empty() || arg.pieces.is_empty() {
                arg.pieces.push(Piece::Text(text));
            }
            arg.quoted = started_quoted;
            args.push(arg);
        }
        // An empty program, as `""` gives, is no program.
        let Some(program) = args.first().filter(|program| !program.is_empty()) else {
            return Err(ExecError::Empty);
        };
        if program.pieces.iter().any(|piece| {
            // A step for the piece, and per character looked through for a
            // `=`.
            work::count(1);
            let Piece::Text(text) = piece else {
                return false;
            };
            work::count(text.len());
            text.contains('=')
        }) {
            problems.push(ExecProblem::EqualsInProgram);
        }
        let mut file_codes = 0;
        for arg in &args {
            for piece in &arg.pieces {
                work::count(1);
                if let Piece::Code(code) = piece {
                    if matches!(code, 'f' | 'F' | 'u' | 'U') {
                        file_codes += 1;
                    }
                    if matches!(code, 'F' | 'U' | 'i') && arg.pieces.len() > 1 {
                        problems.push(ExecProblem::NotStandalone(*code));
                    }
                }
            }
        }
        if file_codes > 1 {
            problems.push(ExecProblem::SeveralFileCodes);
        }
        Ok(Self { args, problems })
    }

    /// What breaks the spec's rules but still lets the command run.
    #[must_use]
    pub fn problems(&self) -> &[ExecProblem] {
        &self.problems
    }

    /// The file field code, if any (the first, when there are several).
    #[must_use]
    pub fn file_code(&self) -> Option<FileCode> {
        self.args
            .iter()
            .filter(|arg| !arg.quoted)
            .flat_map(|arg| &arg.pieces)
            .find_map(|piece| match piece {
                Piece::Code('f') => Some(FileCode::File),
                Piece::Code('F') => Some(FileCode::Files),
                Piece::Code('u') => Some(FileCode::Url),
                Piece::Code('U') => Some(FileCode::Urls),
                _ => None,
            })
    }

    /// The arguments with the field codes expanded. Each replacement is one
    /// argument (or, for `%F`, `%U` and `%i`, several) and is not expanded
    /// again; an argument made only of field codes that expand to nothing
    /// is dropped.
    pub(crate) fn expand(&self, values: &Values<'_>) -> Vec<OsString> {
        let mut out = Vec::new();
        for arg in &self.args {
            if let [Piece::Code(code)] = arg.pieces.as_slice()
                && !arg.quoted
            {
                match code {
                    'F' | 'U' => out.extend(values.files.iter().cloned()),
                    'f' | 'u' => out.extend(values.files.first().cloned()),
                    'i' => {
                        if let Some(icon) = values.icon.filter(|icon| !icon.is_empty()) {
                            out.push("--icon".into());
                            out.push(icon.into());
                        }
                    }
                    'c' if !values.name.is_empty() => out.push(values.name.into()),
                    'k' if !values.location.is_empty() => out.push(values.location.into()),
                    _ => {}
                }
                continue;
            }
            let mut text = OsString::new();
            for piece in &arg.pieces {
                match piece {
                    Piece::Text(part) => text.push(part),
                    Piece::Code(code) => match code {
                        'f' | 'u' | 'F' | 'U' => {
                            if let Some(first) = values.files.first() {
                                text.push(first);
                            }
                        }
                        'c' => text.push(values.name),
                        'k' => text.push(values.location),
                        _ => {}
                    },
                }
            }
            out.push(text);
        }
        out
    }
}

/// Reads a quoted argument after its opening `"` up to the closing one.
fn read_quoted(
    chars: &mut std::iter::Peekable<impl Iterator<Item = char>>,
    text: &mut String,
    problems: &mut Vec<ExecProblem>,
) -> Result<(), ExecError> {
    loop {
        let Some(c) = chars.next() else {
            return Err(ExecError::UnclosedQuote);
        };
        match c {
            '"' => return Ok(()),
            '\\' => match chars.peek() {
                Some(&escaped @ ('"' | '`' | '$' | '\\')) => {
                    chars.next();
                    text.push(escaped);
                }
                // The backslash is kept; the next character is read as if
                // it had none, so a `%` still starts a field code.
                Some(&other) => {
                    problems.push(ExecProblem::InvalidEscapeInQuotes(other));
                    text.push('\\');
                }
                None => return Err(ExecError::IncompleteEscape),
            },
            '`' | '$' => {
                problems.push(ExecProblem::UnescapedInQuotes(c));
                text.push(c);
            }
            '%' => match chars.peek() {
                // Left to the next turn: the quote is not closed.
                None => {}
                Some('"') => return Err(ExecError::IncompleteFieldCode),
                Some('%') => {
                    chars.next();
                    text.push('%');
                }
                Some(&code @ ('f' | 'F' | 'u' | 'U' | 'i' | 'c' | 'k')) => {
                    chars.next();
                    problems.push(ExecProblem::FieldCodeInQuotes);
                    text.push('%');
                    text.push(code);
                }
                Some(&code @ ('d' | 'D' | 'n' | 'N' | 'v' | 'm')) => {
                    chars.next();
                    problems.push(ExecProblem::FieldCodeInQuotes);
                    problems.push(ExecProblem::Deprecated(code));
                }
                Some(&code) => return Err(ExecError::UnknownFieldCode(code)),
            },
            _ => text.push(c),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Checks that parsing `a` and one argument of about n bytes, `unit`
    /// repeated (inside quotes when `quoted`), is linear; the argument reads
    /// as `read` repeated.
    fn assert_long_argument_linear(unit: &str, read: &str, quoted: bool) {
        work::assert_linear(
            512 * 1024,
            |n| {
                let arg = unit.repeat(n / unit.len());
                let exec = if quoted {
                    format!("a \"{arg}\"")
                } else {
                    format!("a {arg}")
                };
                let (line, steps) = work::steps(|| CommandLine::parse(&exec));
                assert_eq!(line.map(|line| line.problems().len()), Ok(0));
                assert_eq!(expand(&exec, &[])[1], read.repeat(n / unit.len()));
                (steps, exec.len())
            },
            // A step per character read, and per argument and piece: about 1
            // per byte. Twice that.
            |_, bytes| 2 * bytes,
        );
    }

    #[test]
    fn one_long_argument_is_parsed_in_linear_time() {
        assert_long_argument_linear("abc%%", "abc%", false);
    }

    #[test]
    fn one_long_quoted_argument_is_parsed_in_linear_time() {
        assert_long_argument_linear(r"ab\\c%%", r"ab\c%", true);
    }

    fn expand(exec: &str, files: &[&str]) -> Vec<String> {
        let line = CommandLine::parse(exec).unwrap();
        let files: Vec<OsString> = files.iter().map(OsString::from).collect();
        line.expand(&Values {
            files: &files,
            icon: Some("fooview"),
            name: "Foo Viewer",
            location: OsStr::new("/usr/share/applications/foo.desktop"),
        })
        .into_iter()
        .map(|arg| arg.into_string().unwrap())
        .collect()
    }

    #[test]
    fn field_codes_expand_into_whole_arguments() {
        assert_eq!(
            expand("fooview %F", &["/a b", "/c"]),
            ["fooview", "/a b", "/c"]
        );
        assert_eq!(expand("fooview %f", &["/a b"]), ["fooview", "/a b"]);
        assert_eq!(expand("fooview %f", &[]), ["fooview"]);
        assert_eq!(
            expand("fooview %i --name=%c %k", &[]),
            [
                "fooview",
                "--icon",
                "fooview",
                "--name=Foo Viewer",
                "/usr/share/applications/foo.desktop"
            ]
        );
        assert_eq!(
            expand("fooview --file=%u", &["file:///x"]),
            ["fooview", "--file=file:///x"]
        );
        assert_eq!(expand("echo 100%%", &[]), ["echo", "100%"]);
        assert_eq!(
            expand("app %d %U", &["u"]),
            ["app", "u"],
            "deprecated codes dropped"
        );
        assert_eq!(expand("app %d%f x%m", &["/f"]), ["app", "/f", "x"]);
    }

    #[test]
    fn quoting_is_undone_before_expansion() {
        assert_eq!(
            expand(
                r#""/opt/My App/bin/app" "a \"q\" \$HOME \\ \`x\`" %f"#,
                &["/f"]
            ),
            ["/opt/My App/bin/app", "a \"q\" $HOME \\ `x`", "/f"]
        );
        assert_eq!(
            expand(r#"sh -c "echo %f""#, &["/f"]),
            ["sh", "-c", "echo %f"]
        );
        assert_eq!(expand("app  spaced   out", &[]), ["app", "spaced", "out"]);
    }

    #[test]
    fn what_makes_a_command_line_unusable() {
        assert_eq!(CommandLine::parse(""), Err(ExecError::Empty));
        assert_eq!(CommandLine::parse("   "), Err(ExecError::Empty));
        assert_eq!(
            CommandLine::parse("app %x"),
            Err(ExecError::UnknownFieldCode('x'))
        );
        assert_eq!(
            CommandLine::parse("app %"),
            Err(ExecError::IncompleteFieldCode)
        );
        assert_eq!(
            CommandLine::parse("app \"open"),
            Err(ExecError::UnclosedQuote)
        );
        assert_eq!(
            CommandLine::parse("app \"a\\"),
            Err(ExecError::IncompleteEscape)
        );
    }

    #[test]
    fn rule_breaking_that_still_runs() {
        let problems = |exec: &str| CommandLine::parse(exec).unwrap().problems().to_vec();
        assert_eq!(
            problems("app ~/x"),
            [ExecProblem::ReservedOutsideQuotes('~')]
        );
        assert_eq!(
            problems("app \"$X\""),
            [ExecProblem::UnescapedInQuotes('$')]
        );
        assert_eq!(problems("app \"%f\""), [ExecProblem::FieldCodeInQuotes]);
        assert_eq!(problems("app %f %U"), [ExecProblem::SeveralFileCodes]);
        assert_eq!(problems("app --x=%U"), [ExecProblem::NotStandalone('U')]);
        assert_eq!(problems("app --icon=%i"), [ExecProblem::NotStandalone('i')]);
        assert_eq!(problems("a=b %f"), [ExecProblem::EqualsInProgram]);
        assert_eq!(problems("app pre\"q\""), [ExecProblem::PartlyQuoted]);
        assert_eq!(problems("app %m"), [ExecProblem::Deprecated('m')]);
        assert!(problems("env A=b app --x=%c %U").is_empty());
    }

    #[test]
    fn an_empty_program_is_no_program() {
        assert_eq!(CommandLine::parse(r#""""#), Err(ExecError::Empty));
        assert_eq!(CommandLine::parse(r#""" foo"#), Err(ExecError::Empty));
        assert_eq!(expand(r#"foo """#, &[]), ["foo", ""], "a later one is kept");
    }

    #[test]
    fn other_escapes_in_quotes_are_problems_and_kept() {
        let line = CommandLine::parse(r#"echo "abc\q""#).unwrap();
        assert_eq!(line.problems(), [ExecProblem::InvalidEscapeInQuotes('q')]);
        assert_eq!(
            ExecProblem::InvalidEscapeInQuotes('q').to_string(),
            r#"a backslash before 'q' is not an escape inside quotes; use \", \`, \$ or \\"#
        );
        assert!(
            ExecProblem::InvalidEscapeInQuotes('\u{200b}')
                .to_string()
                .starts_with(r"a backslash before '\u{200b}' ")
        );
        assert_eq!(expand(r#"echo "abc\q""#, &[]), ["echo", "abc\\q"]);
        assert!(
            CommandLine::parse(r#"echo "\" \` \$ \\""#)
                .unwrap()
                .problems()
                .is_empty()
        );
    }

    #[test]
    fn percent_signs_inside_quotes() {
        for (exec, err) in [
            (r#"app "%z""#, ExecError::UnknownFieldCode('z')),
            (r#"app "% x""#, ExecError::UnknownFieldCode(' ')),
            (r#"app "%1""#, ExecError::UnknownFieldCode('1')),
            (r#"app "100%""#, ExecError::IncompleteFieldCode),
            (r#"app "a%""#, ExecError::IncompleteFieldCode),
        ] {
            assert_eq!(CommandLine::parse(exec), Err(err), "{exec}");
        }
        assert_eq!(
            CommandLine::parse(r#"app "a%"#),
            Err(ExecError::UnclosedQuote)
        );
        let line = CommandLine::parse(r#"app "%%z""#).unwrap();
        assert!(line.problems().is_empty());
        assert_eq!(expand(r#"app "%%z""#, &[]), ["app", "%z"]);
        let line = CommandLine::parse(r#"app "%f""#).unwrap();
        assert_eq!(line.problems(), [ExecProblem::FieldCodeInQuotes]);
        assert_eq!(expand(r#"app "%f""#, &["/f"]), ["app", "%f"]);
        let line = CommandLine::parse(r#"app "%d""#).unwrap();
        assert_eq!(
            line.problems(),
            [ExecProblem::FieldCodeInQuotes, ExecProblem::Deprecated('d')]
        );
        assert_eq!(
            expand(r#"app "%d""#, &[]),
            ["app", ""],
            "the quotes keep it"
        );
        assert_eq!(expand(r#"app "a%db""#, &[]), ["app", "ab"]);
        assert_eq!(
            ExecError::IncompleteFieldCode.to_string(),
            "the command line or a quoted argument ends in an incomplete field code"
        );
        assert_eq!(
            ExecError::UnknownFieldCode('z').to_string(),
            "%z is not a field code the spec lists"
        );
        assert_eq!(
            ExecError::UnknownFieldCode(' ').to_string(),
            "% before ' ' is not a field code the spec lists"
        );
        assert_eq!(
            CommandLine::parse("app %z"),
            Err(ExecError::UnknownFieldCode('z'))
        );
    }

    #[test]
    fn a_bad_escape_in_quotes_leaves_the_next_character_to_the_quoting_rules() {
        assert_eq!(
            CommandLine::parse(r#"app "\%z""#),
            Err(ExecError::UnknownFieldCode('z'))
        );
        let line = CommandLine::parse(r#"app "\%%z""#).unwrap();
        assert_eq!(line.problems(), [ExecProblem::InvalidEscapeInQuotes('%')]);
        assert_eq!(expand(r#"app "\%%z""#, &[]), ["app", r"\%z"]);
        let line = CommandLine::parse(r#"app "\%d""#).unwrap();
        assert_eq!(
            line.problems(),
            [
                ExecProblem::InvalidEscapeInQuotes('%'),
                ExecProblem::FieldCodeInQuotes,
                ExecProblem::Deprecated('d'),
            ]
        );
        assert_eq!(expand(r#"app "\%d""#, &[]), ["app", r"\"]);
        let line = CommandLine::parse(r#"app "\%f""#).unwrap();
        assert_eq!(
            line.problems(),
            [
                ExecProblem::InvalidEscapeInQuotes('%'),
                ExecProblem::FieldCodeInQuotes,
            ]
        );
        assert_eq!(expand(r#"app "\%f""#, &["/f"]), ["app", r"\%f"]);
        let line = CommandLine::parse(r#"app "a\qb""#).unwrap();
        assert_eq!(line.problems(), [ExecProblem::InvalidEscapeInQuotes('q')]);
        assert_eq!(expand(r#"app "a\qb""#, &[]), ["app", r"a\qb"]);
    }

    #[test]
    fn the_file_code_decides_how_targets_are_passed() {
        let code = |exec: &str| CommandLine::parse(exec).unwrap().file_code();
        assert_eq!(code("app"), None);
        assert_eq!(code("app %f"), Some(FileCode::File));
        assert_eq!(code("app %U"), Some(FileCode::Urls));
        assert_eq!(code("sh -c \"x %f\""), None, "not in quotes");
        assert!(FileCode::Files.takes_several() && !FileCode::Url.takes_several());
        assert!(FileCode::Urls.takes_urls() && !FileCode::File.takes_urls());
    }
}
