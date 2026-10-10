//! Validation of desktop entries against the Desktop Entry spec and the
//! Desktop Menu spec's registries, at the level of checking
//! `desktop-file-validate` does: an [`Issue`] of level [`Level::Error`]
//! makes a file invalid; the other levels do not.

use std::fmt;

use crate::entry::{
    DESKTOP_ACTION, DESKTOP_ENTRY, KDE_DESKTOP_ENTRY, before_1_0, comma_list, list_items,
    valid_dbus_name,
};
use crate::error::quoted;
use crate::exec::{CommandLine, ExecError, ExecProblem};
use crate::key_file::{Group, KeyFile, KeyNames, valid_group_name, valid_key_name, work};
use crate::keys::{self, ACTION_KEYS, Applies, DESKTOP_KEYS, KeyDef, Status, ValueType};
use crate::registry::{ADDITIONAL_CATEGORIES, DESKTOPS, MAIN_CATEGORIES, RESERVED_CATEGORIES};

/// How bad an [`Issue`] is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    /// A suggestion.
    Hint,
    /// Allowed, but deprecated or unwise.
    Warning,
    /// Allowed for now; an error in a later version of the rules.
    FutureError,
    /// The file breaks the spec.
    Error,
}

/// One problem `validate` found.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Issue {
    /// How bad it is.
    pub level: Level,
    /// The line, counted from 1, when it is about one line.
    pub line: Option<usize>,
    /// What is wrong.
    pub message: String,
}

impl fmt::Display for Issue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let level = match self.level {
            Level::Hint => "hint",
            Level::Warning => "warning",
            Level::FutureError => "error (will be fatal in the future)",
            Level::Error => "error",
        };
        if let Some(line) = self.line {
            write!(f, "line {line}: ")?;
        }
        write!(f, "{level}: {}", self.message)
    }
}

/// Entry types besides `Application`, `Link` and `Directory` that the
/// spec still names: KDE's (historically reserved) and `MimeType`
/// (deprecated).
const OTHER_TYPES: &[(&str, Level)] = &[
    ("ServiceType", Level::Hint),
    ("Service", Level::Hint),
    ("FSDevice", Level::Hint),
    ("MimeType", Level::Warning),
];

/// The spec versions a `Version` key may name.
fn known_version(value: &str) -> bool {
    matches!(value, "1.0" | "1.1" | "1.2" | "1.3" | "1.4" | "1.5")
        || value
            .strip_prefix("0.9.")
            .is_some_and(|minor| matches!(minor, "3" | "4" | "5" | "6" | "7" | "8"))
}

struct Validator<'a> {
    file: &'a KeyFile,
    /// `[Desktop Entry]`, or the deprecated `[KDE Desktop Entry]`.
    main: &'static str,
    issues: Vec<Issue>,
}

impl Validator<'_> {
    fn push(&mut self, level: Level, line: Option<usize>, message: impl Into<String>) {
        let message = message.into();
        // A step per byte of the message, which is written with whatever
        // input it shows.
        work::count(message.len());
        self.issues.push(Issue {
            level,
            line,
            message,
        });
    }

    fn error(&mut self, line: Option<usize>, message: impl Into<String>) {
        self.push(Level::Error, line, message);
    }
}

/// Checks `file` as a desktop entry. When the key file knows its path, the
/// file name is checked too (`.desktop`, `.directory`, a D-Bus name for
/// `DBusActivatable`). The issues come in line order, issues without a
/// line last.
#[must_use]
pub fn validate(file: &KeyFile) -> Vec<Issue> {
    // The checks run in passes, each in file order; stable, so that the
    // issues of one line keep the order they were found in.
    let mut issues = check_file(file);
    issues.sort_by_key(|issue| (issue.line.is_none(), issue.line));
    issues
}

fn check_file(file: &KeyFile) -> Vec<Issue> {
    let mut v = Validator {
        file,
        main: DESKTOP_ENTRY,
        issues: Vec::new(),
    };
    let text = file.to_bytes();
    if text.is_empty() {
        v.error(None, "the file is empty");
        return v.issues;
    }
    check_lines(&mut v, &text);
    for diagnostic in file.diagnostics() {
        work::count(1 + diagnostic.message.len());
        v.error(diagnostic.line, diagnostic.message.clone());
    }
    // A comment may hold any bytes; one that is not UTF-8 is warned about,
    // as `desktop-file-validate` does.
    for line in file.comments_not_utf8() {
        v.push(
            Level::Warning,
            Some(line),
            "the comment is not UTF-8 encoded",
        );
    }
    let groups = file.groups();
    match groups.first() {
        Some(&DESKTOP_ENTRY) => {}
        Some(&KDE_DESKTOP_ENTRY) => {
            v.main = KDE_DESKTOP_ENTRY;
            v.push(
                Level::Warning,
                file.group_line(KDE_DESKTOP_ENTRY),
                format!("[{KDE_DESKTOP_ENTRY}] is deprecated; use [{DESKTOP_ENTRY}]"),
            );
        }
        _ => {
            v.error(None, format!("the first group is not [{DESKTOP_ENTRY}]"));
            return v.issues;
        }
    }
    let main = Main::read(file, v.main);
    for group in &groups {
        check_group(&mut v, group, &main);
    }
    check_names(&mut v);
    check_file_name(&mut v, &main);
    v.issues
}

/// Line-level rules the key-file layer accepts: carriage returns, and
/// spaces or tabs before a line or after a group header, which
/// `desktop-file-validate` rejects.
fn check_lines(v: &mut Validator<'_>, text: &[u8]) {
    for (index, line) in text
        .split(|&byte| {
            work::count(1);
            byte == b'\n'
        })
        .enumerate()
    {
        work::count(1);
        let number = Some(index + 1);
        if line.ends_with(b"\r") {
            v.error(
                number,
                "the line ends with a carriage return; lines end with a line feed only",
            );
        }
        if line.first() == Some(&b' ') || line.first() == Some(&b'\t') {
            v.error(number, "the line starts with a space");
        }
        let trimmed = line.strip_suffix(b"\r").unwrap_or(line);
        if trimmed.starts_with(b"[") && (trimmed.ends_with(b" ") || trimmed.ends_with(b"\t")) {
            v.error(number, "a group header ends with a space");
        }
    }
}

/// What the main group says that the other checks need.
struct Main {
    entry_type: Option<String>,
    /// A `Version` before 1.0, whose booleans may be `0` and `1` and whose
    /// lists may be separated by commas.
    before_1_0: bool,
    /// The interfaces `Implements` lists.
    implements: std::collections::HashSet<String>,
    /// The ids `Actions` lists.
    actions: std::collections::HashSet<String>,
    dbus_activatable: bool,
    only_show_in: Option<Vec<String>>,
    path: Option<std::path::PathBuf>,
}

impl Main {
    fn read(file: &KeyFile, group: &str) -> Self {
        let before_1_0 = before_1_0(file.string(group, "Version").as_deref());
        let list = |key: &str| {
            file.raw(group, key, None)
                .map(|raw| list_items(raw, before_1_0))
        };
        Self {
            entry_type: file.string(group, "Type"),
            before_1_0,
            implements: list("Implements")
                .unwrap_or_default()
                .into_iter()
                .inspect(|interface| work::count(1 + 2 * interface.len()))
                .collect(),
            dbus_activatable: matches!(
                file.raw(group, "DBusActivatable", None),
                Some("true" | "1")
            ),
            only_show_in: list("OnlyShowIn"),
            actions: list("Actions")
                .unwrap_or_default()
                .into_iter()
                .inspect(|id| work::count(1 + 2 * id.len()))
                .collect(),
            path: file.path().map(|path| {
                work::count(path.as_os_str().len());
                path.to_owned()
            }),
        }
    }

    /// The items of a list value of this file.
    fn items(&self, raw: &str) -> Vec<String> {
        list_items(raw, self.before_1_0)
    }

    fn is(&self, entry_type: &str) -> bool {
        self.entry_type.as_deref() == Some(entry_type)
    }
}

