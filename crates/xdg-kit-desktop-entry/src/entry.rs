//! A desktop entry: the typed view of a key file (Desktop Entry spec
//! "Recognized desktop entry keys", "Additional applications actions").

use std::collections::HashSet;
use std::ffi::OsString;
use std::io;
use std::path::Path;

use xdg_kit_locale::Locales;

use crate::KeyFile;
use crate::key_file::work;

/// The main group of every desktop entry.
pub const DESKTOP_ENTRY: &str = "Desktop Entry";

/// The header older files use instead of `[Desktop Entry]` (deprecated;
/// read when there is no `[Desktop Entry]`).
pub const KDE_DESKTOP_ENTRY: &str = "KDE Desktop Entry";

/// The prefix of an action's group: `[Desktop Action <id>]`.
pub const DESKTOP_ACTION: &str = "Desktop Action ";

/// The `Type` of an entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EntryType {
    /// An application to launch.
    Application,
    /// A link to a URL.
    Link,
    /// A folder in a menu (usually a `.directory` file).
    Directory,
    /// Any other value; the spec says to ignore such entries.
    Other(String),
}

impl EntryType {
    fn parse(value: &str) -> Self {
        match value {
            "Application" => Self::Application,
            "Link" => Self::Link,
            "Directory" => Self::Directory,
            other => Self::Other(other.to_owned()),
        }
    }
}

/// The desktops the user runs, most specific first, as
/// `$XDG_CURRENT_DESKTOP` lists them (for `OnlyShowIn` and `NotShowIn`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Desktops(Vec<String>);

impl Desktops {
    /// The given desktop names, skipping empty names and keeping only the
    /// first occurrence of each name.
    pub fn new(names: impl IntoIterator<Item = impl Into<String>>) -> Self {
        let mut seen = HashSet::new();
        Self(
            names
                .into_iter()
                .map(Into::into)
                .filter(|name: &String| {
                    // The repeat check: a step, and the name copied, hashed
                    // and compared.
                    work::count(1 + 3 * name.len());
                    !name.is_empty() && seen.insert(name.clone())
                })
                .collect(),
        )
    }

    /// Reads `$XDG_CURRENT_DESKTOP`, a colon-separated list, from this
    /// process's environment.
    #[must_use]
    pub fn from_env() -> Self {
        Self::from_vars(|name| std::env::var_os(name))
    }

    /// Reads `XDG_CURRENT_DESKTOP` from `vars`, which returns the value of
    /// an environment variable by name.
    #[must_use]
    pub fn from_vars(vars: impl Fn(&str) -> Option<OsString>) -> Self {
        let value = vars("XDG_CURRENT_DESKTOP")
            .and_then(|value| value.into_string().ok())
            .unwrap_or_default();
        Self::new(value.split(':'))
    }

    /// The names, most specific first.
    #[must_use]
    pub fn names(&self) -> &[String] {
        &self.0
    }
}

/// Whether `version` (a `Version` value) is before 1.0, whose lists may be
/// separated by commas and whose booleans may be `0` and `1`.
pub(crate) fn before_1_0(version: Option<&str>) -> bool {
    version.is_some_and(|version| version.starts_with("0."))
}

/// The items of a written list. Before 1.0 a list without `;` may be
/// separated by commas (spec "Deprecated Items").
pub(crate) fn list_items(raw: &str, before_1_0: bool) -> Vec<String> {
    if before_1_0 {
        // A step per character looked at for a `;`.
        work::count(raw.len());
        if !raw.contains(';') {
            // And per character looked at for the commas.
            work::count(raw.len());
            return raw
                .split(',')
                .filter(|item| !item.is_empty())
                .inspect(|_| work::count(1))
                .map(crate::key_file::unescape)
                .collect();
        }
    }
    crate::key_file::split_list(raw)
}

/// Whether `raw` is a list separated by commas, as only files before 1.0
/// may write them.
pub(crate) fn comma_list(raw: &str) -> bool {
    work::count(2 * raw.len());
    !raw.contains(';') && raw.contains(',')
}

