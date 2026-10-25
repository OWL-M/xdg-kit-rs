//! Building a thumbnailer's command: the deployed pipeline splits the
//! `Exec` line with shell *quoting* rules first (as `g_shell_parse_argv`
//! does), then expands the placeholders inside each resulting argument —
//! so `--output=%o` and quoted placeholders work. Nothing is ever
//! expanded as shell syntax (`$`, backticks and globs are ordinary
//! characters) and nothing ever reaches a real shell; replacements are
//! spliced into `OsString`s and never re-parsed, so a path full of
//! spaces, quotes, percent signs or non-UTF-8 bytes stays one argument.

use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::Path;

use crate::source::ThumbnailSource;
use crate::thumbnailer::Thumbnailer;
use crate::{CommandError, work};

/// What a thumbnail is to be made of, where, and how large.
#[derive(Debug)]
pub struct ThumbnailRequest<'a> {
    /// The resource to thumbnail.
    pub source: &'a ThumbnailSource,
    /// Where the thumbnailer must write its PNG (`%o`): conventionally a
    /// private temporary file the caller installs afterwards.
    pub output: &'a Path,
    /// The largest dimension in pixels (`%s`).
    pub size: u32,
}

/// A ready-to-spawn thumbnailer invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThumbnailerCommand {
    program: OsString,
    args: Vec<OsString>,
}

impl ThumbnailerCommand {
    /// The program to run.
    #[must_use]
    pub fn program(&self) -> &OsStr {
        &self.program
    }

    /// Its arguments, in order.
    #[must_use]
    pub fn args(&self) -> &[OsString] {
        &self.args
    }

    /// A [`std::process::Command`] for the caller to configure, spawn
    /// and supervise — this crate never runs anything.
    #[must_use]
    pub fn to_command(&self) -> std::process::Command {
        let mut command = std::process::Command::new(&self.program);
        command.args(&self.args);
        command
    }
}

impl Thumbnailer {
    /// The command that would produce `request`'s thumbnail.
    ///
    /// # Errors
    ///
    /// [`CommandError`]: an unsplittable or empty line, an unknown or
    /// incomplete field code, a line that could never produce the output
    /// (no `%i`/`%u`, or no `%o`), or `%i` against a remote source,
    /// which has no local path.
    pub fn command(
        &self,
        request: &ThumbnailRequest<'_>,
    ) -> Result<ThumbnailerCommand, CommandError> {
        let tokens = split(self.exec())?;
        if tokens.is_empty() {
            return Err(CommandError::Empty);
        }
        // Validation before any expansion, so the errors do not depend
        // on the particular source.
        let mut input = false;
        let mut output = false;
        for token in &tokens {
            let mut rest = token.chars();
            while let Some(c) = rest.next() {
                work::count(1);
                if c != '%' {
                    continue;
                }
                match rest.next() {
                    Some('i') => {
                        input = true;
                        if request.source.path().is_none() {
                            return Err(CommandError::NoLocalPath);
                        }
                    }
                    Some('u') => input = true,
                    Some('o') => output = true,
                    Some('s' | '%') => {}
                    Some(code) => return Err(CommandError::UnknownFieldCode(code)),
                    None => return Err(CommandError::Incomplete),
                }
            }
        }
        if !input || !output {
            return Err(CommandError::MissingPlaceholder);
        }
        let expanded: Vec<OsString> = tokens.iter().map(|token| expand(token, request)).collect();
        let Some((program, args)) = expanded.split_first() else {
            return Err(CommandError::Empty);
        };
        if program.is_empty() {
            return Err(CommandError::Empty);
        }
        Ok(ThumbnailerCommand {
            program: program.clone(),
            args: args.to_vec(),
        })
    }
}

