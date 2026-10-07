//! Launching an application (Desktop Entry spec "The Exec key", "D-Bus
//! Activation"): [`prepare_launch`] works out what to run without running
//! anything; [`LaunchPlan::spawn`] runs it.

use std::ffi::OsString;
use std::fmt::{self, Write as _};
use std::io;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};

use xdg_kit_locale::Locales;

use crate::exec::{CommandLine, ExecError, Values};
use crate::{Action, DesktopEntry, EntryType};

/// Something to open: a local path or a URI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// A local file. A relative path is taken relative to the current
    /// folder.
    Path(PathBuf),
    /// A URI, such as `https://example.org/a.pdf` or `file:///tmp/a.pdf`.
    Uri(String),
}

/// How to launch.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct LaunchOptions {
    /// One of the entry's actions instead of its main command.
    pub action: Option<String>,
    /// An XDG activation token (Wayland) from the caller, passed on as
    /// `XDG_ACTIVATION_TOKEN` or the D-Bus `activation-token`.
    pub activation_token: Option<String>,
    /// A startup-notification id (X11) from the caller, passed on as
    /// `DESKTOP_STARTUP_ID` or the D-Bus `desktop-startup-id`.
    pub startup_id: Option<String>,
    /// The command that runs a program in a terminal (such as
    /// `["foot", "--"]`), needed for `Terminal=true`.
    pub terminal: Option<Vec<OsString>>,
    /// The languages for `%c` (the translated name) and `%i`.
    pub locales: Locales,
    /// Use D-Bus activation for `DBusActivatable=true` entries. On by
    /// default when the `dbus` feature is built; without it, entries are
    /// always launched through `Exec`.
    pub dbus: bool,
}

impl Default for LaunchOptions {
    fn default() -> Self {
        Self {
            action: None,
            activation_token: None,
            startup_id: None,
            terminal: None,
            locales: Locales::untranslated(),
            dbus: cfg!(feature = "dbus"),
        }
    }
}

/// One program to start.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct PlannedCommand {
    /// The program: a name looked up in `PATH`, or a path.
    pub program: OsString,
    /// Its arguments.
    pub args: Vec<OsString>,
    /// Variables added to the environment.
    pub env: Vec<(OsString, OsString)>,
    /// Variables removed from the environment: the launcher's own
    /// `XDG_ACTIVATION_TOKEN` and `DESKTOP_STARTUP_ID` when the options give
    /// none, so a spent token is not passed on.
    pub env_remove: Vec<OsString>,
    /// The folder to start it in (`Path`), if any.
    pub working_dir: Option<PathBuf>,
}

/// A method of `org.freedesktop.Application`.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum DBusMethod {
    /// Start without targets.
    Activate,
    /// Open these URIs.
    Open(Vec<String>),
    /// Run the action with this name.
    ActivateAction(String),
}

/// A D-Bus activation call.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct DBusCall {
    /// The application's well-known name: its desktop file name without
    /// `.desktop`.
    pub bus_name: String,
    /// `/` and the name with `.` turned into `/` and `-` into `_`.
    pub object_path: String,
    /// What to call.
    pub method: DBusMethod,
    /// The `platform-data` entries (`activation-token`,
    /// `desktop-startup-id`).
    pub platform_data: Vec<(String, String)>,
}

/// How an application is launched.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Launch {
    /// Start these programs (several when the program takes one file at a
    /// time and several were given).
    Commands(Vec<PlannedCommand>),
    /// Make this D-Bus call.
    DBus(DBusCall),
}

/// What [`prepare_launch`] worked out.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct LaunchPlan {
    /// What to start or call.
    pub launch: Launch,
    /// Whether targets were given that the launch does not pass on (the
    /// command line has no file field code, or an action was asked for
    /// over D-Bus).
    pub targets_ignored: bool,
}