fn check_group(v: &mut Validator<'_>, group: &str, main: &Main) {
    // A step, and the name (or its action id) hashed and compared when
    // looked for in `Actions` or `Implements`.
    work::count(1 + 2 * group.len());
    let line = v.file.group_line(group);
    if !valid_group_name(group) {
        // Already reported by the key-file layer.
        return;
    }
    let defs: &[KeyDef] = if group == v.main {
        DESKTOP_KEYS
    } else if let Some(id) = group.strip_prefix(DESKTOP_ACTION) {
        if id.is_empty() {
            v.error(
                line,
                format!("[{}] is an action group with no action id", quoted(group)),
            );
        } else if !valid_action_id(id) {
            v.error(
                line,
                format!(
                    "[{}] has an invalid action id: only letters, digits and '-' are allowed",
                    quoted(group)
                ),
            );
        } else if !main.actions.contains(id) {
            v.error(
                line,
                format!(
                    "[{}] exists, but Actions does not list \"{}\"",
                    quoted(group),
                    quoted(id)
                ),
            );
        }
        ACTION_KEYS
    } else if group.starts_with("X-") {
        return;
    } else if main.implements.contains(group) {
        // Details of an interface the entry implements (spec "Interfaces").
        return;
    } else {
        v.error(
            line,
            format!(
                "[{}] is not a group of the spec; groups extending the format start with \"X-\"",
                quoted(group)
            ),
        );
        return;
    };
    // Found once, so that the work per key does not grow with its name.
    if let Some(found) = v.file.group(group) {
        for (key, locale) in found.keys() {
            work::count(1);
            check_key(v, group, found, key, locale, defs, main);
        }
    }
    check_required(v, group, main);
}

/// The spec's locale form, `lang_COUNTRY.ENCODING@MODIFIER`, where
/// `_COUNTRY`, `.ENCODING` and `@MODIFIER` may be left out; each part is
/// letters, digits and `-`, and an encoding may also hold `_` and `.`, as
/// codeset names do (`ISO_8859-1`, `ANSI_X3.4-1968`).
fn valid_locale(locale: &str) -> bool {
    let made_of = |part: &str, extra: &[char]| {
        !part.is_empty()
            && part
                .chars()
                .inspect(|_| work::count(1))
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || extra.contains(&c))
    };
    let part = |part: &str| made_of(part, &[]);
    // The three splits below each look at a character at most once.
    work::count(3 * locale.len());
    let (rest, modifier) = locale
        .split_once('@')
        .map_or((locale, None), |(rest, modifier)| (rest, Some(modifier)));
    let (rest, encoding) = rest
        .split_once('.')
        .map_or((rest, None), |(rest, encoding)| (rest, Some(encoding)));
    let (lang, country) = rest
        .split_once('_')
        .map_or((rest, None), |(lang, country)| (lang, Some(country)));
    part(lang)
        && encoding.is_none_or(|encoding| made_of(encoding, &['_', '.']))
        && [country, modifier].into_iter().flatten().all(part)
}

fn valid_action_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .chars()
            .inspect(|_| work::count(1))
            .all(|c| c.is_ascii_alphanumeric() || c == '-')
}

fn check_key(
    v: &mut Validator<'_>,
    group: &str,
    found: Group<'_>,
    key: &str,
    locale: Option<&str>,
    defs: &[KeyDef],
    main: &Main,
) {
    let line = found.line_of(key, locale);
    if !valid_key_name(key) {
        // The key-file layer reports it, unless the file takes other key
        // names.
        if v.file.key_names() != KeyNames::DesktopEntry && v.file.key_names().allows(key) {
            let written =
                locale.map_or_else(|| key.to_owned(), |locale| format!("{key}[{locale}]"));
            work::count(written.len());
            v.error(
                line,
                format!(
                    "key \"{}\" has characters a key name may not have",
                    quoted(&written)
                ),
            );
        }
        return;
    }
    let shown = match locale {
        Some(locale) => format!("{}[{}]", quoted(key), quoted(locale)),
        None => quoted(key),
    };
    work::count(shown.len());
    if let Some(locale) = locale.filter(|locale| !valid_locale(locale)) {
        v.error(
            line,
            format!(
                "{shown}: \"{}\" is not a locale (lang_COUNTRY.ENCODING@MODIFIER)",
                quoted(locale)
            ),
        );
    }
    let Some(def) = keys::find(defs, key) else {
        if !key.starts_with("X-") {
            v.error(
                line,
                format!(
                    "{shown} is not a key of the spec; keys extending the format start with \"X-\""
                ),
            );
        }
        return;
    };
    if locale.is_some() {
        if !def.value_type.localized() {
            v.error(line, format!("{shown}: {key} is not a localized key"));
            return;
        }
        // Found without reading the untranslated value, so that the work
        // per translation does not grow with it.
        if !found.has(key, None) {
            v.error(
                line,
                format!("{shown} is translated, but there is no untranslated {key}"),
            );
        }
    }
    match def.status {
        Status::Deprecated => v.push(Level::Warning, line, format!("{key} is deprecated")),
        Status::Reserved => v.push(
            Level::Hint,
            line,
            format!("{key} is a historically reserved key"),
        ),
        Status::Current => {}
    }
    if group == v.main {
        let wanted = match def.applies {
            Applies::Any => None,
            Applies::Application => Some("Application"),
            Applies::Link => Some("Link"),
        };
        if let (Some(wanted), Some(actual)) = (wanted, main.entry_type.as_deref())
            && actual != wanted
            && (known_type(actual) || OTHER_TYPES.iter().any(|(name, _)| *name == actual))
        {
            v.error(
                line,
                format!("{key} is only for Type={wanted}, but the type is {actual}"),
            );
        }
    }
    let Some(raw) = found.raw(key, locale) else {
        return;
    };
    check_value(v, line, &shown, def.value_type, raw, main.before_1_0);
    check_meaning(v, line, group, key, raw, main);
}

fn known_type(value: &str) -> bool {
    matches!(value, "Application" | "Link" | "Directory")
}

/// The escapes in `raw` that the spec does not give its value type, in
/// order: strings have `\s \n \t \r \\`, and lists `\;` as well.
/// Reading keeps any other as written. A backslash at the end is not
/// counted here; see [`ends_in_backslash`].
fn unknown_escapes(raw: &str, list: bool) -> impl Iterator<Item = char> {
    let mut chars = raw.chars().inspect(|_| work::count(1));
    std::iter::from_fn(move || {
        while let Some(c) = chars.next() {
            if c == '\\' {
                match chars.next() {
                    Some('s' | 'n' | 't' | 'r' | '\\') | None => {}
                    Some(';') if list => {}
                    Some(other) => return Some(other),
                }
            }
        }
        None
    })
}

/// Whether `raw` ends in a backslash that escapes nothing. Reading keeps
/// it as one backslash.
fn ends_in_backslash(raw: &str) -> bool {
    raw.bytes()
        .rev()
        .inspect(|_| work::count(1))
        .take_while(|&b| b == b'\\')
        .count()
        % 2
        == 1
}