/// Splits `line` with shell quoting rules only: whitespace separates
/// arguments; single quotes are literal; double quotes honor `\` before
/// `"`, `\`, `` ` ``, `$` and newline; a bare `\` escapes the next
/// character, and a backslash before a newline is a line continuation
/// (both removed); a `#` at the start of a token comments out the rest.
pub(crate) fn split(line: &str) -> Result<Vec<String>, CommandError> {
    work::count(line.len());
    // The separators GLib tokenizes on: space, tab, newline — not the
    // rest of Unicode whitespace, which stays inside a token.
    let separator = |c: char| matches!(c, ' ' | '\t' | '\n');
    let mut tokens = Vec::new();
    let mut chars = line.chars().peekable();
    // What came right before the position: GLib starts a comment only at
    // the beginning or after a space or newline — after a tab, `#` is an
    // ordinary token (checked against 2.88).
    let mut before: Option<char> = None;
    'tokens: loop {
        while let Some(&c) = chars.peek().filter(|&&c| separator(c)) {
            before = Some(c);
            chars.next();
        }
        let Some(&first) = chars.peek() else {
            break;
        };
        if first == '#' && matches!(before, None | Some(' ' | '\n')) {
            // A comment runs to the end of its line; the next line goes
            // on.
            while chars.next_if(|&c| c != '\n').is_some() {
                work::count(1);
            }
            continue 'tokens;
        }
        before = Some(first);
        let mut token = String::new();
        let mut had_quote = false;
        while let Some(&c) = chars.peek() {
            work::count(1);
            match c {
                c if separator(c) => break,
                '\'' => {
                    chars.next();
                    had_quote = true;
                    loop {
                        match chars.next() {
                            Some('\'') => break,
                            Some(c) => token.push(c),
                            None => return Err(CommandError::Unparsable),
                        }
                    }
                }
                '"' => {
                    chars.next();
                    had_quote = true;
                    loop {
                        match chars.next() {
                            Some('"') => break,
                            Some('\\') => match chars.next() {
                                Some(escaped @ ('"' | '\\' | '`' | '$')) => token.push(escaped),
                                // GLib keeps the newline itself (checked
                                // against 2.88): the backslash goes, the
                                // line break stays.
                                Some('\n') => token.push('\n'),
                                Some(other) => {
                                    token.push('\\');
                                    token.push(other);
                                }
                                None => return Err(CommandError::Unparsable),
                            },
                            Some(c) => token.push(c),
                            None => return Err(CommandError::Unparsable),
                        }
                    }
                }
                '\\' => {
                    chars.next();
                    match chars.next() {
                        // A continuation joins the lines; nothing lands
                        // in the token.
                        Some('\n') => {}
                        Some(escaped) => token.push(escaped),
                        None => return Err(CommandError::Unparsable),
                    }
                }
                _ => {
                    chars.next();
                    token.push(c);
                }
            }
        }
        if token.is_empty() && !had_quote {
            // Only line continuations were consumed: no token, but the
            // line goes on.
            continue 'tokens;
        }
        tokens.push(token);
    }
    Ok(tokens)
}

/// Expands the placeholders in one split token into an `OsString`. The
/// codes were validated before; anything here is spliced, never
/// re-parsed.
fn expand(token: &str, request: &ThumbnailRequest<'_>) -> OsString {
    let mut out = Vec::new();
    let mut rest = token.chars();
    while let Some(c) = rest.next() {
        work::count(1);
        if c != '%' {
            let mut buffer = [0u8; 4];
            out.extend_from_slice(c.encode_utf8(&mut buffer).as_bytes());
            continue;
        }
        match rest.next() {
            Some('i') => out.extend_from_slice(
                request
                    .source
                    .path()
                    .expect("validated before expansion")
                    .as_os_str()
                    .as_bytes(),
            ),
            Some('u') => out.extend_from_slice(request.source.uri().as_bytes()),
            Some('o') => out.extend_from_slice(request.output.as_os_str().as_bytes()),
            Some('s') => out.extend_from_slice(request.size.to_string().as_bytes()),
            Some('%') => out.push(b'%'),
            _ => unreachable!("validated before expansion"),
        }
    }
    OsString::from_vec(out)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn thumbnailer(exec: &str) -> Thumbnailer {
        crate::thumbnailer::Thumbnailer::for_tests(exec)
    }

    fn local() -> ThumbnailSource {
        ThumbnailSource::local("/in put/a%b 'c\" d.png").unwrap()
    }

    fn build(exec: &str, source: &ThumbnailSource) -> Result<Vec<String>, CommandError> {
        let output = PathBuf::from("/tmp/out put.png");
        let command = thumbnailer(exec).command(&ThumbnailRequest {
            source,
            output: &output,
            size: 128,
        })?;
        Ok(std::iter::once(command.program().to_owned())
            .chain(command.args().iter().cloned())
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect())
    }

    #[test]
    fn the_interop_matrix_splits_then_expands() {
        let source = local();
        assert_eq!(
            build("convert %i -thumbnail %sx%s '%o'", &source).unwrap(),
            [
                "convert",
                "/in put/a%b 'c\" d.png",
                "-thumbnail",
                "128x128",
                "/tmp/out put.png"
            ],
            "a quoted placeholder still expands; paths stay one argument"
        );
        assert_eq!(
            build("tool --output=%o --input=%u --percent=100%%", &source).unwrap(),
            [
                "tool",
                "--output=/tmp/out put.png",
                "--input=file:///in%20put/a%25b%20'c%22%20d.png",
                "--percent=100%"
            ],
            "embedded placeholders and literal percents"
        );
        assert_eq!(
            build("tool \"a \\\"b\\\" \\$c \\d\" '' %i %o", &source).unwrap()[1..3],
            ["a \"b\" $c \\d".to_owned(), String::new()],
            "double-quote escapes and an empty quoted argument survive"
        );
        assert_eq!(
            build("tool \\\n  %i \\\n  %o # trailing comment %z", &source).unwrap(),
            ["tool", "/in put/a%b 'c\" d.png", "/tmp/out put.png"],
            "continuations join; a comment ends the line before %z"
        );
        assert_eq!(
            build("%i-reader %u %o", &source).unwrap()[0],
            "/in put/a%b 'c\" d.png-reader",
            "placeholders expand in the program position too"
        );
    }

    #[test]
    fn the_errors_are_specific() {
        let source = local();
        let remote = ThumbnailSource::remote("sftp://host/x.png").unwrap();
        assert_eq!(build("", &source), Err(CommandError::Empty));
        assert_eq!(
            build("   # only a comment", &source),
            Err(CommandError::Empty)
        );
        assert_eq!(
            build("tool 'unclosed %i %o", &source),
            Err(CommandError::Unparsable)
        );
        assert_eq!(
            build("tool %i %o \\", &source),
            Err(CommandError::Unparsable)
        );
        assert_eq!(
            build("tool %i %o %z", &source),
            Err(CommandError::UnknownFieldCode('z'))
        );
        assert_eq!(
            build("tool %i %o 50%", &source),
            Err(CommandError::Incomplete)
        );
        assert_eq!(
            build("tool %i", &source),
            Err(CommandError::MissingPlaceholder)
        );
        assert_eq!(
            build("tool %o", &source),
            Err(CommandError::MissingPlaceholder)
        );
        assert_eq!(build("tool %i %o", &remote), Err(CommandError::NoLocalPath));
        assert!(build("tool %u %o", &remote).is_ok(), "%u serves remotes");
        assert_eq!(build("'' %i %o", &source), Err(CommandError::Empty));
    }

    #[test]
    fn non_utf8_paths_stay_whole() {
        use std::os::unix::ffi::OsStrExt;
        let raw = std::ffi::OsStr::from_bytes(b"/tmp/\xFF gap.png");
        let source = ThumbnailSource::local(PathBuf::from(raw)).unwrap();
        let output = PathBuf::from("/tmp/out.png");
        let command = thumbnailer("tool %i %o")
            .command(&ThumbnailRequest {
                source: &source,
                output: &output,
                size: 256,
            })
            .unwrap();
        assert_eq!(command.args()[0].as_bytes(), b"/tmp/\xFF gap.png");
        let std_command = command.to_command();
        assert_eq!(std_command.get_program(), command.program());
    }
}