/// Why an entry cannot be launched.
#[derive(Debug)]
#[non_exhaustive]
pub enum LaunchError {
    /// The entry is a link: open its URL with the default handler instead.
    Link(Option<String>),
    /// The entry is not an application (a folder, or an unknown type).
    NotAnApplication(Option<EntryType>),
    /// No such action in the entry.
    NoSuchAction(String),
    /// There is no `Exec` to run.
    NoExec,
    /// The `Exec` command line cannot be used.
    Exec(ExecError),
    /// The program takes local files and this target is not one: copy it
    /// to a local file and launch with that path.
    NeedsLocalCopy(String),
    /// `Terminal=true`, but no terminal command was given.
    NeedsTerminal,
    /// A target cannot be expressed as needed: a URI without a scheme, a
    /// `file:` URI with a host, a NUL byte, a path that cannot be made
    /// absolute.
    BadTarget(String),
    /// Starting `program` failed. The programs started before it are still
    /// running: their handles are in `started`, to be waited on.
    Spawn {
        /// The program that could not be started.
        program: OsString,
        /// Why.
        error: io::Error,
        /// The programs of the plan started before it.
        started: Vec<Child>,
    },
    /// The D-Bus call failed.
    DBus(String),
}

impl fmt::Display for LaunchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Link(url) => write!(
                f,
                "the entry is a link ({}); open it with the default handler",
                url.as_deref().unwrap_or("no URL")
            ),
            Self::NotAnApplication(kind) => write!(f, "the entry is not an application ({kind:?})"),
            Self::NoSuchAction(id) => write!(f, "the entry has no action {id:?}"),
            Self::NoExec => write!(f, "the entry has no Exec to run"),
            Self::Exec(err) => write!(f, "Exec cannot be used: {err}"),
            Self::NeedsLocalCopy(uri) => write!(
                f,
                "{uri} is not a local file and the program takes only local files; copy it first"
            ),
            Self::NeedsTerminal => write!(
                f,
                "the program runs in a terminal, but no terminal was given"
            ),
            Self::BadTarget(target) => write!(f, "cannot pass {target}"),
            Self::Spawn { program, error, .. } => {
                write!(f, "cannot start {}: {error}", program.to_string_lossy())
            }
            Self::DBus(err) => write!(f, "D-Bus activation failed: {err}"),
        }
    }
}

impl std::error::Error for LaunchError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Exec(err) => Some(err),
            Self::Spawn { error, .. } => Some(error),
            _ => None,
        }
    }
}

/// Works out how to launch `entry` with `targets`, without starting
/// anything.
///
/// `DBusActivatable=true` with [`LaunchOptions::dbus`] (and the `dbus`
/// feature) gives a D-Bus call; otherwise the `Exec` command line is
/// expanded: `%f`/`%u` take one target per command, `%F`/`%U` all of them
/// in one; `%f`/`%F` need local files (a `file:` URI is turned into its
/// path, any other URI is [`LaunchError::NeedsLocalCopy`]); `%u`/`%U` get
/// URIs (paths become `file:` URIs).
///
/// # Errors
///
/// When the entry is not an application, the action or `Exec` is missing
/// or unusable, a target cannot be passed, or a terminal is needed and not
/// given.
pub fn prepare_launch(
    entry: &DesktopEntry,
    targets: &[Target],
    options: &LaunchOptions,
) -> Result<LaunchPlan, LaunchError> {
    match entry.entry_type() {
        Some(EntryType::Application) => {}
        Some(EntryType::Link) => return Err(LaunchError::Link(entry.url())),
        other => return Err(LaunchError::NotAnApplication(other)),
    }
    let action = match &options.action {
        Some(id) => Some(
            entry
                .action(id)
                .ok_or_else(|| LaunchError::NoSuchAction(id.clone()))?,
        ),
        None => None,
    };
    if cfg!(feature = "dbus")
        && options.dbus
        && entry.dbus_activatable()
        && let Some(bus_name) = bus_name(entry)
    {
        return dbus_plan(bus_name, action.as_ref(), targets, options);
    }
    exec_plan(entry, action.as_ref(), targets, options)
}

/// The D-Bus activation call for the application named `bus_name`.
fn dbus_plan(
    bus_name: String,
    action: Option<&Action<'_>>,
    targets: &[Target],
    options: &LaunchOptions,
) -> Result<LaunchPlan, LaunchError> {
    let object_path = format!("/{}", bus_name.replace('.', "/").replace('-', "_"));
    let mut platform_data = Vec::new();
    if let Some(token) = &options.activation_token {
        platform_data.push(("activation-token".to_owned(), token.clone()));
    }
    if let Some(id) = &options.startup_id {
        platform_data.push(("desktop-startup-id".to_owned(), id.clone()));
    }
    let (method, targets_ignored) = match (action, targets.is_empty()) {
        (Some(action), _) => (
            DBusMethod::ActivateAction(action.id().to_owned()),
            !targets.is_empty(),
        ),
        (None, true) => (DBusMethod::Activate, false),
        (None, false) => (
            DBusMethod::Open(targets.iter().map(to_uri).collect::<Result<_, _>>()?),
            false,
        ),
    };
    Ok(LaunchPlan {
        launch: Launch::DBus(DBusCall {
            bus_name,
            object_path,
            method,
            platform_data,
        }),
        targets_ignored,
    })
}

