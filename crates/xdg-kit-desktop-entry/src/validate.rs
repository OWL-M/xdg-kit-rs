//! Validation of desktop entries against the Desktop Entry spec and the
//! Desktop Menu spec's registries, at the level of checking
//! `desktop-file-validate` does: an [`Issue`] of level [`Level::Error`]
//! makes a file invalid; the other levels do not.

use std::fmt;

use crate::entry::{
    DESKTOP_ACTION, DESKTOP_ENTRY, KDE_DESKTOP_ENTRY, before_1_0, comma_list, list_items,
    valid_dbus_name,
};
use crate::exec::{CommandLine, ExecError, ExecProblem};
use crate::key_file::{KeyFile, KeyNames, valid_group_name, valid_key_name, work};
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
        self.issues.push(Issue {
            level,
            line,
            message: message.into(),
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
        work::count(1);
        v.error(diagnostic.line, diagnostic.message.clone());
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

/// Line-level rules the key-file layer accepts: carriage returns and
/// leading spaces, which `desktop-file-validate` rejects.
fn check_lines(v: &mut Validator<'_>, text: &[u8]) {
    for (index, line) in text.split(|&byte| byte == b'\n').enumerate() {
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
        if trimmed.starts_with(b"[") && trimmed.ends_with(b" ") {
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
    implements: Vec<String>,
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
            implements: list("Implements").unwrap_or_default(),
            dbus_activatable: matches!(
                file.raw(group, "DBusActivatable", None),
                Some("true" | "1")
            ),
            only_show_in: list("OnlyShowIn"),
            actions: list("Actions")
                .unwrap_or_default()
                .into_iter()
                .inspect(|_| work::count(1))
                .collect(),
            path: file.path().map(ToOwned::to_owned),
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
    work::count(1);
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
                format!("[{group}] is an action group with no action id"),
            );
        } else if !valid_action_id(id) {
            v.error(
                line,
                format!(
                    "[{group}] has an invalid action id: only letters, digits and '-' are allowed"
                ),
            );
        } else if !main.actions.contains(id) {
            v.error(
                line,
                format!("[{group}] exists, but Actions does not list {id:?}"),
            );
        }
        ACTION_KEYS
    } else if group.starts_with("X-") {
        return;
    } else if main.implements.iter().any(|interface| interface == group) {
        // Details of an interface the entry implements (spec "Interfaces").
        return;
    } else {
        v.error(
            line,
            format!("[{group}] is not a group of the spec; groups extending the format start with \"X-\""),
        );
        return;
    };
    for (key, locale) in v.file.keys(group) {
        work::count(1);
        check_key(v, group, key, locale, defs, main);
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
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || extra.contains(&c))
    };
    let part = |part: &str| made_of(part, &[]);
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
    !id.is_empty() && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

fn check_key(
    v: &mut Validator<'_>,
    group: &str,
    key: &str,
    locale: Option<&str>,
    defs: &[KeyDef],
    main: &Main,
) {
    let line = v.file.line_of(group, key, locale);
    let shown = locale.map_or_else(|| key.to_owned(), |locale| format!("{key}[{locale}]"));
    if !valid_key_name(key) {
        // The key-file layer reports it, unless the file takes other key
        // names.
        if v.file.key_names() != KeyNames::DesktopEntry && v.file.key_names().allows(key) {
            v.error(
                line,
                format!("key {shown:?} has characters a key name may not have"),
            );
        }
        return;
    }
    if let Some(locale) = locale.filter(|locale| !valid_locale(locale)) {
        v.error(
            line,
            format!("{shown}: {locale:?} is not a locale (lang_COUNTRY.ENCODING@MODIFIER)"),
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
        if v.file.raw(group, key, None).is_none() {
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
    let Some(raw) = v.file.raw(group, key, locale) else {
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
    let mut chars = raw.chars();
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
    raw.bytes().rev().take_while(|&b| b == b'\\').count() % 2 == 1
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
            if let Some(bad) = raw.chars().find(|&c| !c.is_ascii() || c.is_ascii_control()) {
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
                    format!("{key}: {raw:?} is not a boolean; use true or false"),
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
                            "Type={value} is not a registered type (Application, Link, Directory)"
                        ),
                    ),
                }
            }
        }
        "Version" => {
            if !known_version(&value) {
                v.error(
                    line,
                    format!("Version={value} is not a known version of the spec"),
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
                    format!("Path={value} is not an absolute path"),
                );
            }
        }
        "MimeType" => {
            for mime in main.items(raw) {
                if !looks_like_mime_type(&mime) {
                    v.push(
                        Level::FutureError,
                        line,
                        format!("MimeType: {mime:?} is not a MIME type"),
                    );
                }
            }
        }
        "Categories" => check_categories(v, line, raw, main),
        "Keywords" => {
            if main.items(raw).iter().any(String::is_empty) {
                v.error(line, "Keywords contains an empty keyword");
            }
        }
        "Actions" => check_actions(v, line, raw, main),
        "Implements" => {
            for interface in main.items(raw) {
                if !valid_interface_name(&interface) {
                    v.error(
                        line,
                        format!("Implements: {interface:?} is not a D-Bus interface name"),
                    );
                }
            }
        }
        "Encoding" if !matches!(value.as_str(), "UTF-8" | "Legacy-Mixed") => {
            v.error(
                line,
                format!("Encoding={value} is not UTF-8 or Legacy-Mixed"),
            );
        }
        _ => {}
    }
}