/// A D-Bus well-known name: two or more elements separated by dots, each
/// `[A-Za-z0-9_-]` and not starting with a digit, at most 255 bytes.
pub(crate) fn valid_dbus_name(name: &str) -> bool {
    if name.len() > 255 {
        return false;
    }
    // Split twice; the element characters are counted as checked.
    work::count(2 * name.len());
    name.split('.').count() >= 2
        && name.split('.').all(|element| {
            !element.is_empty()
                && !element.starts_with(|c: char| c.is_ascii_digit())
                && element
                    .chars()
                    .inspect(|_| work::count(1))
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        })
}

/// A boolean value as `GLib` reads one: `true`/`false`, and also `1`/`0`
/// (which `validate` reports), with spaces or tabs after it ignored.
fn parse_boolean(value: &str) -> Option<bool> {
    match value.trim_end_matches([' ', '\t']) {
        "true" | "1" => Some(true),
        "false" | "0" => Some(false),
        _ => None,
    }
}

/// Whether `OnlyShowIn`/`NotShowIn` let an entry be shown on `desktops`:
/// each desktop in turn, a match in `OnlyShowIn` shows it and a match in
/// `NotShowIn` hides it; with no match, shown unless `OnlyShowIn` is set.
/// The lists are made sets first, so the work grows with the desktops and
/// names, not with their product.
pub(crate) fn shown(
    only_show_in: Option<&[String]>,
    not_show_in: Option<&[String]>,
    desktops: &Desktops,
) -> bool {
    let (only_show_in, not_show_in) = (name_set(only_show_in), name_set(not_show_in));
    let has = |set: &Option<HashSet<&str>>, desktop: &str| {
        // Hashed and compared.
        work::count(1 + 2 * desktop.len());
        set.as_ref().is_some_and(|set| set.contains(desktop))
    };
    for desktop in desktops.names() {
        if has(&only_show_in, desktop) {
            return true;
        }
        if has(&not_show_in, desktop) {
            return false;
        }
    }
    only_show_in.is_none()
}

/// The names of a show list as a set.
fn name_set(list: Option<&[String]>) -> Option<HashSet<&str>> {
    list.map(|list| {
        list.iter()
            .map(|name| {
                // Hashed and compared.
                work::count(1 + 2 * name.len());
                name.as_str()
            })
            .collect()
    })
}

/// A desktop entry (`.desktop` or `.directory` file), read with every line
/// kept; edit it through [`DesktopEntry::key_file_mut`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DesktopEntry {
    file: KeyFile,
}

impl DesktopEntry {
    /// Reads the entry at `path`.
    ///
    /// # Errors
    ///
    /// When the file cannot be read, is not a regular file or is larger
    /// than [`xdg_kit_basedir::MAX_FILE_LEN`]. Problems in its text are
    /// diagnostics of the key file.
    pub fn read(path: impl AsRef<Path>) -> io::Result<Self> {
        KeyFile::read(path).map(Self::from_key_file)
    }

    /// Parses `bytes` as an entry.
    #[must_use]
    pub fn parse(bytes: &[u8]) -> Self {
        Self::from_key_file(KeyFile::parse(bytes))
    }

    /// The entry a key file holds.
    #[must_use]
    pub fn from_key_file(file: KeyFile) -> Self {
        Self { file }
    }

    /// The key file underneath, with every line as written.
    #[must_use]
    pub fn key_file(&self) -> &KeyFile {
        &self.file
    }

    /// The key file, to change keys; the typed accessors read the changes.
    pub fn key_file_mut(&mut self) -> &mut KeyFile {
        &mut self.file
    }

    /// The key file, giving up the entry.
    #[must_use]
    pub fn into_key_file(self) -> KeyFile {
        self.file
    }

    /// The file this was read from, if any.
    #[must_use]
    pub fn path(&self) -> Option<&Path> {
        self.file.path()
    }