/// The commands the `Exec` key (of `action`, if given) expands to.
fn exec_plan(
    entry: &DesktopEntry,
    action: Option<&Action<'_>>,
    targets: &[Target],
    options: &LaunchOptions,
) -> Result<LaunchPlan, LaunchError> {
    let exec = action
        .map_or_else(|| entry.exec(), Action::exec)
        .ok_or(LaunchError::NoExec)?;
    let line = CommandLine::parse(&exec).map_err(LaunchError::Exec)?;
    let (groups, targets_ignored) = target_groups(&line, targets)?;
    let icon = entry.icon(&options.locales);
    let name = entry.name(&options.locales).unwrap_or_default();
    // Absolute: `Path=` changes the folder the program starts in.
    let location = entry
        .path()
        .map(|path| std::path::absolute(path).unwrap_or_else(|_| path.to_owned()));
    let location = location.as_deref().map(Path::as_os_str).unwrap_or_default();
    let mut env = Vec::new();
    let mut env_remove = Vec::new();
    for (name, value) in [
        ("XDG_ACTIVATION_TOKEN", &options.activation_token),
        ("DESKTOP_STARTUP_ID", &options.startup_id),
    ] {
        match value {
            Some(value) => env.push((name.into(), value.into())),
            None => env_remove.push(name.into()),
        }
    }
    let working_dir = entry
        .working_dir()
        .filter(|dir| !dir.is_empty())
        .map(PathBuf::from);
    let mut commands = Vec::new();
    for files in &groups {
        let mut args = line.expand(&Values {
            files,
            icon: icon.as_deref(),
            name: &name,
            location,
        });
        if entry.terminal() {
            let terminal = options
                .terminal
                .as_ref()
                .ok_or(LaunchError::NeedsTerminal)?;
            args.splice(0..0, terminal.iter().cloned());
        }
        if args.is_empty() {
            return Err(LaunchError::Exec(ExecError::Empty));
        }
        let program = args.remove(0);
        commands.push(PlannedCommand {
            program,
            args,
            env: env.clone(),
            env_remove: env_remove.clone(),
            working_dir: working_dir.clone(),
        });
    }
    Ok(LaunchPlan {
        launch: Launch::Commands(commands),
        targets_ignored,
    })
}

/// The targets of each command, converted as the file field code needs,
/// and whether targets go unused (no file field code).
fn target_groups(
    line: &CommandLine,
    targets: &[Target],
) -> Result<(Vec<Vec<OsString>>, bool), LaunchError> {
    let Some(code) = line.file_code() else {
        return Ok((vec![Vec::new()], !targets.is_empty()));
    };
    let converted: Vec<OsString> = targets
        .iter()
        .map(|target| {
            if code.takes_urls() {
                to_uri(target).map(OsString::from)
            } else {
                to_local(target)
            }
        })
        .collect::<Result<_, _>>()?;
    if code.takes_several() || converted.len() <= 1 {
        Ok((vec![converted], false))
    } else {
        Ok((converted.into_iter().map(|one| vec![one]).collect(), false))
    }
}

/// The D-Bus name the entry's file name gives, if it is a valid one.
fn bus_name(entry: &DesktopEntry) -> Option<String> {
    let name = entry
        .path()?
        .file_name()?
        .to_str()?
        .strip_suffix(".desktop")?;
    crate::entry::valid_dbus_name(name).then(|| name.to_owned())
}

/// Bytes a URI path keeps as they are (RFC 3986 unreserved, and `/`).
fn keep_in_uri(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~' | b'/')
}

