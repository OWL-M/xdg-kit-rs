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

use crate::error::{quoted, quoted_message, shown_path};
use crate::exec::{CommandLine, ExecError, Values};
use crate::key_file::work;
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
    /// `["foot", "--"]`), needed for `Terminal=true`. An empty one counts
    /// as none.
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
    /// The program takes local files and this target is a URI with a
    /// scheme other than `file`: copy it to a local file and launch with
    /// that path.
    NeedsLocalCopy(String),
    /// `Terminal=true`, but no terminal command was given.
    NeedsTerminal,
    /// A path given as a target cannot be passed: it has a NUL byte, or
    /// cannot be made absolute.
    BadPath(PathBuf),
    /// A URI target cannot be passed. Every URI passed through `%u`, `%U`,
    /// `%f`, `%F` or D-Bus is first checked for a scheme and the absence of
    /// NUL bytes.
    ///
    /// Conversion to a local path (`%f`, `%F`) also requires a `file:` URI;
    /// other schemes return [`LaunchError::NeedsLocalCopy`]. Its host must
    /// be absent or `localhost`, and its path must be absolute. The path
    /// before any `?` or `#` must contain valid `%` escapes and no escaped
    /// NUL byte. Targets that are not passed (no file field code, or an
    /// action called over D-Bus) are not checked.
    BadUri(String),
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
                url.as_deref().map_or_else(|| "no URL".to_owned(), quoted)
            ),
            Self::NotAnApplication(Some(EntryType::Other(name))) => write!(
                f,
                "the entry is not an application (Some(Other(\"{}\")))",
                quoted(name)
            ),
            Self::NotAnApplication(kind) => write!(f, "the entry is not an application ({kind:?})"),
            Self::NoSuchAction(id) => write!(f, "the entry has no action \"{}\"", quoted(id)),
            Self::NoExec => write!(f, "the entry has no Exec to run"),
            Self::Exec(err) => write!(f, "Exec cannot be used: {err}"),
            Self::NeedsLocalCopy(uri) => write!(
                f,
                "{} is not a local file and the program takes only local files; copy it first",
                quoted(uri)
            ),
            Self::NeedsTerminal => write!(
                f,
                "the program runs in a terminal, but no terminal was given"
            ),
            Self::BadPath(path) => write!(f, "cannot pass {}", shown_path(path)),
            Self::BadUri(uri) => write!(f, "cannot pass {}", quoted(uri)),
            Self::Spawn { program, error, .. } => {
                write!(
                    f,
                    "cannot start {}: {error}",
                    shown_path(Path::new(program))
                )
            }
            Self::DBus(err) => write!(f, "D-Bus activation failed: {}", quoted_message(err)),
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
/// path, a URI with another scheme is [`LaunchError::NeedsLocalCopy`]);
/// `%u`/`%U` get URIs (paths become `file:` URIs).
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
    let name = entry.name(&options.locales);
    // Absolute: `Path=` changes the folder the program starts in.
    let location = entry
        .path()
        .map(|path| std::path::absolute(path).unwrap_or_else(|_| path.to_owned()));
    let location = location.as_deref().map(Path::as_os_str).unwrap_or_default();
    // Each byte of the absolute location written.
    work::count(location.len());
    // Read once: each read looks through the value's trailing spaces.
    let terminal = if entry.terminal() {
        Some(
            options
                .terminal
                .as_ref()
                .filter(|terminal| !terminal.is_empty())
                .ok_or(LaunchError::NeedsTerminal)?,
        )
    } else {
        None
    };
    let mut env: Vec<(OsString, OsString)> = Vec::new();
    let mut env_remove: Vec<OsString> = Vec::new();
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
    let copied = env
        .iter()
        .map(|(name, value)| name.len() + value.len())
        .chain(env_remove.iter().map(|part| part.len()))
        .chain(working_dir.iter().map(|dir| dir.as_os_str().len()))
        .sum::<usize>();
    // Everything but the file codes is expanded once, not for each
    // command.
    let prepared = line.prepare(Values {
        icon: icon.as_deref(),
        name: name.as_deref(),
        location,
    });
    let mut commands = Vec::with_capacity(groups.len());
    for files in &groups {
        let mut args = prepared.expand(files);
        // The application's own command, before a terminal is put in front
        // of it, starts with its program as written: parsing refuses an
        // empty program and a field code in it, and only later arguments
        // can vanish. So a terminal is never run with no program.
        debug_assert!(
            args.first().is_some_and(|program| !program.is_empty()),
            "parsing refuses an empty program and field codes in it"
        );
        if let Some(terminal) = terminal {
            // Each byte of the terminal command copied, and each argument
            // moved after it.
            work::count(terminal.iter().map(|part| part.len()).sum::<usize>() + args.len());
            args.splice(0..0, terminal.iter().cloned());
        }
        // Each argument moved when the program is taken off, and each byte
        // of the environment and the folder copied into the command.
        work::count(args.len() + copied);
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
            work::count(1);
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
        work::count(converted.len());
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
            check_uri(uri)?;
            // Each byte copied.
            work::count(uri.len());
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
            // Each byte of the path looked at, and of the URI written.
            work::count(absolute.as_os_str().len() + uri.len());
            Ok(uri)
        }
    }
}