/// The value type's rules: strings are printable ASCII and use only the
/// escapes the spec lists; booleans `true` or `false`.
fn check_value(
    v: &mut Validator<'_>,
    line: Option<usize>,
    key: &str,
    value_type: ValueType,
    raw: &str,
    before_1_0: bool,
) {
    let list = matches!(
        value_type,
        ValueType::StringList | ValueType::LocaleStringList
    );
    // Files before version 1.0 may separate lists by commas (spec
    // "Deprecated Items").
    if before_1_0 && list && comma_list(raw) {
        v.push(
            Level::Warning,
            line,
            format!("{key}: a list separated by commas is deprecated; use ';'"),
        );
    }
    let (kind, known) = if list {
        ("a list", r"\s, \n, \t, \r, \\ or \;")
    } else {
        ("a string", r"\s, \n, \t, \r or \\")
    };
    if value_type != ValueType::Boolean
        && let Some(escape) = unknown_escapes(raw, list).next()
    {
        // The command line's own escape needs its backslash escaped too.
        let hint = if key == "Exec" && matches!(escape, '"' | '`' | '$') {
            format!(r"; inside quotes, write \\{escape} in the file")
        } else {
            String::new()
        };
        v.error(
            line,
            format!(
                "{key}: a backslash before {escape:?} is not an escape in {kind}; use {known}{hint}"
            ),
        );
    }
    if value_type != ValueType::Boolean && ends_in_backslash(raw) {
        v.error(
            line,
            format!(
                r"{key}: a backslash at the end is not an escape in {kind}; use \\ for a backslash"
            ),
        );
    }
    match value_type {
        ValueType::String | ValueType::StringList => {
            if let Some(bad) = raw
                .chars()
                .inspect(|_| work::count(1))
                .find(|&c| !c.is_ascii() || c.is_ascii_control())
            {
                v.error(
                    line,
                    format!(
                        "{key}: {bad:?} is not allowed in a string; strings are printable ASCII"
                    ),
                );
            }
        }
        // Files before version 1.0 may use 0 and 1 (spec "Deprecated Items").
        ValueType::Boolean if before_1_0 && matches!(raw, "0" | "1") => {
            v.push(
                Level::Warning,
                line,
                format!("{key}: {raw:?} is a deprecated boolean; use true or false"),
            );
        }
        ValueType::Boolean => {
            if !matches!(raw, "true" | "false") {
                v.error(
                    line,
                    format!(
                        "{key}: \"{}\" is not a boolean; use true or false",
                        quoted(raw)
                    ),
                );
            }
        }
        // UTF-8 already: a line that is not is reported by the key file.
        ValueType::LocaleString | ValueType::IconString | ValueType::LocaleStringList => {}
    }
}

/// The rules of particular keys.
fn check_meaning(
    v: &mut Validator<'_>,
    line: Option<usize>,
    group: &str,
    key: &str,
    raw: &str,
    main: &Main,
) {
    let value = crate::key_file::unescape(raw);
    match key {
        "Type" if group == v.main => {
            if !known_type(&value) {
                match OTHER_TYPES.iter().find(|(name, _)| *name == value) {
                    Some((_, level)) => {
                        v.push(
                            *level,
                            line,
                            format!("Type={value} is not one of Application, Link and Directory"),
                        );
                    }
                    None => v.error(
                        line,
                        format!(
                            "Type={} is not a registered type (Application, Link, Directory)",
                            quoted(&value)
                        ),
                    ),
                }
            }
        }
        "Version" => {
            if !known_version(&value) {
                v.error(
                    line,
                    format!(
                        "Version={} is not a known version of the spec",
                        quoted(&value)
                    ),
                );
            }
        }
        "Icon" => check_icon(v, line, &value),
        "OnlyShowIn" | "NotShowIn" => check_show_in(v, line, group, key, raw, main),
        // A bad escape in the file was reported by `check_value` already.
        "Exec" => check_exec(
            v,
            line,
            &value,
            &unknown_escapes(raw, false).collect(),
            ends_in_backslash(raw),
        ),
        "Path" => {
            if !value.starts_with('/') {
                v.push(
                    Level::Warning,
                    line,
                    format!("Path={} is not an absolute path", quoted(&value)),
                );
            }
        }
        "MimeType" => {
            for mime in main.items(raw) {
                work::count(1);
                if !looks_like_mime_type(&mime) {
                    v.push(
                        Level::FutureError,
                        line,
                        format!("MimeType: \"{}\" is not a MIME type", quoted(&mime)),
                    );
                }
            }
        }
        "Categories" => check_categories(v, line, raw, main),
        "Keywords" => {
            if main
                .items(raw)
                .iter()
                .inspect(|_| work::count(1))
                .any(String::is_empty)
            {
                v.error(line, "Keywords contains an empty keyword");
            }
        }
        "Actions" => check_actions(v, line, raw, main),
        "Implements" => {
            for interface in main.items(raw) {
                work::count(1);
                if !valid_interface_name(&interface) {
                    v.error(
                        line,
                        format!(
                            "Implements: \"{}\" is not a D-Bus interface name",
                            quoted(&interface)
                        ),
                    );
                }
            }
        }
        "Encoding" if !matches!(value.as_str(), "UTF-8" | "Legacy-Mixed") => {
            v.error(
                line,
                format!("Encoding={} is not UTF-8 or Legacy-Mixed", quoted(&value)),
            );
        }
        _ => {}
    }
}

/// An icon is an absolute path or a name without a path or an extension.
fn check_icon(v: &mut Validator<'_>, line: Option<usize>, value: &str) {
    // Looked through for a `/`, then lowered.
    work::count(2 * value.len());
    if value.starts_with('/') {
        if value.ends_with('/') {
            v.error(
                line,
                format!("Icon={} is a folder, not an icon", quoted(value)),
            );
        }
        return;
    }
    if value.contains('/') {
        v.error(
            line,
            format!(
                "Icon={} looks like a relative path; use an absolute path or an icon name",
                quoted(value)
            ),
        );
        return;
    }
    let lower = value.to_ascii_lowercase();
    if [".png", ".xpm", ".svg"]
        .iter()
        .any(|ext| lower.ends_with(ext))
    {
        v.push(
            Level::FutureError,
            line,
            format!(
                "Icon={}: an icon name has no extension (Icon Theme spec)",
                quoted(value)
            ),
        );
    }
}

/// A bad escape inside quotes is left out when the value already has a
/// bad string escape before the same character: it is most likely the
/// same mistake. So the same character written both ways in one value
/// (`"\q" "\\q"`) is reported once, as a string's. Likewise a lone
/// backslash at the end of the value, inside quotes, is reported as a
/// string's, and the quote it leaves open as not closed. Outside quotes
/// it is also a reserved character, as `\\` would be.
fn check_exec(
    v: &mut Validator<'_>,
    line: Option<usize>,
    value: &str,
    string_escapes: &std::collections::HashSet<char>,
    ends_in_backslash: bool,
) {
    match CommandLine::parse(value) {
        Err(ExecError::IncompleteEscape) if ends_in_backslash => {
            v.error(line, format!("Exec: {}", ExecError::UnclosedQuote));
        }
        Err(err) => v.error(line, format!("Exec: {err}")),
        Ok(command) => {
            for problem in command.problems() {
                work::count(1);
                let level = match problem {
                    ExecProblem::Deprecated(_) => Level::Warning,
                    ExecProblem::InvalidEscapeInQuotes(c) if string_escapes.contains(c) => continue,
                    _ => Level::Error,
                };
                v.push(level, line, format!("Exec: {problem}"));
            }
        }
    }
}

/// `type/subtype`, both parts non-empty printable ASCII without spaces.
fn looks_like_mime_type(value: &str) -> bool {
    work::count(value.len());
    let part = |text: &str| {
        !text.is_empty()
            && text
                .chars()
                .inspect(|_| work::count(1))
                .all(|c| c.is_ascii_graphic() && !"()<>@,;:\\\"/[]?=".contains(c))
    };
    value
        .split_once('/')
        .is_some_and(|(media, subtype)| part(media) && (subtype == "*" || part(subtype)))
}

/// `OnlyShowIn` and `NotShowIn`: registered desktops, and none in both.
fn check_show_in(
    v: &mut Validator<'_>,
    line: Option<usize>,
    group: &str,
    key: &str,
    raw: &str,
    main: &Main,
) {
    // "The same desktop name may not appear in both OnlyShowIn and
    // NotShowIn of a group."
    if key == "OnlyShowIn" {
        let not_show_in: std::collections::HashSet<String> = v
            .file
            .raw(group, "NotShowIn", None)
            .map(|raw| main.items(raw))
            .unwrap_or_default()
            .into_iter()
            .inspect(|desktop| work::count(1 + 2 * desktop.len()))
            .collect();
        for desktop in main.items(raw) {
            work::count(1 + 2 * desktop.len());
            if not_show_in.contains(&desktop) {
                v.error(
                    line,
                    format!(
                        "{} is in both OnlyShowIn and NotShowIn of [{}]",
                        quoted(&desktop),
                        quoted(group)
                    ),
                );
            }
        }
    }
    for desktop in main.items(raw) {
        work::count(1);
        if !DESKTOPS.contains(&desktop.as_str()) && !desktop.starts_with("X-") {
            v.error(
                line,
                format!(
                    "{key}: \"{}\" is not a registered desktop; others start with \"X-\"",
                    quoted(&desktop)
                ),
            );
        }
    }
}