/// A target as a URI; a path becomes an absolute `file:` URI.
fn to_uri(target: &Target) -> Result<String, LaunchError> {
    match target {
        Target::Uri(uri) => {
            // A scheme (RFC 3986: a letter, then letters, digits, `+-.`) and
            // `:`, so a "URI" such as `--help` is not passed as an option.
            let scheme = uri.split_once(':').map(|(scheme, _)| scheme);
            let valid = scheme.is_some_and(|scheme| {
                scheme.starts_with(|c: char| c.is_ascii_alphabetic())
                    && scheme
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
            });
            if !valid || uri.contains('\0') {
                return Err(LaunchError::BadTarget(uri.clone()));
            }
            Ok(uri.clone())
        }
        Target::Path(path) => {
            let absolute = absolute(path)?;
            let mut uri = String::from("file://");
            for &byte in absolute.as_os_str().as_bytes() {
                if keep_in_uri(byte) {
                    uri.push(char::from(byte));
                } else {
                    let _ = write!(uri, "%{byte:02X}");
                }
            }
            Ok(uri)
        }
    }
}

/// A target as a local path; a `file:` URI is decoded, any other URI needs
/// a local copy.
fn to_local(target: &Target) -> Result<OsString, LaunchError> {
    match target {
        // Absolute, so that a `Path=` working folder does not change which
        // file a relative path names.
        Target::Path(path) => absolute(path).map(PathBuf::into_os_string),
        Target::Uri(uri) => {
            let Some(rest) = uri
                .get(..5)
                .filter(|scheme| scheme.eq_ignore_ascii_case("file:"))
                .map(|_| &uri[5..])
            else {
                return Err(LaunchError::NeedsLocalCopy(uri.clone()));
            };
            // `file:///p`, `file://localhost/p`, or `file:/p` (no authority).
            let path = match rest.strip_prefix("//") {
                Some(rest) => {
                    let (host, path) = rest.split_at(rest.find('/').unwrap_or(rest.len()));
                    if !(host.is_empty() || host.eq_ignore_ascii_case("localhost")) {
                        return Err(LaunchError::BadTarget(uri.clone()));
                    }
                    path
                }
                None => rest,
            };
            if !path.starts_with('/') {
                return Err(LaunchError::BadTarget(uri.clone()));
            }
            let path = path.split(['?', '#']).next().unwrap_or(path);
            percent_decode(path)
                .filter(|bytes| !bytes.contains(&0))
                .map(OsString::from_vec)
                .ok_or_else(|| LaunchError::BadTarget(uri.clone()))
        }
    }
}

/// `path` made absolute against the current folder; one with a NUL byte
/// cannot be passed to a program.
fn absolute(path: &Path) -> Result<PathBuf, LaunchError> {
    let bad = || LaunchError::BadTarget(path.display().to_string());
    if path.as_os_str().as_bytes().contains(&0) {
        return Err(bad());
    }
    std::path::absolute(path).map_err(|_| bad())
}

/// Decodes `%XX` escapes; `None` when one is not two hexadecimal digits.
fn percent_decode(text: &str) -> Option<Vec<u8>> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while at < bytes.len() {
        if bytes[at] == b'%' {
            let hex = bytes.get(at + 1..at + 3)?;
            if !hex.iter().all(u8::is_ascii_hexdigit) {
                return None;
            }
            out.push(u8::from_str_radix(std::str::from_utf8(hex).ok()?, 16).ok()?);
            at += 3;
        } else {
            out.push(bytes[at]);
            at += 1;
        }
    }
    Some(out)
}

impl LaunchPlan {
    /// Runs the plan. Programs are started in their own process group,
    /// and their [`Child`] handles are returned: wait on them (the library
    /// starts no threads to do it). A D-Bus call returns no children.
    ///
    /// # Errors
    ///
    /// When a program cannot be started ([`LaunchError::Spawn`] holds the
    /// handles of those started before it, which keep running) or the D-Bus
    /// call fails.
    pub fn spawn(&self) -> Result<Vec<Child>, LaunchError> {
        match &self.launch {
            Launch::Commands(commands) => {
                let mut started = Vec::with_capacity(commands.len());
                for planned in commands {
                    let mut command = Command::new(&planned.program);
                    command
                        .args(&planned.args)
                        .envs(planned.env.iter().map(|(k, v)| (k, v)))
                        .process_group(0);
                    for name in &planned.env_remove {
                        command.env_remove(name);
                    }
                    if let Some(dir) = &planned.working_dir {
                        command.current_dir(dir);
                    }
                    match command.spawn() {
                        Ok(child) => started.push(child),
                        Err(error) => {
                            return Err(LaunchError::Spawn {
                                program: planned.program.clone(),
                                error,
                                started,
                            });
                        }
                    }
                }
                Ok(started)
            }
            Launch::DBus(call) => call_dbus(call).map(|()| Vec::new()),
        }
    }
}