    /// The main group: `[Desktop Entry]`, or the deprecated
    /// `[KDE Desktop Entry]` of a file that has only that.
    #[must_use]
    pub fn main_group(&self) -> &'static str {
        if !self.file.has_group(DESKTOP_ENTRY) && self.file.has_group(KDE_DESKTOP_ENTRY) {
            KDE_DESKTOP_ENTRY
        } else {
            DESKTOP_ENTRY
        }
    }

    /// Whether the file follows a version of the spec before 1.0, whose
    /// lists may be separated by commas (spec "Deprecated Items").
    fn before_1_0(&self) -> bool {
        before_1_0(self.version().as_deref())
    }

    /// The untranslated string value of `key` in the main group.
    #[must_use]
    pub fn string(&self, key: &str) -> Option<String> {
        self.file.string(self.main_group(), key)
    }

    /// The value of `key` in the main group in the first of `locales` that
    /// has one, else untranslated.
    #[must_use]
    pub fn locale_string(&self, key: &str, locales: &Locales) -> Option<String> {
        self.file.locale_string(self.main_group(), key, locales)
    }

    /// The boolean value of `key` in the main group; `None` when missing or
    /// not a boolean. `1` and `0` are read as `GLib` reads them (the spec
    /// keeps them for files before version 1.0).
    #[must_use]
    pub fn boolean(&self, key: &str) -> Option<bool> {
        self.file
            .raw(self.main_group(), key, None)
            .and_then(parse_boolean)
    }

    /// The list value of `key` in the main group. In a file before version
    /// 1.0, a value without `;` is split at commas.
    #[must_use]
    pub fn list(&self, key: &str) -> Option<Vec<String>> {
        let raw = self.file.raw(self.main_group(), key, None)?;
        Some(list_items(raw, self.before_1_0()))
    }

    /// `Type`; `None` when missing.
    #[must_use]
    pub fn entry_type(&self) -> Option<EntryType> {
        self.string("Type").map(|value| EntryType::parse(&value))
    }

    /// `Version`: the spec version the file follows.
    #[must_use]
    pub fn version(&self) -> Option<String> {
        self.string("Version")
    }

    /// `Name`, translated.
    #[must_use]
    pub fn name(&self, locales: &Locales) -> Option<String> {
        self.locale_string("Name", locales)
    }

    /// `GenericName`, translated.
    #[must_use]
    pub fn generic_name(&self, locales: &Locales) -> Option<String> {
        self.locale_string("GenericName", locales)
    }

    /// `Comment`, translated.
    #[must_use]
    pub fn comment(&self, locales: &Locales) -> Option<String> {
        self.locale_string("Comment", locales)
    }

    /// `Icon`: an icon name or an absolute path; may be translated.
    #[must_use]
    pub fn icon(&self, locales: &Locales) -> Option<String> {
        self.locale_string("Icon", locales)
    }

    /// `NoDisplay`: exists but is not shown in menus.
    #[must_use]
    pub fn no_display(&self) -> bool {
        self.boolean("NoDisplay").unwrap_or(false)
    }

    /// `Hidden`: deleted, as if the file did not exist.
    #[must_use]
    pub fn hidden(&self) -> bool {
        self.boolean("Hidden").unwrap_or(false)
    }

    /// Whether the file is in an encoding this crate reads: no `Encoding`
    /// key, or `UTF-8` in any case (as `GLib` takes it). The spec says to
    /// ignore a file in an encoding not supported, such as `Legacy-Mixed`.
    #[must_use]
    pub fn supports_encoding(&self) -> bool {
        self.string("Encoding")
            .is_none_or(|encoding| encoding.eq_ignore_ascii_case("UTF-8"))
    }

    /// `OnlyShowIn`.
    #[must_use]
    pub fn only_show_in(&self) -> Option<Vec<String>> {
        self.list("OnlyShowIn")
    }

    /// `NotShowIn`.
    #[must_use]
    pub fn not_show_in(&self) -> Option<Vec<String>> {
        self.list("NotShowIn")
    }

    /// Whether `OnlyShowIn`/`NotShowIn` let the entry be shown on
    /// `desktops`, by the spec's rule: each desktop in turn, a match in
    /// `OnlyShowIn` shows it and one in `NotShowIn` hides it; with no match
    /// it is shown unless `OnlyShowIn` is set.
    #[must_use]
    pub fn shown_in(&self, desktops: &Desktops) -> bool {
        shown(
            self.only_show_in().as_deref(),
            self.not_show_in().as_deref(),
            desktops,
        )
    }

    /// `DBusActivatable`.
    #[must_use]
    pub fn dbus_activatable(&self) -> bool {
        self.boolean("DBusActivatable").unwrap_or(false)
    }

    /// `TryExec`: a program whose absence means the application is not
    /// installed.
    #[must_use]
    pub fn try_exec(&self) -> Option<String> {
        self.string("TryExec")
    }

    /// `Exec`: the command line, string escapes undone.
    #[must_use]
    pub fn exec(&self) -> Option<String> {
        self.string("Exec")
    }

    /// `Path`: the folder to run the program in.
    #[must_use]
    pub fn working_dir(&self) -> Option<String> {
        self.string("Path")
    }

    /// `Terminal`: whether the program runs in a terminal.
    #[must_use]
    pub fn terminal(&self) -> bool {
        self.boolean("Terminal").unwrap_or(false)
    }

    /// `Actions`: the ids of the application's actions.
    #[must_use]
    pub fn action_ids(&self) -> Vec<String> {
        let mut seen = std::collections::HashSet::new();
        self.list("Actions")
            .unwrap_or_default()
            .into_iter()
            .filter(|id| {
                // The repeat check: a step, and the id copied, hashed and
                // compared.
                work::count(1 + 3 * id.len());
                seen.insert(id.clone())
            })
            .collect()
    }

    /// `MimeType`: the types the application opens, as written (wildcards
    /// such as `image/*` included).
    #[must_use]
    pub fn mime_types(&self) -> Vec<String> {
        self.list("MimeType").unwrap_or_default()
    }

    /// `Categories`.
    #[must_use]
    pub fn categories(&self) -> Vec<String> {
        self.list("Categories").unwrap_or_default()
    }

    /// `Implements`: D-Bus interfaces the application implements.
    #[must_use]
    pub fn implements(&self) -> Vec<String> {
        self.list("Implements").unwrap_or_default()
    }

    /// `Keywords`, translated. In a file before version 1.0, a value
    /// without `;` is split at commas, as [`DesktopEntry::list`] does.
    #[must_use]
    pub fn keywords(&self, locales: &Locales) -> Vec<String> {
        self.file
            .locale_raw(self.main_group(), "Keywords", locales)
            .map(|raw| list_items(raw, self.before_1_0()))
            .unwrap_or_default()
    }

    /// `StartupNotify`; `None` when not said.
    #[must_use]
    pub fn startup_notify(&self) -> Option<bool> {
        self.boolean("StartupNotify")
    }

    /// `StartupWMClass`.
    #[must_use]
    pub fn startup_wm_class(&self) -> Option<String> {
        self.string("StartupWMClass")
    }

    /// `URL`, for a link.
    #[must_use]
    pub fn url(&self) -> Option<String> {
        self.string("URL")
    }

    /// `PrefersNonDefaultGPU`.
    #[must_use]
    pub fn prefers_non_default_gpu(&self) -> bool {
        self.boolean("PrefersNonDefaultGPU").unwrap_or(false)
    }

    /// `SingleMainWindow`.
    #[must_use]
    pub fn single_main_window(&self) -> bool {
        self.boolean("SingleMainWindow").unwrap_or(false)
    }

    /// The action `id`, when `Actions` lists it and its group exists with
    /// the required `Name` (the spec says to ignore an action without it).
    #[must_use]
    pub fn action(&self, id: &str) -> Option<Action<'_>> {
        if !self.list("Actions").is_some_and(|ids| {
            ids.iter().any(|listed| {
                work::count(1 + listed.len().min(id.len()));
                listed == id
            })
        }) {
            return None;
        }
        self.listed_action(id)
    }

    /// The actions `Actions` lists that have a group with a `Name`, in its
    /// order, each once.
    #[must_use]
    pub fn actions(&self) -> Vec<Action<'_>> {
        self.action_ids()
            .iter()
            .filter_map(|id| {
                work::count(1);
                self.listed_action(id)
            })
            .collect()
    }

    /// The action `id`, known to be listed, when its group has a `Name`.
    fn listed_action(&self, id: &str) -> Option<Action<'_>> {
        let group = format!("{DESKTOP_ACTION}{id}");
        work::count(group.len());
        self.file
            .raw(&group, "Name", None)
            .is_some()
            .then_some(Action { entry: self, group })
    }
}