fn check_categories(v: &mut Validator<'_>, line: Option<usize>, raw: &str, main: &Main) {
    let categories = main.items(raw);
    let present: std::collections::HashSet<&str> = categories
        .iter()
        .inspect(|category| work::count(1 + 2 * category.len()))
        .map(String::as_str)
        .collect();
    let has = |name: &str| {
        work::count(1);
        present.contains(name)
    };
    let mut main_count = 0;
    for category in &categories {
        work::count(1);
        if let Some((_, required)) = MAIN_CATEGORIES.iter().find(|(name, _)| name == category) {
            main_count += 1;
            if let Some(required) = required
                && !has(required)
            {
                v.error(
                    line,
                    format!("Categories: {category} needs {required} as well"),
                );
            }
        } else if let Some((_, related)) = ADDITIONAL_CATEGORIES
            .iter()
            .find(|(name, _)| name == category)
        {
            if !related.is_empty() && !related.iter().any(|set| set.iter().all(|name| has(name))) {
                let wanted: Vec<String> = related.iter().map(|set| set.join(";")).collect();
                v.push(
                    Level::Hint,
                    line,
                    format!(
                        "Categories: {category} should come with one of {}",
                        wanted.join(", ")
                    ),
                );
            }
        } else if RESERVED_CATEGORIES.contains(&category.as_str()) {
            if main.only_show_in.is_none() {
                v.error(
                    line,
                    format!("Categories: {category} is reserved; it needs OnlyShowIn"),
                );
            }
        } else if !category.starts_with("X-") {
            v.error(
                line,
                format!(
                    "Categories: \"{}\" is not registered; others start with \"X-\"",
                    quoted(category)
                ),
            );
        }
    }
    if main_count > 1 {
        v.push(
            Level::Hint,
            line,
            "Categories: more than one main category; the entry may appear in several menus",
        );
    }
    if main_count == 0 && !categories.is_empty() {
        v.push(Level::Hint, line, "Categories: no main category");
    }
}

fn check_actions(v: &mut Validator<'_>, line: Option<usize>, raw: &str, main: &Main) {
    for id in main.items(raw) {
        // A step, and the id copied into the group name looked for.
        work::count(1 + id.len());
        if id.is_empty() {
            v.error(line, "Actions contains an empty action");
        } else if !valid_action_id(&id) {
            v.error(
                line,
                format!(
                    "Actions: \"{}\" is not a valid action id (letters, digits and '-')",
                    quoted(&id)
                ),
            );
        } else if !v.file.has_group(&format!("{DESKTOP_ACTION}{id}")) {
            v.error(
                line,
                format!(
                    "Actions lists \"{}\", but there is no [{DESKTOP_ACTION}{}] group",
                    quoted(&id),
                    quoted(&id)
                ),
            );
        }
    }
}

/// A D-Bus interface name: like a bus name, but `-` is not allowed.
fn valid_interface_name(name: &str) -> bool {
    valid_dbus_name(name) && {
        work::count(name.len());
        !name.contains('-')
    }
}

fn check_required(v: &mut Validator<'_>, group: &str, main: &Main) {
    let line = v.file.group_line(group);
    let has = |key: &str| v.file.raw(group, key, None).is_some();
    let mut missing: Vec<&str> = Vec::new();
    if group == v.main {
        for key in ["Type", "Name"] {
            if !has(key) {
                missing.push(key);
            }
        }
        if main.is("Link") && !has("URL") {
            missing.push("URL");
        }
    } else if !has("Name") {
        missing.push("Name");
    }
    for key in missing {
        v.error(line, format!("[{}] needs the key {key}", quoted(group)));
    }
    let application = main.is("Application");
    if application && !main.dbus_activatable && !has("Exec") {
        v.error(
            line,
            format!(
                "[{}] needs Exec, as DBusActivatable is not true",
                quoted(group)
            ),
        );
    } else if application && !has("Exec") {
        v.push(
            Level::Warning,
            line,
            format!(
                "[{}] should have Exec for launchers that do not know DBusActivatable",
                quoted(group)
            ),
        );
    }
}

/// Names in the main group that repeat each other.
fn check_names(v: &mut Validator<'_>) {
    let (file, main) = (v.file, v.main);
    let name = file.raw(main, "Name", None);
    let comment = file.raw(main, "Comment", None);
    let generic = file.raw(main, "GenericName", None);
    let line = file.line_of(main, "Comment", None);
    // Each compared with the others.
    work::count(
        2 * [name, comment, generic]
            .iter()
            .flatten()
            .map(|text| text.len())
            .sum::<usize>(),
    );
    if comment.is_some() && (comment == name || comment == generic) {
        v.push(Level::Warning, line, "Comment repeats Name or GenericName");
    }
    if generic.is_some() && generic == name {
        v.push(
            Level::Warning,
            file.line_of(main, "GenericName", None),
            "GenericName repeats Name",
        );
    }
}