/// Checks that `uri` is one: a scheme (RFC 3986: a letter, then letters,
/// digits, `+-.`) and `:`, so that a "URI" such as `--help` is not passed
/// as an option, and no NUL byte.
fn check_uri(uri: &str) -> Result<(), LaunchError> {
    let scheme = uri.split_once(':').map(|(scheme, _)| scheme);
    // The scheme looked through for `:` and checked, and the whole URI for a
    // NUL byte.
    work::count_with(|| 2 * scheme.map_or(uri.len(), str::len) + uri.len());
    let valid = scheme.is_some_and(|scheme| {
        scheme.starts_with(|c: char| c.is_ascii_alphabetic())
            && scheme
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
    });
    if !valid || uri.contains('\0') {
        return Err(LaunchError::BadUri(uri.to_owned()));
    }
    Ok(())
}

/// A target as a local path; a `file:` URI is decoded, a URI with another
/// scheme needs a local copy, and one that is not a URI is refused.
fn to_local(target: &Target) -> Result<OsString, LaunchError> {
    match target {
        // Absolute, so that a `Path=` working folder does not change which
        // file a relative path names.
        Target::Path(path) => absolute(path).map(PathBuf::into_os_string),
        Target::Uri(uri) => {
            check_uri(uri)?;
            let Some(rest) = uri
                .get(..5)
                .filter(|scheme| scheme.eq_ignore_ascii_case("file:"))
                .map(|_| &uri[5..])
            else {
                return Err(LaunchError::NeedsLocalCopy(uri.clone()));
            };
            // The rest looked through for the end of the host and for `?` or
            // `#`.
            work::count(2 * rest.len());
            // `file:///p`, `file://localhost/p`, or `file:/p` (no authority).
            let path = match rest.strip_prefix("//") {
                Some(rest) => {
                    let (host, path) = rest.split_at(rest.find('/').unwrap_or(rest.len()));
                    if !(host.is_empty() || host.eq_ignore_ascii_case("localhost")) {
                        return Err(LaunchError::BadUri(uri.clone()));
                    }
                    path
                }
                None => rest,
            };
            if !path.starts_with('/') {
                return Err(LaunchError::BadUri(uri.clone()));
            }
            let path = path.split(['?', '#']).next().unwrap_or(path);
            percent_decode(path)
                .filter(|bytes| !bytes.contains(&0))
                .map(OsString::from_vec)
                .ok_or_else(|| LaunchError::BadUri(uri.clone()))
        }
    }
}

/// `path` made absolute against the current folder; one with a NUL byte
/// cannot be passed to a program.
fn absolute(path: &Path) -> Result<PathBuf, LaunchError> {
    let bad = || LaunchError::BadPath(path.to_owned());
    // Each byte looked at for a NUL.
    work::count(path.as_os_str().len());
    if path.as_os_str().as_bytes().contains(&0) {
        return Err(bad());
    }
    let absolute = std::path::absolute(path).map_err(|_| bad())?;
    // Each byte of the absolute path written.
    work::count(absolute.as_os_str().len());
    Ok(absolute)
}