/// An icon is an absolute path or a name without a path or an extension.
fn check_icon(v: &mut Validator<'_>, line: Option<usize>, value: &str) {
    if value.starts_with('/') {
        if value.ends_with('/') {
            v.error(line, format!("Icon={value} is a folder, not an icon"));
        }
        return;
    }
    if value.contains('/') {
        v.error(
            line,
            format!(
                "Icon={value} looks like a relative path; use an absolute path or an icon name"
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
            format!("Icon={value}: an icon name has no extension (Icon Theme spec)"),
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
    let part = |text: &str| {
        !text.is_empty()
            && text
                .chars()
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
        let not_show_in = v
            .file
            .raw(group, "NotShowIn", None)
            .map(|raw| main.items(raw))
            .unwrap_or_default();
        for desktop in main.items(raw) {
            if not_show_in.contains(&desktop) {
                v.error(
                    line,
                    format!("{desktop} is in both OnlyShowIn and NotShowIn of [{group}]"),
                );
            }
        }
    }
    for desktop in main.items(raw) {
        if !DESKTOPS.contains(&desktop.as_str()) && !desktop.starts_with("X-") {
            v.error(
                line,
                format!("{key}: {desktop:?} is not a registered desktop; others start with \"X-\""),
            );
        }
    }
}

fn check_categories(v: &mut Validator<'_>, line: Option<usize>, raw: &str, main: &Main) {
    let categories = main.items(raw);
    let present: std::collections::HashSet<&str> = categories
        .iter()
        .inspect(|_| work::count(1))
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
                format!("Categories: {category:?} is not registered; others start with \"X-\""),
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
        work::count(1);
        if id.is_empty() {
            v.error(line, "Actions contains an empty action");
        } else if !valid_action_id(&id) {
            v.error(
                line,
                format!("Actions: {id:?} is not a valid action id (letters, digits and '-')"),
            );
        } else if !v.file.has_group(&format!("{DESKTOP_ACTION}{id}")) {
            v.error(
                line,
                format!("Actions lists {id:?}, but there is no [{DESKTOP_ACTION}{id}] group"),
            );
        }
    }
}

/// A D-Bus interface name: like a bus name, but `-` is not allowed.
fn valid_interface_name(name: &str) -> bool {
    valid_dbus_name(name) && !name.contains('-')
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
        v.error(line, format!("[{group}] needs the key {key}"));
    }
    let application = main.is("Application");
    if application && !main.dbus_activatable && !has("Exec") {
        v.error(
            line,
            format!("[{group}] needs Exec, as DBusActivatable is not true"),
        );
    } else if application && !has("Exec") {
        v.push(
            Level::Warning,
            line,
            format!("[{group}] should have Exec for launchers that do not know DBusActivatable"),
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
        let mut text = String::from("[Desktop Entry]\nType=Application\nName=a\nExec=a\nActions=");
        for at in 0..20_000 {
            let _ = write!(text, "A{at};");
        }
        text.push('\n');
        for at in 0..20_000 {
            let _ = write!(text, "[Desktop Action A{at}]\nName=n\nExec=a\n");
        }
        let file = KeyFile::parse(text.as_bytes());
        let (issues, steps) = work::steps(|| validate(&file));
        let errors: Vec<_> = issues
            .iter()
            .filter(|issue| issue.level == Level::Error)
            .collect();
        assert!(errors.is_empty(), "{errors:?}");
        // A step per line written out and checked, per group, key or
        // action id checked or listed, per line a lookup visits, and per
        // block whose first line is worked out: about 25 per action.
        assert!(steps <= 26 * 20_000, "{steps} steps for 20,000 actions");
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
    fn long_category_lists_are_checked_in_linear_time() {
        let mut value = "Building;".repeat(10_000);
        value.push_str("Development;");
        let file = KeyFile::parse(
            format!("[Desktop Entry]\nType=Application\nName=a\nExec=a\nCategories={value}\n")
                .as_bytes(),
        );
        let (issues, steps) = work::steps(|| validate(&file));
        assert!(issues.is_empty(), "{issues:?}");
        // A step per category split off the list, gathered and checked,
        // and per category looked for.
        assert!(steps <= 5 * 10_000, "{steps} steps for 10,000 categories");
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