/// An application action: a `[Desktop Action <id>]` group.
#[derive(Debug, Clone)]
pub struct Action<'a> {
    entry: &'a DesktopEntry,
    group: String,
}

impl Action<'_> {
    /// The action's id.
    #[must_use]
    pub fn id(&self) -> &str {
        &self.group[DESKTOP_ACTION.len()..]
    }

    /// `Name`, translated.
    #[must_use]
    pub fn name(&self, locales: &Locales) -> Option<String> {
        self.entry.file.locale_string(&self.group, "Name", locales)
    }

    /// `Icon`.
    #[must_use]
    pub fn icon(&self, locales: &Locales) -> Option<String> {
        self.entry.file.locale_string(&self.group, "Icon", locales)
    }

    /// `Exec`.
    #[must_use]
    pub fn exec(&self) -> Option<String> {
        self.entry.file.string(&self.group, "Exec")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FOO: &str = "[Desktop Entry]\n\
        Version=1.5\n\
        Type=Application\n\
        Name=Foo Viewer\n\
        Name[de]=Foo-Betrachter\n\
        Comment=The best viewer in Foo history.\n\
        TryExec=fooview\n\
        Exec=fooview %F\n\
        Icon=fooview\n\
        Icon[de]=fooview-de\n\
        MimeType=image/x-foo;image/*;\n\
        Actions=Gallery;Create;Missing;\n\
        Keywords=foo;view;\n\
        Keywords[de]=foo;ansehen;\n\
        NoDisplay=1\n\
        Terminal=maybe\n\
        \n\
        [Desktop Action Gallery]\n\
        Exec=fooview --gallery\n\
        Name=Browse Gallery\n\
        \n\
        [Desktop Action Create]\n\
        Exec=fooview --create-new\n\
        Name=Create a new Foo!\n\
        Icon=fooview-new\n\
        \n\
        [Desktop Action Unlisted]\n\
        Name=Not in Actions\n";

    #[test]
    fn booleans_and_encodings_as_glib_reads_them() {
        let entry = |extra: &str| {
            DesktopEntry::from_key_file(KeyFile::parse(
                format!("[Desktop Entry]\nType=Application\nName=A\nExec=a\n{extra}").as_bytes(),
            ))
        };
        assert!(entry("Hidden=true \t\n").hidden());
        assert!(!entry("Hidden=True\n").hidden());
        assert!(entry("").supports_encoding());
        assert!(entry("Encoding=UTF-8\n").supports_encoding());
        assert!(entry("Encoding=utf-8\n").supports_encoding());
        assert!(!entry("Encoding=UTF-8 \n").supports_encoding());
        assert!(!entry("Encoding=Legacy-Mixed\n").supports_encoding());
    }

    #[test]
    fn typed_values_from_the_spec_example() {
        let entry = DesktopEntry::parse(FOO.as_bytes());
        let german = Locales::new(["de_DE"]);
        assert_eq!(entry.entry_type(), Some(EntryType::Application));
        assert_eq!(entry.version().as_deref(), Some("1.5"));
        assert_eq!(entry.name(&german).as_deref(), Some("Foo-Betrachter"));
        assert_eq!(
            entry.name(&Locales::untranslated()).as_deref(),
            Some("Foo Viewer")
        );
        assert_eq!(entry.icon(&german).as_deref(), Some("fooview-de"));
        assert_eq!(entry.exec().as_deref(), Some("fooview %F"));
        assert_eq!(entry.try_exec().as_deref(), Some("fooview"));
        assert_eq!(entry.mime_types(), ["image/x-foo", "image/*"]);
        assert_eq!(entry.keywords(&german), ["foo", "ansehen"]);
        assert!(entry.no_display(), "GLib reads 1 as true");
        assert!(!entry.terminal(), "not a boolean: the default");
        assert!(!entry.hidden());
        assert_eq!(entry.startup_notify(), None);
        assert_eq!(
            DesktopEntry::parse(b"[Desktop Entry]\nType=Service\n").entry_type(),
            Some(EntryType::Other("Service".into()))
        );
    }

    #[test]
    fn actions_need_both_the_list_and_the_group() {
        let entry = DesktopEntry::parse(FOO.as_bytes());
        let ids: Vec<_> = entry.actions().iter().map(|a| a.id().to_owned()).collect();
        assert_eq!(ids, ["Gallery", "Create"]);
        let create = entry.action("Create").unwrap();
        assert_eq!(create.exec().as_deref(), Some("fooview --create-new"));
        assert_eq!(
            create.icon(&Locales::untranslated()).as_deref(),
            Some("fooview-new")
        );
        assert!(entry.action("Missing").is_none());
        assert!(entry.action("Unlisted").is_none());
    }

    #[test]
    fn shown_by_the_first_desktop_that_matches() {
        let entry = |text: &str| DesktopEntry::parse(format!("[Desktop Entry]\n{text}").as_bytes());
        let gnome_kde = Desktops::new(["GNOME", "KDE"]);
        assert!(entry("").shown_in(&gnome_kde));
        assert!(entry("OnlyShowIn=KDE;").shown_in(&gnome_kde));
        assert!(!entry("OnlyShowIn=XFCE;").shown_in(&gnome_kde));
        assert!(!entry("NotShowIn=KDE;").shown_in(&gnome_kde));
        assert!(
            entry("OnlyShowIn=KDE;\nNotShowIn=GNOME;").shown_in(&Desktops::new(["KDE", "GNOME"])),
            "the first desktop decides"
        );
        assert!(!entry("OnlyShowIn=KDE;").shown_in(&Desktops::default()));
        let from_env = Desktops::from_vars(|name| {
            (name == "XDG_CURRENT_DESKTOP").then(|| "ubuntu:GNOME".into())
        });
        assert_eq!(from_env.names(), ["ubuntu", "GNOME"]);
        let repeated = Desktops::from_vars(|name| {
            (name == "XDG_CURRENT_DESKTOP").then(|| "GNOME:ubuntu:GNOME::KDE:ubuntu".into())
        });
        assert_eq!(
            repeated.names(),
            ["GNOME", "ubuntu", "KDE"],
            "the first kept"
        );
    }

    #[test]
    fn many_desktops_against_long_show_lists_take_linear_time() {
        use std::fmt::Write as _;
        work::assert_linear(
            2_000,
            |n| {
                // `n` names in each list and `n` desktops, none matching, and
                // a long name in each that differs from the others only at
                // its end.
                let long = "X-Long".to_owned() + &"g".repeat(n);
                let (mut only, mut not) = (String::new(), String::new());
                for at in 0..n {
                    let _ = write!(only, "X-Only{at};");
                    let _ = write!(not, "X-Not{at};");
                }
                let text = format!(
                    "[Desktop Entry]\nType=Application\nName=a\nExec=a\nOnlyShowIn={only}{long}1;\nNotShowIn={not}{long}2;\n"
                );
                let entry = DesktopEntry::parse(text.as_bytes());
                // Each desktop named twice.
                let names: Vec<String> = (0..2 * n)
                    .map(|at| format!("X-Desk{}", at % n))
                    .chain([format!("{long}3"), format!("{long}3")])
                    .collect();
                let bytes = text.len() + names.iter().map(String::len).sum::<usize>();
                let (shown, steps) = work::steps(|| {
                    let desktops = Desktops::new(names);
                    assert_eq!(desktops.names().len(), n + 1);
                    entry.shown_in(&desktops)
                });
                assert!(!shown);
                (steps, bytes)
            },
            // A step per name split off a list and put in a set, and per
            // desktop checked for repeats and looked up: under 1 per item; and
            // per character of the lists split and unescaped, of the names
            // hashed into the sets, and of the desktops copied and hashed for
            // the repeat check and hashed in two lookups: about 5 per byte.
            // Twice that.
            |items, bytes| 2 * items + 10 * bytes,
        );
    }

    #[test]
    fn old_files_kde_headers_and_comma_lists() {
        let old = DesktopEntry::parse(
            b"[KDE Desktop Entry]\nVersion=0.9.4\nType=Application\nName=Old\nExec=old\nMimeType=text/plain,text/html\nTerminal=1\n",
        );
        assert_eq!(old.main_group(), KDE_DESKTOP_ENTRY);
        assert_eq!(old.name(&Locales::untranslated()).as_deref(), Some("Old"));
        assert_eq!(old.mime_types(), ["text/plain", "text/html"]);
        assert!(old.terminal());
        let new = DesktopEntry::parse(b"[Desktop Entry]\nVersion=1.5\nMimeType=a/b,c;\n");
        assert_eq!(new.mime_types(), ["a/b,c"], "commas are kept from 1.0 on");
    }

    #[test]
    fn keywords_of_old_files_are_split_at_commas() {
        let old = DesktopEntry::parse(
            b"[Desktop Entry]\nVersion=0.9.8\nType=Application\nName=Foo\nExec=foo\nKeywords=one,two\nKeywords[de]=eins,zwei\n",
        );
        assert_eq!(old.keywords(&Locales::untranslated()), ["one", "two"]);
        assert_eq!(old.keywords(&Locales::new(["de_DE"])), ["eins", "zwei"]);
        let new = DesktopEntry::parse(
            b"[Desktop Entry]\nVersion=1.5\nKeywords=one,two\nKeywords[de]=a\\;b,c;\n",
        );
        assert_eq!(new.keywords(&Locales::untranslated()), ["one,two"]);
        assert_eq!(new.keywords(&Locales::new(["de_DE"])), ["a;b,c"]);
    }

    #[test]
    fn long_action_lists_are_read_in_linear_time() {
        use std::fmt::Write as _;
        work::assert_linear(
            5_000,
            |n| {
                let mut ids = String::new();
                for at in 0..n {
                    let _ = write!(ids, "A{at};");
                }
                let text = format!(
                    "[Desktop Entry]\nType=Application\nName=a\nExec=a\nActions={ids}\n[Desktop Action A1]\nName=One\n"
                );
                let entry = DesktopEntry::parse(text.as_bytes());
                let ((), steps) = work::steps(|| {
                    assert_eq!(entry.action_ids().len(), n);
                    assert_eq!(entry.actions().len(), 1);
                    assert_eq!(
                        entry.action("A1").map(|a| a.id().to_owned()).as_deref(),
                        Some("A1")
                    );
                });
                (steps, text.len())
            },
            // A step per id split off the list, checked for repeats, looked
            // up or compared, and per line a lookup visits, with each id
            // copied, hashed and compared and each action's group name made
            // and looked up: about 95 per id; and per character of the list
            // checked, split and unescaped, each time it is read: about 8.5
            // per byte. Twice that.
            |ids, bytes| 190 * ids + 17 * bytes,
        );
    }

    #[test]
    fn an_action_without_a_name_is_ignored() {
        let entry = DesktopEntry::parse(
            b"[Desktop Entry]\nType=Application\nName=a\nExec=a\nActions=A;B;B;\n[Desktop Action A]\nExec=a -a\n[Desktop Action B]\nName=B\n",
        );
        let ids: Vec<_> = entry.actions().iter().map(|a| a.id().to_owned()).collect();
        assert_eq!(ids, ["B"]);
    }

    #[test]
    fn changes_through_the_key_file_are_seen() {
        let mut entry = DesktopEntry::parse(FOO.as_bytes());
        entry
            .key_file_mut()
            .set_string(DESKTOP_ENTRY, "Exec", "fooview --new %U")
            .unwrap();
        assert_eq!(entry.exec().as_deref(), Some("fooview --new %U"));
    }
}