#[cfg(test)]
mod linearity {
    use super::*;
    use crate::work;

    #[test]
    fn long_command_lines_split_in_linear_time() {
        let source = ThumbnailSource::local("/tmp/in.png").unwrap();
        let output = std::path::PathBuf::from("/tmp/out.png");
        let exec = |n: usize| {
            let mut line = String::from("tool %i %o");
            for at in 0..n {
                use std::fmt::Write as _;
                let _ = write!(line, " 'arg {at}' \"q{at}\" plain{at}");
            }
            line
        };
        work::assert_linear(
            500,
            |n| {
                let line = exec(n);
                let thumbnailer = crate::thumbnailer::Thumbnailer::for_tests(&line);
                let (command, steps) = work::steps(|| {
                    thumbnailer
                        .command(&ThumbnailRequest {
                            source: &source,
                            output: &output,
                            size: 128,
                        })
                        .unwrap()
                });
                assert_eq!(command.args().len(), 2 + 3 * n);
                (steps, line.len())
            },
            // The line split (a step per byte), every token scanned for
            // codes and expanded (two more passes).
            |_, bytes| 2 * (4 * bytes + 40),
        );
    }
}

#[cfg(test)]
mod review_regressions {
    use super::*;

    fn split_ok(line: &str) -> Vec<String> {
        split(line).unwrap()
    }

    #[test]
    fn only_space_tab_and_newline_separate() {
        // A no-break space is token content, as GLib treats it.
        assert_eq!(
            split_ok("tool\u{00A0}name %u %o"),
            ["tool\u{00A0}name", "%u", "%o"]
        );
        assert_eq!(split_ok("a\tb\nc d"), ["a", "b", "c", "d"]);
        // Other Unicode whitespace stays inside tokens too.
        assert_eq!(split_ok("a\u{2003}b"), ["a\u{2003}b"]);
    }

    #[test]
    fn a_comment_ends_its_line_not_the_command() {
        assert_eq!(
            split_ok("tool %u # comment with %z\n%o"),
            ["tool", "%u", "%o"],
            "parsing resumes after the newline"
        );
        assert_eq!(split_ok("# all of it\n"), Vec::<String>::new());
        // Inside double quotes a backslash-newline keeps the newline
        // itself, as GLib does (2.88): the backslash goes, the break
        // stays.
        assert_eq!(split_ok("\"a\\\nb\""), ["a\nb"]);
        // After a tab, `#` is an ordinary token, not a comment; after a
        // newline it is one; inside a token it is content (all checked
        // against GLib 2.88 directly).
        assert_eq!(
            split_ok("tool\t#literal %u %o"),
            ["tool", "#literal", "%u", "%o"]
        );
        assert_eq!(split_ok("tool\n#line %o"), ["tool"]);
        assert_eq!(split_ok("a#b"), ["a#b"]);
        // A single-quoted newline is content.
        assert_eq!(split_ok("'a\nb'"), ["a\nb"]);
    }
}