/// How long a D-Bus activation call may take.
#[cfg(feature = "dbus")]
const DBUS_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(25);

#[cfg(feature = "dbus")]
fn call_dbus(call: &DBusCall) -> Result<(), LaunchError> {
    use std::collections::HashMap;
    use zbus::zvariant::Value;

    let failed = |err: zbus::Error| LaunchError::DBus(err.to_string());
    // The reference D-Bus library's default; zbus has none, and a hung
    // application would block the caller for ever.
    let connection = zbus::blocking::connection::Builder::session()
        .and_then(|builder| builder.method_timeout(DBUS_TIMEOUT).build())
        .map_err(failed)?;
    let platform_data: HashMap<&str, Value<'_>> = call
        .platform_data
        .iter()
        .map(|(key, value)| (key.as_str(), Value::from(value.as_str())))
        .collect();
    let (bus, path, interface) = (
        call.bus_name.as_str(),
        call.object_path.as_str(),
        "org.freedesktop.Application",
    );
    match &call.method {
        DBusMethod::Activate => connection.call_method(
            Some(bus),
            path,
            Some(interface),
            "Activate",
            &(platform_data,),
        ),
        DBusMethod::Open(uris) => connection.call_method(
            Some(bus),
            path,
            Some(interface),
            "Open",
            &(uris, platform_data),
        ),
        DBusMethod::ActivateAction(name) => connection.call_method(
            Some(bus),
            path,
            Some(interface),
            "ActivateAction",
            &(name, Vec::<Value<'_>>::new(), platform_data),
        ),
    }
    .map(drop)
    .map_err(failed)
}

#[cfg(not(feature = "dbus"))]
fn call_dbus(_call: &DBusCall) -> Result<(), LaunchError> {
    Err(LaunchError::DBus(
        "this build has no D-Bus support (the `dbus` feature)".to_owned(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::KeyFile;

    fn entry(text: &str) -> DesktopEntry {
        DesktopEntry::parse(
            format!("[Desktop Entry]\nType=Application\nName=Foo Viewer\n{text}").as_bytes(),
        )
    }

    fn commands(plan: &LaunchPlan) -> Vec<Vec<String>> {
        let Launch::Commands(commands) = &plan.launch else {
            panic!("not commands: {plan:?}");
        };
        commands
            .iter()
            .map(|c| {
                std::iter::once(&c.program)
                    .chain(&c.args)
                    .map(|a| a.to_string_lossy().into_owned())
                    .collect()
            })
            .collect()
    }

    fn plan(text: &str, targets: &[Target]) -> Result<LaunchPlan, LaunchError> {
        prepare_launch(&entry(text), targets, &LaunchOptions::default())
    }

    fn path(p: &str) -> Target {
        Target::Path(p.into())
    }

    fn uri(u: &str) -> Target {
        Target::Uri(u.into())
    }

    #[test]
    fn one_file_codes_give_one_command_per_target() {
        let both = [path("/a"), path("/b c")];
        assert_eq!(
            commands(&plan("Exec=view %f", &both).unwrap()),
            [vec!["view", "/a"], vec!["view", "/b c"]]
        );
        assert_eq!(
            commands(&plan("Exec=view %F", &both).unwrap()),
            [vec!["view", "/a", "/b c"]]
        );
        assert_eq!(
            commands(&plan("Exec=view %f", &[]).unwrap()),
            [vec!["view"]]
        );
        let ignored = plan("Exec=view", &both).unwrap();
        assert_eq!(commands(&ignored), [vec!["view"]]);
        assert!(ignored.targets_ignored);
    }

    #[test]
    fn local_files_and_uris_are_converted_as_the_code_needs() {
        assert_eq!(
            commands(
                &plan(
                    "Exec=view %U",
                    &[path("/tmp/a b.txt"), uri("https://x.org/p")]
                )
                .unwrap()
            ),
            [vec!["view", "file:///tmp/a%20b.txt", "https://x.org/p"]]
        );
        assert_eq!(
            commands(&plan("Exec=view %f", &[uri("file:///tmp/caf%C3%A9.txt")]).unwrap()),
            [vec!["view", "/tmp/café.txt"]]
        );
        assert_eq!(
            commands(&plan("Exec=view %f", &[uri("file://localhost/x")]).unwrap()),
            [vec!["view", "/x"]]
        );
        assert_eq!(
            commands(&plan("Exec=view %f", &[uri("file:/no/authority")]).unwrap()),
            [vec!["view", "/no/authority"]]
        );
        let here = std::env::current_dir().unwrap().join("notes.txt");
        assert_eq!(
            commands(&plan("Exec=view %f\nPath=/srv", &[path("notes.txt")]).unwrap()),
            [vec!["view".to_owned(), here.display().to_string()]],
            "a relative path is the caller's, not Path's"
        );
        assert!(matches!(
            plan("Exec=view %F", &[uri("sftp://host/a")]),
            Err(LaunchError::NeedsLocalCopy(u)) if u == "sftp://host/a"
        ));
        assert!(matches!(
            plan("Exec=view %f", &[uri("file://otherhost/a")]),
            Err(LaunchError::BadTarget(_))
        ));
        for bad in ["--help", "no scheme", "x:\0"] {
            assert!(
                matches!(
                    plan("Exec=view %u", &[uri(bad)]),
                    Err(LaunchError::BadTarget(_))
                ),
                "{bad:?} as a URI"
            );
        }
        for bad in ["file:///a%+1b", "file:///a%00b", "file:///a%4"] {
            assert!(
                matches!(
                    plan("Exec=view %f", &[uri(bad)]),
                    Err(LaunchError::BadTarget(_))
                ),
                "{bad:?} as a local file"
            );
        }
        assert!(matches!(
            plan("Exec=view %f", &[path("/a\0b")]),
            Err(LaunchError::BadTarget(_))
        ));
    }

    #[test]
    fn name_icon_location_environment_and_folder() {
        let options = LaunchOptions {
            activation_token: Some("token-1".into()),
            startup_id: Some("id-1".into()),
            ..LaunchOptions::default()
        };
        let file = KeyFile::parse(
            b"[Desktop Entry]\nType=Application\nName=Foo\nIcon=foo\nPath=/srv\nExec=foo %i %c %k\n",
        );
        let found = prepare_launch(&DesktopEntry::from_key_file(file), &[], &options).unwrap();
        let Launch::Commands(commands) = &found.launch else {
            panic!()
        };
        let command = &commands[0];
        assert_eq!(
            command.args,
            ["--icon", "foo", "Foo"],
            "no location: %k dropped"
        );
        assert!(command.env_remove.is_empty());
        let plain = plan("Exec=foo", &[]).unwrap();
        let Launch::Commands(commands) = &plain.launch else {
            panic!()
        };
        assert_eq!(
            commands[0].env_remove,
            ["XDG_ACTIVATION_TOKEN", "DESKTOP_STARTUP_ID"],
            "the launcher's own tokens are not passed on"
        );
        assert_eq!(command.working_dir.as_deref(), Some(Path::new("/srv")));
        assert_eq!(
            command.env,
            [
                ("XDG_ACTIVATION_TOKEN".into(), "token-1".into()),
                ("DESKTOP_STARTUP_ID".into(), "id-1".into())
            ]
        );
    }

    #[test]
    fn the_location_is_absolute_when_the_file_was_read_by_a_relative_path() {
        // Tests run in the crate's folder.
        let mut file = KeyFile::read("tests/corpus/app-minimal.desktop").unwrap();
        file.set_string("Desktop Entry", "Exec", "foo %k").unwrap();
        file.set_string("Desktop Entry", "Path", "/srv").unwrap();
        let found = prepare_launch(
            &DesktopEntry::from_key_file(file),
            &[],
            &LaunchOptions::default(),
        )
        .unwrap();
        let Launch::Commands(commands) = &found.launch else {
            panic!()
        };
        assert_eq!(
            commands[0].args,
            [std::env::current_dir()
                .unwrap()
                .join("tests/corpus/app-minimal.desktop")
                .into_os_string()]
        );
    }

    #[test]
    fn terminals_actions_and_what_cannot_launch() {
        assert!(matches!(
            plan("Exec=top\nTerminal=true", &[]),
            Err(LaunchError::NeedsTerminal)
        ));
        let options = LaunchOptions {
            terminal: Some(vec!["foot".into(), "--".into()]),
            ..LaunchOptions::default()
        };
        let found = prepare_launch(&entry("Exec=top\nTerminal=true"), &[], &options).unwrap();
        assert_eq!(commands(&found), [vec!["foot", "--", "top"]]);
        let with_action =
            entry("Exec=foo\nActions=New;\n[Desktop Action New]\nName=New\nExec=foo --new\n");
        let mut options = LaunchOptions {
            action: Some("New".into()),
            ..LaunchOptions::default()
        };
        assert_eq!(
            commands(&prepare_launch(&with_action, &[], &options).unwrap()),
            [vec!["foo", "--new"]]
        );
        options.action = Some("Gone".into());
        assert!(matches!(
            prepare_launch(&with_action, &[], &options),
            Err(LaunchError::NoSuchAction(_))
        ));
        let link = DesktopEntry::parse(b"[Desktop Entry]\nType=Link\nName=x\nURL=https://x.org\n");
        assert!(matches!(
            prepare_launch(&link, &[], &LaunchOptions::default()),
            Err(LaunchError::Link(Some(u))) if u == "https://x.org"
        ));
        assert!(matches!(plan("", &[]), Err(LaunchError::NoExec)));
        assert!(matches!(
            plan("Exec=foo %z", &[]),
            Err(LaunchError::Exec(_))
        ));
    }

    #[cfg(feature = "dbus")]
    #[test]
    fn dbus_activatable_entries_become_calls() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("org.example.Foo-Bar.desktop");
        std::fs::write(
            &path,
            "[Desktop Entry]\nType=Application\nName=x\nExec=x %U\nDBusActivatable=true\nActions=New;\n[Desktop Action New]\nName=New\n",
        )
        .unwrap();
        let entry = DesktopEntry::read(&path).unwrap();
        let mut options = LaunchOptions {
            activation_token: Some("t".into()),
            ..LaunchOptions::default()
        };
        let call = |targets: &[Target], options: &LaunchOptions| match prepare_launch(
            &entry, targets, options,
        )
        .unwrap()
        .launch
        {
            Launch::DBus(call) => call,
            other @ Launch::Commands(_) => panic!("{other:?}"),
        };
        let open = call(&[path_target("/tmp/a")], &options);
        assert_eq!(open.bus_name, "org.example.Foo-Bar");
        assert_eq!(open.object_path, "/org/example/Foo_Bar");
        assert_eq!(open.method, DBusMethod::Open(vec!["file:///tmp/a".into()]));
        assert_eq!(
            open.platform_data,
            [("activation-token".to_owned(), "t".to_owned())]
        );
        assert_eq!(call(&[], &options).method, DBusMethod::Activate);
        options.action = Some("New".into());
        assert_eq!(
            call(&[], &options).method,
            DBusMethod::ActivateAction("New".into())
        );
        options.dbus = false;
        options.action = None;
        assert!(matches!(
            prepare_launch(&entry, &[], &options).unwrap().launch,
            Launch::Commands(_)
        ));
    }

    #[cfg(feature = "dbus")]
    fn path_target(p: &str) -> Target {
        Target::Path(p.into())
    }

    #[test]
    fn spawned_programs_get_their_arguments() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("out");
        let script = format!(
            "Exec=sh -c \"printf '%%s|' \\\"\\$@\\\" > {}\" sh %F",
            out.display()
        );
        let found = plan(&script, &[path("/a b"), path("/c")]).unwrap();
        for mut child in found.spawn().unwrap() {
            assert!(child.wait().unwrap().success());
        }
        assert_eq!(std::fs::read_to_string(&out).unwrap(), "/a b|/c|");
        let missing = plan("Exec=/nonexistent/program", &[]).unwrap();
        assert!(matches!(missing.spawn(), Err(LaunchError::Spawn { .. })));
        // A later program failing leaves the earlier ones to be waited on.
        // A script that exits at once (not `sh` itself, which would read
        // the inherited standard input).
        let script = dir.path().join("quick");
        std::fs::write(&script, "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&script, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .unwrap();
        let both = plan(
            "Exec=%f",
            &[Target::Path(script), path("/nonexistent/program")],
        )
        .unwrap();
        match both.spawn() {
            Err(LaunchError::Spawn {
                program, started, ..
            }) => {
                assert_eq!(program, "/nonexistent/program");
                assert_eq!(started.len(), 1);
                for mut child in started {
                    child.wait().unwrap();
                }
            }
            other => panic!("{other:?}"),
        }
    }
}