/// Decodes `%XX` escapes; `None` when one is not two hexadecimal digits.
fn percent_decode(text: &str) -> Option<Vec<u8>> {
    let bytes = text.as_bytes();
    // Each byte looked at and written.
    work::count(2 * bytes.len());
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
    /// call fails, which it also does when the bus and the application have
    /// not answered within 25 seconds from the start of connecting. A bus
    /// whose queue of waiting connections stays full can leave a thread of
    /// the `blocking` crate's pool (zbus on async-io, as this crate builds
    /// it) in `connect` for each such launch, as
    /// many as the pool's limit on threads.
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

/// How long a D-Bus activation may take, from connecting to the bus to the
/// application's answer: the reference D-Bus library's default for a call.
/// A bus or application that stops answering at any step would otherwise
/// block the caller for ever.
#[cfg(feature = "dbus")]
const DBUS_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(25);

#[cfg(feature = "dbus")]
fn call_dbus(call: &DBusCall) -> Result<(), LaunchError> {
    let address = zbus::Address::session().map_err(|err| LaunchError::DBus(err.to_string()))?;
    call_bus_at(call, address, DBUS_TIMEOUT)
}

/// Makes `call` on the bus at `address`. Connection, authentication, `Hello`
/// and the call run as one task with deadline `limit`. At the deadline,
/// dropping the task closes the connection.
///
/// zbus's own `method_timeout` bounds only the call, not the steps before
/// it, and its socket is non-blocking, so a socket timeout bounds nothing.
/// With async-io, as this crate builds zbus, zbus runs the blocking
/// `connect` to a Unix socket on a thread of the `blocking` crate's pool.
/// On a bus whose queue of waiting connections stays full, that `connect`
/// blocks: the call still fails at the deadline, but the thread stays in
/// `connect` until it returns, so each such launch can leave one thread
/// behind, as many as the pool's limit on threads; once it is reached,
/// later calls wait for a thread and still fail at the deadline.
#[cfg(feature = "dbus")]
fn call_bus_at(
    call: &DBusCall,
    address: zbus::Address,
    limit: std::time::Duration,
) -> Result<(), LaunchError> {
    use std::collections::HashMap;
    use zbus::zvariant::Value;

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
    let connect_and_call = async {
        let connection = zbus::connection::Builder::address(address)?.build().await?;
        match &call.method {
            DBusMethod::Activate => {
                connection
                    .call_method(
                        Some(bus),
                        path,
                        Some(interface),
                        "Activate",
                        &(&platform_data,),
                    )
                    .await
            }
            DBusMethod::Open(uris) => {
                connection
                    .call_method(
                        Some(bus),
                        path,
                        Some(interface),
                        "Open",
                        &(uris, &platform_data),
                    )
                    .await
            }
            DBusMethod::ActivateAction(name) => {
                connection
                    .call_method(
                        Some(bus),
                        path,
                        Some(interface),
                        "ActivateAction",
                        &(name, Vec::<Value<'_>>::new(), &platform_data),
                    )
                    .await
            }
        }
        .map(drop)
    };
    let work = async {
        connect_and_call
            .await
            .map_err(|err: zbus::Error| LaunchError::DBus(err.to_string()))
    };
    // Made before the task is first polled, so that the deadline counts
    // from before connecting.
    let timer = async_io::Timer::after(limit);
    let deadline = async {
        timer.await;
        Err(LaunchError::DBus(format!(
            "the bus or the application did not answer within {limit:?}"
        )))
    };
    async_io::block_on(futures_lite::future::or(work, deadline))
}

#[cfg(not(feature = "dbus"))]
fn call_dbus(_call: &DBusCall) -> Result<(), LaunchError> {
    Err(LaunchError::DBus(
        "this build has no D-Bus support (the `dbus` feature)".to_owned(),
    ))
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;

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
    fn text_from_the_entry_is_shown_escaped_and_cut() {
        let bad = format!("\x1b\\n{}", "a".repeat(5000));
        let errors = [
            prepare_launch(
                &DesktopEntry::parse(
                    format!("[Desktop Entry]\nType=Link\nName=x\nURL={bad}\n").as_bytes(),
                ),
                &[],
                &LaunchOptions::default(),
            )
            .unwrap_err(),
            prepare_launch(
                &DesktopEntry::parse(format!("[Desktop Entry]\nType={bad}\nName=x\n").as_bytes()),
                &[],
                &LaunchOptions::default(),
            )
            .unwrap_err(),
            prepare_launch(
                &entry("Exec=a"),
                &[],
                &LaunchOptions {
                    action: Some(bad.clone()),
                    ..LaunchOptions::default()
                },
            )
            .unwrap_err(),
            // A link's URL, given as a target.
            plan("Exec=a %u", &[uri(&bad)]).unwrap_err(),
            plan("Exec=a %f", &[uri(&format!("sftp://host/{bad}"))]).unwrap_err(),
        ];
        for error in errors {
            crate::error::assert_shown_safely(&error.to_string());
        }
    }

    #[test]
    fn a_path_is_shown_escaped_and_whole() {
        let long = format!("/nonexistent/{}\x1b", "d/".repeat(100));
        let shown = format!("/nonexistent/{}\\u{{1b}}", "d/".repeat(100));
        // A path given as a target.
        let bad = plan("Exec=a %f", &[path(&format!("{long}\"'\0"))]).unwrap_err();
        assert_eq!(bad.to_string(), format!("cannot pass {shown}\"'\\0"));
        // The program, from `Exec`.
        let missing = plan(&format!("Exec={long}"), &[])
            .unwrap()
            .spawn()
            .unwrap_err()
            .to_string();
        assert!(
            missing.starts_with(&format!("cannot start {shown}: ")),
            "{missing}"
        );
    }

    #[test]
    fn an_error_from_d_bus_is_shown_escaped_and_cut_at_256_characters() {
        let error = LaunchError::DBus(format!("a\x1b[2J\n{}", "b".repeat(5000)));
        assert_eq!(
            error.to_string(),
            format!(
                "D-Bus activation failed: a\\u{{1b}}[2J\\n{}…",
                "b".repeat(250)
            )
        );
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
            Err(LaunchError::BadUri(_))
        ));
        for bad in ["--help", "no scheme", "x:\0"] {
            assert!(
                matches!(
                    plan("Exec=view %u", &[uri(bad)]),
                    Err(LaunchError::BadUri(_))
                ),
                "{bad:?} as a URI"
            );
        }
        for bad in ["file:///a%+1b", "file:///a%00b", "file:///a%4"] {
            assert!(
                matches!(
                    plan("Exec=view %f", &[uri(bad)]),
                    Err(LaunchError::BadUri(_))
                ),
                "{bad:?} as a local file"
            );
        }
        assert!(matches!(
            plan("Exec=view %f", &[path("/a\0b")]),
            Err(LaunchError::BadPath(_))
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
        assert_eq!(command.env_remove, Vec::<OsString>::new());
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
        assert!(matches!(
            plan(r#"Exec=foo "%z""#, &[]),
            Err(LaunchError::Exec(ExecError::UnknownFieldCode('z')))
        ));
        assert!(matches!(
            plan(r#"Exec=foo "\\%z""#, &[]),
            Err(LaunchError::Exec(ExecError::UnknownFieldCode('z')))
        ));
        assert_eq!(
            commands(&plan(r#"Exec=foo "%%z" "a%db""#, &[]).unwrap()),
            [vec!["foo", "%z", "ab"]]
        );
    }

    #[test]
    fn an_empty_terminal_command_is_no_terminal() {
        let options = LaunchOptions {
            terminal: Some(Vec::new()),
            ..LaunchOptions::default()
        };
        assert!(matches!(
            prepare_launch(&entry("Exec=top\nTerminal=true"), &[], &options),
            Err(LaunchError::NeedsTerminal)
        ));
    }

    #[test]
    fn an_empty_program_cannot_launch() {
        for exec in [r#"Exec="""#, r#"Exec="" foo"#] {
            assert!(
                matches!(plan(exec, &[]), Err(LaunchError::Exec(ExecError::Empty))),
                "{exec}"
            );
        }
        assert_eq!(
            commands(&plan(r#"Exec=foo """#, &[]).unwrap()),
            [vec!["foo", ""]]
        );
    }

    #[test]
    fn a_name_code_vanishes_only_when_there_is_no_name() {
        let found = |keys: &str, locales: &[&str]| {
            let entry = DesktopEntry::parse(
                format!("[Desktop Entry]\nType=Application\n{keys}Exec=app %c x %u\n").as_bytes(),
            );
            let options = LaunchOptions {
                locales: Locales::new(locales),
                ..LaunchOptions::default()
            };
            commands(&prepare_launch(&entry, &[path("/t/a")], &options).unwrap())
        };
        // As `GLib` gives (its name is the translated lookup's result).
        assert_eq!(found("", &[]), [vec!["app", "x", "file:///t/a"]], "no Name");
        assert_eq!(
            found("Name[de]=Foo\n", &[]),
            [vec!["app", "x", "file:///t/a"]],
            "no Name for these languages"
        );
        assert_eq!(
            found("Name[de]=Foo\n", &["de_DE"]),
            [vec!["app", "Foo", "x", "file:///t/a"]]
        );
        assert_eq!(
            found("Name=\n", &[]),
            [vec!["app", "", "x", "file:///t/a"]],
            "an empty Name"
        );
        assert_eq!(
            found("Name=\nName[de]=Foo\n", &["fr"]),
            [vec!["app", "", "x", "file:///t/a"]]
        );
    }

    #[test]
    fn a_target_that_is_not_a_uri_is_a_bad_uri_for_local_files_too() {
        for code in ["%f", "%F", "%u", "%U"] {
            for bad in [
                "--help",
                "",
                ":x",
                "1a:b",
                "f\u{ef}le:///a",
                "x:\0",
                "file:///a?\0",
            ] {
                let found = plan(&format!("Exec=view {code}"), &[uri(bad)]);
                assert!(
                    matches!(&found, Err(LaunchError::BadUri(u)) if u == bad),
                    "{bad:?} for {code}: {found:?}"
                );
            }
        }
        for code in ["%f", "%F"] {
            assert!(matches!(
                plan(&format!("Exec=view {code}"), &[uri("x-y+z.w:q")]),
                Err(LaunchError::NeedsLocalCopy(u)) if u == "x-y+z.w:q"
            ));
        }
    }

    #[test]
    fn a_field_code_as_the_program_cannot_launch() {
        let terminal = LaunchOptions {
            terminal: Some(vec!["foot".into(), "--".into()]),
            ..LaunchOptions::default()
        };
        let mut launched = Vec::new();
        let targets = [path("/usr/bin/true")];
        for (name, exec) in [
            ("Foo", "%f"),
            ("Foo", "%U"),
            ("Foo", "%k%k"),
            ("Foo", "%i"),
            ("", "%c"),
            ("", "%c%k"),
            ("true", "%c"),
            ("Foo", "%f --x"),
            ("Foo", "%k --x"),
        ] {
            for terminal_key in ["", "Terminal=true\n"] {
                for options in [&LaunchOptions::default(), &terminal] {
                    let entry = DesktopEntry::parse(
                        format!(
                            "[Desktop Entry]\nType=Application\nName={name}\nExec={exec}\n{terminal_key}"
                        )
                        .as_bytes(),
                    );
                    let found = prepare_launch(&entry, &targets, options);
                    if !matches!(found, Err(LaunchError::Exec(ExecError::FieldCodeInProgram))) {
                        launched.push(format!(
                            "Exec={exec} Name={name:?} {terminal_key:?} with terminal {:?}: {:?}",
                            options.terminal,
                            found.map(|plan| commands(&plan))
                        ));
                    }
                }
            }
        }
        assert!(launched.is_empty(), "{launched:#?}");
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
    fn preparing_a_launch_is_linear_in_its_targets() {
        // A `Terminal` value with n spaces after it; n arguments that
        // vanish (no `Icon`); and n codes that expand to nothing (no `Icon`,
        // no location) in the argument of `%f`: each with n targets, one
        // command per target. What follows `%f` and the keys after `Exec`,
        // for n.
        type Shape = fn(usize) -> (String, String);
        let shapes: [(&str, Shape); 4] = [
            ("spaces after Terminal", |n| {
                (String::new(), format!("Terminal=false{}\n", " ".repeat(n)))
            }),
            ("vanishing arguments", |n| (" %i".repeat(n), String::new())),
            ("icons beside the file", |n| ("%i".repeat(n), String::new())),
            ("locations beside the file", |n| {
                ("%k".repeat(n), String::new())
            }),
        ];
        for (what, extra) in shapes {
            work::assert_linear(
                1_000,
                |n| {
                    let (args, keys) = extra(n);
                    let text = format!(
                        "[Desktop Entry]\nType=Application\nName=a\nExec=app %f{args}\n{keys}"
                    );
                    let entry = DesktopEntry::parse(text.as_bytes());
                    let targets: Vec<Target> = (0..n).map(|at| path(&format!("/t/{at}"))).collect();
                    let bytes =
                        text.len() + (0..n).map(|at| format!("/t/{at}").len()).sum::<usize>();
                    let (found, steps) =
                        work::steps(|| prepare_launch(&entry, &targets, &LaunchOptions::default()));
                    let found = commands(&found.unwrap());
                    assert_eq!(found.len(), n, "{what}");
                    assert_eq!(found[n - 1], ["app".to_owned(), format!("/t/{}", n - 1)]);
                    (steps, bytes)
                },
                // A step per target converted and grouped and per argument
                // of its command, and each byte of the two environment names
                // the command drops copied into it: about 50 per target; per
                // byte of the entry's values looked up, parsed and expanded,
                // and of each target's path checked, made absolute and
                // copied: about 5 per byte. Twice that.
                |targets, bytes| 100 * targets + 10 * bytes,
            );
        }
    }

    #[test]
    fn each_command_gets_what_expanding_each_argument_gives() {
        let terminal = LaunchOptions {
            terminal: Some(vec!["foot".into(), "--".into()]),
            ..LaunchOptions::default()
        };
        let target_sets = [
            Vec::new(),
            vec![path("/a")],
            vec![path("/a b"), uri("file:///c%20d"), path("/e")],
        ];
        for exec in crate::exec::TEST_LINES {
            for keys in ["Icon=foo\n", "Path=/srv\nTerminal=true\n", "Name[de]=Foo\n"] {
                let entry = entry(&format!(
                    "Exec={}\n{keys}",
                    crate::key_file::escape(exec, false)
                ));
                let line = CommandLine::parse(&entry.exec().unwrap()).unwrap();
                let (icon, name) = (entry.icon(&terminal.locales), entry.name(&terminal.locales));
                let values = Values {
                    icon: icon.as_deref(),
                    name: name.as_deref(),
                    location: OsStr::new(""),
                };
                for targets in &target_sets {
                    let (groups, _) = target_groups(&line, targets).unwrap();
                    let expected: Vec<Vec<String>> = groups
                        .iter()
                        .map(|files| {
                            let args = line.expand_each_argument(&values, files);
                            let front: &[&str] = if entry.terminal() {
                                &["foot", "--"]
                            } else {
                                &[]
                            };
                            front
                                .iter()
                                .map(|&part| part.to_owned())
                                .chain(args.iter().map(|arg| arg.to_string_lossy().into_owned()))
                                .collect()
                        })
                        .collect();
                    let found = prepare_launch(&entry, targets, &terminal).unwrap();
                    assert_eq!(commands(&found), expected, "{exec} {keys:?} {targets:?}");
                }
            }
        }
    }

    #[test]
    fn spawned_programs_get_their_arguments() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("out");
        // The temporary folder's path, which may hold anything, is quoted
        // for the shell (`'` as `'\''`), then as a quoted argument (`"`,
        // `` ` ``, `$` and `\` after a `\`, `%` doubled), then escaped as
        // the key file's string.
        let shell = format!("'{}'", out.to_str().unwrap().replace('\'', r"'\''"));
        let mut argument = String::new();
        for c in shell.chars() {
            match c {
                '"' | '`' | '$' | '\\' => argument.extend(['\\', c]),
                '%' => argument.push_str("%%"),
                _ => argument.push(c),
            }
        }
        let script = format!(
            "Exec={}",
            crate::key_file::escape(
                &format!("sh -c \"printf '%%s|' \\\"\\$@\\\" > {argument}\" sh %F"),
                false
            )
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
        let command = |program: &Path| PlannedCommand {
            program: program.into(),
            args: Vec::new(),
            env: Vec::new(),
            env_remove: Vec::new(),
            working_dir: None,
        };
        let both = LaunchPlan {
            launch: Launch::Commands(vec![
                command(&script),
                command(Path::new("/nonexistent/program")),
            ]),
            targets_ignored: false,
        };
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

// The fake bus uses a Linux abstract socket because a temporary folder's
// path may be too long for a socket address.
#[cfg(all(test, feature = "dbus", target_os = "linux"))]
mod deadline_tests {
    use std::io::{Read as _, Write as _};
    use std::os::linux::net::SocketAddrExt as _;
    use std::os::unix::net::{SocketAddr, UnixListener};
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    use super::*;

    /// The deadline the tests give the call: short, so that they do not
    /// wait the 25 s a launch gets.
    const LIMIT: Duration = Duration::from_millis(300);

    /// How much later than the deadline the call may return, and the bus
    /// may see the connection closed, on a slow machine.
    const SLACK: Duration = Duration::from_secs(10);

    /// The steps of the start of a connection that the fake bus answers,
    /// in order; it answers the first `answered`, then nothing more.
    const STEPS: [&str; 3] = ["authentication", "file descriptor passing", "Hello"];

    /// A bus that answers the first `answered` of [`STEPS`] and then stays
    /// silent. The receiver gets the moment the client closed the
    /// connection, and all the client sent.
    fn fake_bus(answered: usize) -> (zbus::Address, mpsc::Receiver<(Instant, Vec<u8>)>) {
        // Unique to this run and bus: the process, the time and `answered`.
        let name = format!(
            "xdg-kit-test-{}-{}-{answered}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let listener =
            UnixListener::bind_addr(&SocketAddr::from_abstract_name(&name).unwrap()).unwrap();
        let address = format!("unix:abstract={name}").parse().unwrap();
        let (closed, closed_at) = mpsc::channel();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut read = Vec::new();
            let mut sent = 0;
            let mut buffer = [0; 4096];
            loop {
                let n = match stream.read(&mut buffer) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => n,
                };
                read.extend_from_slice(&buffer[..n]);
                while sent < answered {
                    let Some(answer) = answer(sent, &read) else {
                        break;
                    };
                    stream.write_all(&answer).unwrap();
                    sent += 1;
                }
            }
            let _ = closed.send((Instant::now(), read));
        });
        (address, closed_at)
    }

    /// The answer to step `step` of [`STEPS`], once what the client sent
    /// so far, `read`, holds its request.
    fn answer(step: usize, read: &[u8]) -> Option<Vec<u8>> {
        let has = |line: &[u8]| read.windows(line.len()).any(|window| window == line);
        match step {
            0 => (has(b"AUTH EXTERNAL") && has(b"\r\n"))
                .then(|| b"OK 0123456789abcdef0123456789abcdef\r\n".to_vec()),
            1 => has(b"NEGOTIATE_UNIX_FD\r\n").then(|| b"AGREE_UNIX_FD\r\n".to_vec()),
            _ => {
                let at = read.windows(7).position(|window| window == b"BEGIN\r\n")? + 7;
                let header = read.get(at..at + 12)?;
                let serial: [u8; 4] = header[8..12].try_into().unwrap();
                let serial = if header[0] == b'l' {
                    u32::from_le_bytes(serial)
                } else {
                    u32::from_be_bytes(serial)
                };
                Some(hello_reply(serial))
            }
        }
    }

    /// The bus's reply to `Hello` (a little-endian method return to
    /// `serial` from `org.freedesktop.DBus`, with the unique name `:1.1`).
    fn hello_reply(serial: u32) -> Vec<u8> {
        fn pad(message: &mut Vec<u8>, to: usize) {
            while !message.len().is_multiple_of(to) {
                message.push(0);
            }
        }
        fn string(message: &mut Vec<u8>, text: &str) {
            pad(message, 4);
            message.extend_from_slice(&u32::try_from(text.len()).unwrap().to_le_bytes());
            message.extend_from_slice(text.as_bytes());
            message.push(0);
        }
        let body = ":1.1";
        // Endianness, method return, no flags, version 1; the body's
        // length; this message's serial.
        let mut message = vec![b'l', 2, 0, 1];
        message.extend_from_slice(&u32::try_from(4 + body.len() + 1).unwrap().to_le_bytes());
        message.extend_from_slice(&1_u32.to_le_bytes());
        // The header fields' length, filled in below.
        message.extend_from_slice(&[0; 4]);
        let fields = message.len();
        // REPLY_SERIAL (5), a `u`.
        pad(&mut message, 8);
        message.extend_from_slice(&[5, 1, b'u', 0]);
        message.extend_from_slice(&serial.to_le_bytes());
        // SENDER (7), an `s`.
        pad(&mut message, 8);
        message.extend_from_slice(&[7, 1, b's', 0]);
        string(&mut message, "org.freedesktop.DBus");
        // SIGNATURE (8), a `g`: the body is one string.
        pad(&mut message, 8);
        message.extend_from_slice(&[8, 1, b'g', 0, 1, b's', 0]);
        let length = u32::try_from(message.len() - fields).unwrap();
        message[fields - 4..fields].copy_from_slice(&length.to_le_bytes());
        pad(&mut message, 8);
        string(&mut message, body);
        message
    }

    /// Calls `Activate` on a fake bus that answers the first `answered` of
    /// [`STEPS`], and checks that the call fails at the deadline and that
    /// the bus sees the connection closed.
    fn assert_given_up_at_the_deadline(answered: usize) {
        let (address, closed_at) = fake_bus(answered);
        let call = DBusCall {
            bus_name: "org.example.Foo".to_owned(),
            object_path: "/org/example/Foo".to_owned(),
            method: DBusMethod::Activate,
            platform_data: Vec::new(),
        };
        let started = Instant::now();
        let found = call_bus_at(&call, address, LIMIT);
        let took = started.elapsed();
        let stage = STEPS.get(answered).map_or("the call", |step| *step);
        assert!(
            matches!(&found, Err(LaunchError::DBus(message)) if message.contains("did not answer")),
            "stopped at {stage}: {found:?}"
        );
        assert!(
            (LIMIT..LIMIT + SLACK).contains(&took),
            "stopped at {stage}: returned after {took:?}"
        );
        let (closed, sent) = closed_at
            .recv_timeout(SLACK)
            .unwrap_or_else(|_| panic!("stopped at {stage}: the connection is still open"));
        assert!(closed - started >= LIMIT, "stopped at {stage}");
        let called = sent.windows(8).any(|window| window == b"Activate");
        assert_eq!(
            called,
            answered == STEPS.len(),
            "stopped at {stage}: the call was sent"
        );
    }

    #[test]
    fn a_bus_that_never_answers_fails_the_call_at_the_deadline() {
        assert_given_up_at_the_deadline(0);
    }

    #[test]
    fn a_bus_that_stops_after_authentication_fails_the_call_at_the_deadline() {
        assert_given_up_at_the_deadline(1);
    }

    #[test]
    fn a_bus_that_stops_before_hello_fails_the_call_at_the_deadline() {
        assert_given_up_at_the_deadline(2);
    }

    #[test]
    fn an_application_that_never_answers_fails_the_call_at_the_deadline() {
        assert_given_up_at_the_deadline(3);
    }
}