/// `.desktop`, or `.directory` for `Type=Directory`; a D-Bus name for a
/// `DBusActivatable` application.
fn check_file_name(v: &mut Validator<'_>, main: &Main) {
    let Some(name) = main
        .path
        .as_deref()
        .and_then(std::path::Path::file_name)
        .and_then(|name| name.to_str())
    else {
        return;
    };
    if main.is("Directory") {
        if !name.ends_with(".directory") {
            v.error(
                None,
                "the file is of type Directory, but its name does not end in .directory",
            );
        }
    } else if !name.ends_with(".desktop") {
        v.error(None, "the file's name does not end in .desktop");
    }
    if main.dbus_activatable && !name.strip_suffix(".desktop").is_some_and(valid_dbus_name) {
        v.error(
            None,
            "DBusActivatable needs a file name that is a D-Bus name (reverse DNS) plus .desktop",
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn errors(text: &str) -> Vec<String> {
        validate(&KeyFile::parse(text.as_bytes()))
            .into_iter()
            .filter(|issue| issue.level == Level::Error)
            .map(|issue| issue.message)
            .collect()
    }

    const GOOD: &str =
        "[Desktop Entry]\nType=Application\nName=Foo\nExec=foo %U\nCategories=Utility;\n";

    #[test]
    fn application_and_link_keys_are_errors_on_kde_and_mimetype_types() {
        for kind in ["Service", "ServiceType", "FSDevice", "MimeType"] {
            let errors = errors(&format!(
                "[Desktop Entry]\nType={kind}\nName=Foo\nExec=foo\nURL=https://example.org/\n"
            ));
            assert_eq!(
                errors,
                [
                    format!("Exec is only for Type=Application, but the type is {kind}"),
                    format!("URL is only for Type=Link, but the type is {kind}"),
                ],
                "{kind}"
            );
        }
        // An unknown type is reported once, not again for each key.
        let errors = errors("[Desktop Entry]\nType=Thing\nName=Foo\nExec=foo\nURL=x\n");
        assert_eq!(errors.len(), 1, "{errors:?}");
    }

    #[test]
    fn names_of_safe_characters_are_cut_too() {
        let long = "a".repeat(5000);
        let start = "[Desktop Entry]\nType=Application\nName=a\nExec=a\n";
        for rest in [
            format!("Actions={long};\n"),
            format!("[Desktop Action {long}]\nExec=a\n"),
            format!("[{long}]\n"),
            format!("{long}=a\n"),
            format!("OnlyShowIn=X-{long};\nNotShowIn=X-{long};\n"),
        ] {
            let issues = validate(&KeyFile::parse(format!("{start}{rest}").as_bytes()));
            assert!(!issues.is_empty(), "{rest:?}");
            for issue in issues {
                let shown: String = issue.message.chars().take(100).collect();
                assert!(issue.message.chars().count() <= 150, "{shown:?}…");
            }
        }
    }

    #[test]
    fn text_from_the_file_is_shown_escaped_and_cut() {
        // An escape character, a line feed once unescaped, and length.
        let bad = format!("\x1b\\n{}", "a".repeat(5000));
        let start = "[Desktop Entry]\nType=Application\nName=a\nExec=a\n";
        let lines = [
            format!("Version={bad}"),
            format!("Icon=a/{bad}"),
            format!("Icon=/{bad}/"),
            format!("Icon={bad}.png"),
            format!("Path={bad}"),
            format!("MimeType={bad};"),
            format!("Implements={bad};"),
            format!("Encoding={bad}"),
            format!("OnlyShowIn={bad};\nNotShowIn={bad};"),
            format!("Categories={bad};"),
            format!("Actions={bad};"),
            format!("Terminal={bad}"),
            format!("Name[{bad}]=a"),
            format!("Foo[{bad}]=a"),
            format!("Exec[{bad}]=a"),
            format!("Comment[{bad}]=a"),
            format!("Keywords[{bad}]=\\q;"),
            format!("X{bad}=a"),
            format!("X.{}=a", "a".repeat(5000)),
        ];
        let mut texts: Vec<String> = lines
            .iter()
            .map(|line| format!("{start}{line}\n"))
            .collect();
        texts.push(format!("[Desktop Entry]\nType={bad}\nName=a\n"));
        for text in &texts {
            for file in [
                KeyFile::parse(text.as_bytes()),
                KeyFile::parse_with(text.as_bytes(), KeyNames::Any),
            ] {
                let issues = validate(&file);
                assert!(!issues.is_empty(), "{text:?}");
                for issue in issues {
                    crate::error::assert_shown_safely(&issue.message);
                    crate::error::assert_shown_safely(&issue.to_string());
                }
            }
        }
        let messages: Vec<String> = validate(&KeyFile::parse(texts[0].as_bytes()))
            .into_iter()
            .map(|issue| issue.message)
            .collect();
        let shown = format!("Version=\\u{{1b}}\\n{}…", "a".repeat(30));
        let expected = format!("{shown} is not a known version of the spec");
        assert!(messages.contains(&expected), "{messages:?}");
    }

    #[test]
    fn key_names_are_checked_in_files_that_take_any() {
        let text = format!("{GOOD}image/png=x\na]b=y\n");
        let errors_of = |file: &KeyFile| -> Vec<String> {
            validate(file)
                .into_iter()
                .filter(|issue| issue.level == Level::Error)
                .map(|issue| issue.message)
                .collect()
        };
        let mut any = errors_of(&KeyFile::parse_with(text.as_bytes(), KeyNames::Any));
        let mut desktop = errors_of(&KeyFile::parse(text.as_bytes()));
        any.sort();
        desktop.sort();
        assert!(!any.is_empty());
        assert_eq!(any, desktop, "the same error either way");
    }

    #[test]
    fn locale_postfixes_have_the_spec_form() {
        for good in [
            "de",
            "de_AT",
            "sr@latin",
            "de_DE.UTF-8@euro",
            "ca@valencia",
            "en_US.ISO_8859-1",
            "en_US.ANSI_X3.4-1968",
        ] {
            assert!(
                errors(&format!("{GOOD}Name[{good}]=x\n")).is_empty(),
                "{good}"
            );
        }
        for bad in [
            "",
            "de[foo",
            "de[foo]",
            "x y",
            "_AT",
            "de_",
            "de@",
            "de.",
            "de_A_T",
            "de.UTF-8.x@",
        ] {
            let errors = errors(&format!("{GOOD}Name[{bad}]=x\n"));
            assert!(
                errors.iter().any(|error| error.contains("is not a locale")),
                "{bad:?}: {errors:?}"
            );
        }
    }

    #[test]
    fn lists_before_1_0_may_be_separated_by_commas() {
        let issues = validate(&KeyFile::parse(
            b"[Desktop Entry]\nType=Application\nName=Foo\nExec=foo\nVersion=0.9.4\n\
              OnlyShowIn=GNOME,KDE\nCategories=Utility,Office\n",
        ));
        assert!(
            issues.iter().all(|issue| issue.level != Level::Error),
            "{issues:?}"
        );
        assert_eq!(
            issues
                .iter()
                .filter(|issue| issue.message.contains("separated by commas"))
                .count(),
            2,
            "{issues:?}"
        );
        // From 1.0 on, a comma is part of the item.
        assert!(
            !errors(
                "[Desktop Entry]\nType=Application\nName=Foo\nExec=foo\nOnlyShowIn=GNOME,KDE\n"
            )
            .is_empty()
        );
    }

    #[test]
    fn dbus_activatable_without_exec_is_warned_about_in_each_group() {
        let issues = validate(&KeyFile::parse(
            b"[Desktop Entry]\nType=Application\nName=Foo\nDBusActivatable=true\nActions=New;\n\
              [Desktop Action New]\nName=New\n",
        ));
        assert!(
            issues.iter().all(|issue| issue.level != Level::Error),
            "{issues:?}"
        );
        let warned: Vec<&str> = issues
            .iter()
            .filter(|issue| {
                issue.level == Level::Warning && issue.message.contains("should have Exec")
            })
            .map(|issue| issue.message.as_str())
            .collect();
        assert_eq!(warned.len(), 2, "{issues:?}");
        assert!(warned[1].starts_with("[Desktop Action New]"), "{warned:?}");
    }

    #[test]
    fn a_good_entry_has_no_errors() {
        assert_eq!(errors(GOOD), Vec::<String>::new());
        assert_eq!(
            errors("[Desktop Entry]\nType=Link\nName=Site\nURL=https://example.org\n"),
            Vec::<String>::new()
        );
    }

    #[test]
    fn a_comment_that_is_not_utf8_is_a_warning() {
        let mut text = GOOD.as_bytes().to_vec();
        text.extend_from_slice(b"# caf\xe9\n# fine\n  #\xff\n");
        let issues = validate(&KeyFile::parse(&text));
        let found: Vec<_> = issues
            .iter()
            .map(|issue| (issue.level, issue.line, issue.message.as_str()))
            .collect();
        let warning = "the comment is not UTF-8 encoded";
        assert_eq!(
            found,
            [
                (Level::Warning, Some(6), warning),
                (Level::Error, Some(8), "the line starts with a space"),
                (Level::Warning, Some(8), warning),
            ]
        );
    }

    #[test]
    fn required_keys_and_groups() {
        assert!(errors("").iter().any(|e| e.contains("empty")));
        assert!(errors("[Other]\nk=v\n")[0].contains("first group"));
        assert!(
            errors("[Desktop Entry]\nType=Application\nName=Foo\n")
                .iter()
                .any(|e| e.contains("needs Exec"))
        );
        assert!(
            errors("[Desktop Entry]\nType=Application\nName=Foo\nDBusActivatable=true\n")
                .is_empty()
        );
        assert!(
            errors("[Desktop Entry]\nType=Link\nName=Foo\n")
                .iter()
                .any(|e| e.contains("needs the key URL"))
        );
        assert!(
            errors("[Desktop Entry]\nType=Application\nExec=x\n")
                .iter()
                .any(|e| e.contains("needs the key Name"))
        );
    }

    #[test]
    fn keys_values_and_types() {
        let with = |extra: &str| errors(&format!("{GOOD}{extra}\n"));
        assert!(with("Unknown=1")[0].contains("not a key of the spec"));
        assert!(with("X-Mine=1").is_empty());
        assert!(with("Terminal=yes")[0].contains("not a boolean"));
        assert!(with("Exec[de]=x")[0].contains("not a localized key"));
        assert!(with("Comment[de]=x")[0].contains("no untranslated"));
        assert!(with("Version=2.0")[0].contains("not a known version"));
        assert!(with("Version=0.9.4").is_empty());
        assert!(
            with("Version=0.9.4\nTerminal=1").is_empty(),
            "0 and 1 before 1.0"
        );
        assert!(with("Version=1.0\nTerminal=1")[0].contains("not a boolean"));
        let kde = validate(&KeyFile::parse(
            b"[KDE Desktop Entry]\nType=Application\nName=a\nExec=a\nTerminal=maybe\n",
        ));
        assert!(
            kde.iter()
                .any(|i| i.level == Level::Warning && i.message.contains("deprecated"))
        );
        assert!(
            kde.iter()
                .any(|i| i.level == Level::Error && i.message.contains("not a boolean")),
            "the old header's group is checked as the main group"
        );
        assert!(with("StartupWMClass=caf\u{e9}")[0].contains("printable ASCII"));
        assert!(
            errors("[Desktop Entry]\nType=Link\nName=a\nURL=u\nExec=x\n")[0]
                .contains("only for Type=Application")
        );
        assert!(
            errors("[Desktop Entry]\nType=Thing\nName=a\n")[0].contains("not a registered type")
        );
        assert!(errors("[Desktop Entry]\nType=Service\nName=a\n").is_empty());
    }

    #[test]
    fn exec_icon_show_in_and_lists() {
        let with = |extra: &str| errors(&format!("{GOOD}{extra}\n"));
        let exec = |value: &str| {
            errors(&format!(
                "[Desktop Entry]\nType=Application\nName=a\nExec={value}\n"
            ))
        };
        assert!(exec("foo %x")[0].contains("%x"));
        assert!(exec("foo ~/x")[0].contains("reserved"));
        assert!(exec("foo %f %u")[0].contains("at most one"));
        assert!(exec(r#"foo "a\\$b""#).is_empty());
        assert_eq!(
            exec(r#"foo "%z""#),
            ["Exec: %z is not a field code the spec lists"]
        );
        assert_eq!(
            exec(r#"foo "% x""#),
            ["Exec: % before ' ' is not a field code the spec lists"]
        );
        assert!(exec(r#"foo "%%z""#).is_empty());
        assert_eq!(
            exec(r#"foo "%d""#),
            ["Exec: field codes may not be used inside quotes"]
        );
        let deprecated = validate(&KeyFile::parse(
            b"[Desktop Entry]\nType=Application\nName=a\nExec=foo \"%d\"\n",
        ));
        assert!(deprecated.iter().any(|issue| issue.level == Level::Warning
            && issue.message == "Exec: %d is deprecated and is dropped"));
        assert!(with("Icon=icons/foo")[0].contains("relative path"));
        let icon_ext = validate(&KeyFile::parse(format!("{GOOD}Icon=foo.png\n").as_bytes()));
        assert!(icon_ext.iter().any(|i| i.level == Level::FutureError));
        assert!(
            with("OnlyShowIn=GNOME;\nNotShowIn=KDE;").is_empty(),
            "no name in both"
        );
        assert!(with("OnlyShowIn=GNOME;KDE;\nNotShowIn=KDE;")[0].contains("KDE is in both"));
        assert!(with("OnlyShowIn=Weird;")[0].contains("not a registered desktop"));
        assert!(with("OnlyShowIn=X-Weird;").is_empty());
        assert!(with("Keywords=a;;b;")[0].contains("empty keyword"));
        assert!(with("Implements=org.freedesktop.Foo;").is_empty());
        assert!(with("Implements=nodots;")[0].contains("D-Bus interface"));
        assert!(with("Implements=org.exa-mple.Iface;")[0].contains("D-Bus interface"));
        assert!(
            with("Implements=org.example.Iface;\n[org.example.Iface]\nDetail=1").is_empty(),
            "a group for an implemented interface"
        );
        assert!(with("[org.example.Other]\nDetail=1")[0].contains("not a group of the spec"));
    }

    #[test]
    fn an_empty_program_is_an_error() {
        let exec = |value: &str| {
            errors(&format!(
                "[Desktop Entry]\nType=Application\nName=a\nExec={value}\n"
            ))
        };
        assert_eq!(exec(r#""""#), ["Exec: the command line has no program"]);
        assert_eq!(exec(r#""" foo"#), ["Exec: the command line has no program"]);
        assert!(exec(r#"foo """#).is_empty());
    }

    #[test]
    fn a_field_code_in_the_program_is_an_error() {
        let exec = |value: &str| {
            errors(&format!(
                "[Desktop Entry]\nType=Application\nName=a\nExec={value}\n"
            ))
        };
        for value in ["%f", "%k --x", "a%c x"] {
            assert_eq!(
                exec(value),
                ["Exec: the program's name may not contain a field code"],
                "{value}"
            );
        }
        let actions = errors(
            "[Desktop Entry]\nType=Application\nName=a\nExec=a\nActions=n;\n\
             [Desktop Action n]\nName=n\nExec=%U\n",
        );
        assert_eq!(
            actions,
            ["Exec: the program's name may not contain a field code"]
        );
    }

    #[test]
    fn escapes_the_value_type_does_not_list_are_errors() {
        let issues = |extra: &str| -> Vec<(Option<usize>, String)> {
            // No Exec in the first five lines, so that the sixth may be one.
            let text = format!(
                "[Desktop Entry]\nType=Application\nName=Foo\nDBusActivatable=true\n\
                 Categories=Utility;\n{extra}\n"
            );
            validate(&KeyFile::parse(text.as_bytes()))
                .into_iter()
                .filter(|issue| issue.level == Level::Error)
                .map(|issue| (issue.line, issue.message))
                .collect()
        };
        let one = |extra: &str| {
            let found = issues(extra);
            assert_eq!(found.len(), 1, "{extra}: {found:?}");
            assert_eq!(found[0].0, Some(6), "{extra}");
            found[0].1.clone()
        };
        let string =
            r"a backslash before 'q' is not an escape in a string; use \s, \n, \t, \r or \\";
        let list =
            r"a backslash before 'q' is not an escape in a list; use \s, \n, \t, \r, \\ or \;";
        assert_eq!(one(r"Comment=a\qb"), format!("Comment: {string}"));
        assert_eq!(one(r"Name[de]=a\qb"), format!("Name[de]: {string}"));
        assert_eq!(one(r"Icon=a\qb"), format!("Icon: {string}"));
        assert_eq!(
            one(r"StartupWMClass=a\qb"),
            format!("StartupWMClass: {string}")
        );
        assert_eq!(one(r"Keywords=a\q;b;"), format!("Keywords: {list}"));
        assert!(one(r"Comment=a\;b").starts_with("Comment: a backslash before ';' "));
        assert!(
            one("Comment=a\\\u{200b}b").starts_with(r"Comment: a backslash before '\u{200b}' "),
            "an invisible character is shown escaped"
        );
        assert!(issues(r"Comment=\s\n\t\r\\").is_empty());
        assert!(issues(r"Keywords=a\;b;\s\n\t\r\\;").is_empty());
        assert!(issues("Keywords=a;\nKeywords[de]=a\\;b;").is_empty());
        // Booleans and keys outside the spec are not checked for escapes.
        assert!(!one(r"Terminal=a\qb").contains("backslash"));
        assert!(issues(r"X-Foo=a\qb").is_empty());
        // In a quoted argument, `\\q` in the file is `\q` once unescaped.
        assert_eq!(
            one(r#"Exec=echo "abc\\q""#),
            r#"Exec: a backslash before 'q' is not an escape inside quotes; use \", \`, \$ or \\"#
        );
        // `\q` in the file is one mistake, reported once, as a string's.
        assert_eq!(one(r#"Exec=echo "abc\q""#), format!("Exec: {string}"));
        // A bad string escape does not hide a different one inside quotes.
        let messages = |extra: &str| -> Vec<String> {
            issues(extra)
                .into_iter()
                .map(|(_, message)| message)
                .collect()
        };
        let quoted_z =
            r#"Exec: a backslash before 'z' is not an escape inside quotes; use \", \`, \$ or \\"#;
        assert_eq!(
            messages(r#"Exec=echo "a\q" "b\\z""#),
            [format!("Exec: {string}"), quoted_z.to_owned()]
        );
        assert_eq!(
            messages(r#"Exec=echo a\q "b\\z""#),
            [
                format!("Exec: {string}"),
                r"Exec: reserved character '\\' outside quotes".to_owned(),
                quoted_z.to_owned(),
            ]
        );
        assert_eq!(
            messages(r#"Exec=echo "a\"" "b\\z""#),
            [
                r#"Exec: a backslash before '"' is not an escape in a string; use \s, \n, \t, \r or \\; inside quotes, write \\" in the file"#.to_owned(),
                quoted_z.to_owned(),
            ]
        );
        // `\\%` in the file is `\%` once unescaped: the `%` still starts a
        // field code, and an unknown one refuses the command line.
        assert_eq!(
            one(r#"Exec=app "\\%z""#),
            "Exec: %z is not a field code the spec lists"
        );
        // `\%` in the file is a bad string escape as well.
        assert_eq!(
            messages(r#"Exec=app "\%z""#),
            [
                format!("Exec: {}", string.replace("'q'", "'%'")),
                "Exec: %z is not a field code the spec lists".to_owned(),
            ]
        );
        // A quote's escape written with one backslash gets a hint.
        assert_eq!(
            one(r#"Exec=sh -c "echo \"hi\"""#),
            r#"Exec: a backslash before '"' is not an escape in a string; use \s, \n, \t, \r or \\; inside quotes, write \\" in the file"#
        );
        assert!(
            one(r#"Exec=sh -c "echo \$HOME""#).ends_with(r"inside quotes, write \\$ in the file")
        );
        assert!(
            one(r#"Exec=sh -c "echo \`date\`""#).ends_with(r"inside quotes, write \\` in the file")
        );
    }

    #[test]
    fn a_backslash_at_the_end_is_an_error() {
        let with = |extra: &str| errors(&format!("{GOOD}{extra}\n"));
        assert_eq!(
            with(r"Comment=abc\"),
            [
                r"Comment: a backslash at the end is not an escape in a string; use \\ for a backslash"
            ]
        );
        assert!(with(r"Comment=abc\\").is_empty());
        assert!(with(r"Comment=abc\\\\").is_empty());
        assert_eq!(
            with(r"Keywords=a;b\"),
            [
                r"Keywords: a backslash at the end is not an escape in a list; use \\ for a backslash"
            ]
        );
        assert_eq!(
            with(r"Name[de]=a\"),
            [
                r"Name[de]: a backslash at the end is not an escape in a string; use \\ for a backslash"
            ]
        );
        assert_eq!(
            with(r"Icon=a\"),
            [r"Icon: a backslash at the end is not an escape in a string; use \\ for a backslash"]
        );
        assert_eq!(
            with("Actions=New;\n[Desktop Action New]\nName=a\\\nExec=a"),
            [r"Name: a backslash at the end is not an escape in a string; use \\ for a backslash"]
        );
        let exec = |value: &str| {
            errors(&format!(
                "[Desktop Entry]\nType=Application\nName=a\nExec={value}\n"
            ))
        };
        let string =
            r"Exec: a backslash at the end is not an escape in a string; use \\ for a backslash";
        // Inside quotes the backslash is reported once, as a string's; the
        // quote it leaves open is a mistake of its own.
        assert_eq!(exec(r#"foo "a\"#), [string, "Exec: a quote is not closed"]);
        assert_eq!(
            exec(r#"foo "a\\"#),
            ["Exec: the command line ends in an incomplete escape"]
        );
        // Outside quotes a backslash is reserved, escaped in the file or not.
        assert_eq!(
            exec(r"foo a\"),
            [string, r"Exec: reserved character '\\' outside quotes"]
        );
    }

    #[test]
    fn issues_come_in_line_order_then_whole_file_ones() {
        let issues = validate(&KeyFile::parse(
            b"[Desktop Entry]\nType=Application\nUnknown=1\nName=a\nExec=a\n not an entry\n",
        ));
        let lines: Vec<Option<usize>> = issues.iter().map(|issue| issue.line).collect();
        assert_eq!(lines, [Some(3), Some(6), Some(6)], "{issues:?}");
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("foo.txt");
        // The missing Name is found after the group's keys (line 4) and
        // before the next group's (line 9); the file name last of all.
        std::fs::write(
            &path,
            "[Desktop Entry]\nType=Application\nExec=a\nUnknown=1\nActions=n;\n\
             [Desktop Action n]\nName=n\nExec=a\nBad=1\n",
        )
        .unwrap();
        let issues = validate(&KeyFile::read(&path).unwrap());
        let lines: Vec<Option<usize>> = issues.iter().map(|issue| issue.line).collect();
        assert_eq!(lines, [Some(1), Some(4), Some(9), None], "{issues:?}");
    }

    #[test]
    fn many_actions_are_checked_in_linear_time() {
        use std::fmt::Write as _;
        work::assert_linear(
            10_000,
            |n| {
                let mut text =
                    String::from("[Desktop Entry]\nType=Application\nName=a\nExec=a\nActions=");
                for at in 0..n {
                    let _ = write!(text, "A{at};");
                }
                text.push('\n');
                for at in 0..n {
                    let _ = write!(text, "[Desktop Action A{at}]\nName=n\nExec=a\n");
                }
                let file = KeyFile::parse(text.as_bytes());
                let (issues, steps) = work::steps(|| validate(&file));
                let errors: Vec<_> = issues
                    .iter()
                    .filter(|issue| issue.level == Level::Error)
                    .collect();
                assert!(errors.is_empty(), "{errors:?}");
                (steps, text.len())
            },
            // A step per line written out and checked, per group, key or
            // action id checked or listed, per line a lookup visits, per
            // block whose first line is worked out, and per argument, piece
            // or problem of a command line, with the action's group name
            // (about 20 bytes) hashed and compared for each lookup of the
            // group, not for each of its keys: about 517 per action; and per
            // character split off, checked for UTF-8, looked through,
            // unescaped or parsed: about 4 per byte. Twice that.
            |actions, bytes| 1034 * actions + 8 * bytes,
        );
    }

    #[test]
    fn categories_by_the_menu_spec() {
        let categories = |value: &str| {
            errors(&format!(
                "[Desktop Entry]\nType=Application\nName=a\nExec=a\nCategories={value}\n"
            ))
        };
        assert!(categories("AudioVideo;Audio;").is_empty());
        assert!(categories("Audio;")[0].contains("needs AudioVideo"));
        assert!(categories("Made-Up;")[0].contains("not registered"));
        assert!(categories("X-Made-Up;").is_empty());
        assert!(categories("TrayIcon;")[0].contains("needs OnlyShowIn"));
        assert!(categories("Utility;Calculator;").is_empty());
    }

    #[test]
    fn kde_and_lxqt_hint_at_the_registered_qt() {
        let hints = |value: &str| -> Vec<String> {
            let text =
                format!("[Desktop Entry]\nType=Application\nName=a\nExec=a\nCategories={value}\n");
            validate(&KeyFile::parse(text.as_bytes()))
                .into_iter()
                .filter(|issue| issue.level == Level::Hint)
                .map(|issue| issue.message)
                .collect()
        };
        for desktop in ["KDE", "LXQt"] {
            assert_eq!(
                hints(&format!("Utility;{desktop};Qt;")),
                Vec::<String>::new()
            );
            assert_eq!(
                hints(&format!("Utility;{desktop};")),
                [format!("Categories: {desktop} should come with one of Qt")]
            );
        }
    }

    #[test]
    fn long_category_lists_are_checked_in_linear_time() {
        work::assert_linear(
            5_000,
            |n| {
                let mut value = "Building;".repeat(n);
                value.push_str("Development;");
                let text = format!(
                    "[Desktop Entry]\nType=Application\nName=a\nExec=a\nCategories={value}\n"
                );
                let file = KeyFile::parse(text.as_bytes());
                let (issues, steps) = work::steps(|| validate(&file));
                assert!(issues.is_empty(), "{issues:?}");
                (steps, text.len())
            },
            // A step per category split off the list, gathered and checked,
            // and per category looked for, with each hashed: about 28 per
            // category; and per character of the value split off, checked or
            // unescaped, each time it is read: about 7 per byte. Twice that.
            |categories, bytes| 57 * categories + 14 * bytes,
        );
    }

    #[test]
    fn very_long_values_are_read_and_checked_in_linear_time() {
        work::assert_linear(
            5_000,
            |n| {
                // About 100 n bytes each (1 MB at n = 10,000): a name, a
                // command line with quoted arguments, escapes and field codes,
                // and a list.
                let name = "Foo Viewer ".repeat(10 * n);
                let exec = r#""a\\\\b" c%%d "#.repeat(8 * n);
                let mut categories = "Building;".repeat(11 * n);
                categories.push_str("Development;");
                let text = format!(
                    "[Desktop Entry]\nType=Application\nName={name}\nExec=a {exec}\nCategories={categories}\n"
                );
                let (issues, steps) = work::steps(|| validate(&KeyFile::parse(text.as_bytes())));
                assert!(issues.is_empty(), "{issues:?}");
                assert!(
                    steps >= text.len(),
                    "{steps} steps for {} bytes",
                    text.len()
                );
                (steps, text.len())
            },
            // Every character is counted, so that a loop over them going
            // quadratic shows: each value is checked for UTF-8, looked through
            // and unescaped several times over, the list split and the command
            // line parsed, the values copied and the lines indexed: about 13.7
            // steps per byte, items included. Twice that.
            |_, bytes| 28 * bytes,
        );
    }

    #[test]
    fn a_long_untranslated_value_with_many_translations_is_checked_in_linear_time() {
        use std::fmt::Write as _;
        work::assert_linear(
            2_000,
            |n| {
                // An untranslated name as long as it has translations, so
                // that work per translation that grew with it would grow
                // quadratically.
                let mut text = format!(
                    "[Desktop Entry]\nType=Application\nExec=a\nName={}\n",
                    "a".repeat(n)
                );
                for at in 0..n {
                    let _ = writeln!(text, "Name[l{at}]=v");
                }
                let file = KeyFile::parse(text.as_bytes());
                let (issues, steps) = work::steps(|| validate(&file));
                assert!(issues.is_empty(), "{issues:?}");
                (steps, text.len())
            },
            // A step per translation checked, with its key and locale
            // copied, hashed and compared to find its line and the
            // untranslated one, its locale checked and its line numbered;
            // per line written out and checked; and per character of the
            // values checked and unescaped: about 10.1 per byte, items
            // included. Twice that.
            |_, bytes| 21 * bytes,
        );
    }

    #[test]
    fn many_interfaces_are_checked_in_linear_time() {
        use std::fmt::Write as _;
        work::assert_linear(
            5_000,
            |n| {
                let mut text =
                    String::from("[Desktop Entry]\nType=Application\nName=a\nExec=a\nImplements=");
                for at in 0..n {
                    let _ = write!(text, "org.example.I{at};");
                }
                text.push('\n');
                for at in 0..n {
                    let _ = writeln!(text, "[org.example.I{at}]");
                }
                let file = KeyFile::parse(text.as_bytes());
                let (issues, steps) = work::steps(|| validate(&file));
                assert!(issues.is_empty(), "{issues:?}");
                (steps, text.len())
            },
            // A step per interface split off the list, gathered and checked,
            // per line written out and checked, and per group checked or
            // looked up, with each name hashed and compared as it is
            // gathered, indexed and looked for: about 144 per interface; and
            // per character of the list and the headers checked, split off or
            // unescaped: about 7.5 per byte. Twice that.
            |interfaces, bytes| 289 * interfaces + 15 * bytes,
        );
    }

    #[test]
    fn long_show_in_lists_are_checked_in_linear_time() {
        use std::fmt::Write as _;
        work::assert_linear(
            5_000,
            |n| {
                let mut text =
                    String::from("[Desktop Entry]\nType=Application\nName=a\nExec=a\nOnlyShowIn=");
                for at in 0..n {
                    let _ = write!(text, "X-Only{at};");
                }
                text.push_str("\nNotShowIn=");
                for at in 0..n {
                    let _ = write!(text, "X-Not{at};");
                }
                text.push('\n');
                let file = KeyFile::parse(text.as_bytes());
                let (issues, steps) = work::steps(|| validate(&file));
                assert!(issues.is_empty(), "{issues:?}");
                (steps, text.len())
            },
            // A step per desktop split off a list (the lists are split for
            // each check), gathered and checked, with each name hashed and
            // compared: about 63 per pair; and per character of the lists
            // checked, split off or unescaped, each time they are read: about
            // 11 per byte. Twice that.
            |pairs, bytes| 127 * pairs + 22 * bytes,
        );
    }

    #[test]
    fn many_keys_under_a_long_action_id_are_checked_in_linear_time() {
        use std::fmt::Write as _;
        work::assert_linear(
            2_000,
            |n| {
                // An action id as long as its group has keys, so that work
                // per key that grew with the group's name would grow
                // quadratically: extension keys, looked up for their lines,
                // and translations, looked up for their lines and values and
                // for the untranslated value.
                let id = "A".repeat(n);
                let mut text = format!(
                    "[Desktop Entry]\nType=Application\nName=a\nExec=a\nActions={id};\n\
                     [Desktop Action {id}]\nName=n\nExec=a\n"
                );
                for at in 0..n / 2 {
                    let _ = write!(text, "X-K{at}=v\nName[l{at}]=v\n");
                }
                let file = KeyFile::parse(text.as_bytes());
                let (issues, steps) = work::steps(|| validate(&file));
                assert!(issues.is_empty(), "{issues:?}");
                (steps, text.len())
            },
            // A step per line written out and checked and per key checked,
            // listed or looked up, with each key copied, hashed and compared
            // to index it and for each lookup of it, and the group's name
            // hashed and compared a few times for the group, not for each
            // key; and per character split off, checked for UTF-8, looked
            // through or unescaped: about 9.7 per byte, items included.
            // Twice that.
            |_, bytes| 20 * bytes,
        );
    }

    #[test]
    fn actions_must_match_their_groups() {
        let base = "[Desktop Entry]\nType=Application\nName=a\nExec=a\n";
        assert!(
            errors(&format!(
                "{base}Actions=New;\n[Desktop Action New]\nName=New\nExec=a --new\n"
            ))
            .is_empty()
        );
        assert!(errors(&format!("{base}Actions=New;\n"))[0].contains("no [Desktop Action New]"));
        assert!(
            errors(&format!("{base}[Desktop Action New]\nName=New\nExec=a\n"))[0]
                .contains("does not list")
        );
        assert!(
            errors(&format!(
                "{base}Actions=New;\n[Desktop Action New]\nExec=a\n"
            ))
            .iter()
            .any(|e| e.contains("needs the key Name"))
        );
        assert!(errors(&format!("{base}Actions=a b;\n"))[0].contains("not a valid action id"));
        assert!(errors(&format!("{base}[Extra]\nk=v\n"))[0].contains("not a group of the spec"));
    }

    #[test]
    fn line_rules_and_file_names() {
        assert!(
            errors("[Desktop Entry]\r\nType=Application\r\nName=a\r\nExec=a\r\n")[0]
                .contains("carriage return")
        );
        assert!(errors(&format!(" {GOOD}"))[0].contains("starts with a space"));
        let action = "Actions=a;\n[Desktop Action a]\nName=A\nExec=a\n";
        for (header, blank) in [
            ("[Desktop Entry]", " "),
            ("[Desktop Entry]", "\t"),
            ("[Desktop Action a]", " "),
            ("[Desktop Action a]", "\t"),
        ] {
            let text = format!("{GOOD}{action}").replacen(header, &format!("{header}{blank}"), 1);
            let found = errors(&text);
            assert!(
                found == ["a group header ends with a space"],
                "{header:?} and {blank:?}: {found:?}"
            );
        }
        assert!(errors(&format!("{GOOD}Name=again\n"))[0].contains("appears again"));
        let dir = tempfile::tempdir().unwrap();
        let named = |name: &str, text: &str| {
            let path = dir.path().join(name);
            std::fs::write(&path, text).unwrap();
            validate(&KeyFile::read(&path).unwrap())
                .into_iter()
                .filter(|issue| issue.level == Level::Error)
                .map(|issue| issue.message)
                .collect::<Vec<_>>()
        };
        assert!(named("foo.desktop", GOOD).is_empty());
        assert!(named("foo.txt", GOOD)[0].contains(".desktop"));
        assert!(
            named("foo.desktop", "[Desktop Entry]\nType=Directory\nName=a\n")[0]
                .contains(".directory")
        );
        let dbus = "[Desktop Entry]\nType=Application\nName=a\nDBusActivatable=true\n";
        assert!(named("org.example.Foo.desktop", dbus).is_empty());
        assert!(named("foo.desktop", dbus)[0].contains("D-Bus name"));
    }
}
