//! The keys the Desktop Entry spec defines ("Recognized desktop entry
//! keys", "Action Specific Keys", "Historically Reserved Items",
//! "Deprecated Items"), with their value types.

/// A value type of the spec ("Possible value types"); no key it defines is
/// `numeric`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ValueType {
    String,
    LocaleString,
    IconString,
    Boolean,
    StringList,
    LocaleStringList,
}

impl ValueType {
    /// Whether keys of this type may have `[LOCALE]` variants.
    pub(crate) fn localized(self) -> bool {
        matches!(
            self,
            Self::LocaleString | Self::IconString | Self::LocaleStringList
        )
    }
}

/// Which entry types a key belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Applies {
    Any,
    Application,
    Link,
}

/// What the spec says about a key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Status {
    Current,
    /// Listed in "Historically Reserved Items" (KDE, GNOME).
    Reserved,
    /// Listed in "Deprecated Items".
    Deprecated,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct KeyDef {
    pub(crate) name: &'static str,
    pub(crate) value_type: ValueType,
    pub(crate) applies: Applies,
    pub(crate) status: Status,
}

const fn key(name: &'static str, value_type: ValueType, applies: Applies) -> KeyDef {
    KeyDef {
        name,
        value_type,
        applies,
        status: Status::Current,
    }
}

const fn reserved(name: &'static str, value_type: ValueType) -> KeyDef {
    KeyDef {
        name,
        value_type,
        applies: Applies::Any,
        status: Status::Reserved,
    }
}

const fn deprecated(name: &'static str, value_type: ValueType) -> KeyDef {
    KeyDef {
        name,
        value_type,
        applies: Applies::Any,
        status: Status::Deprecated,
    }
}

use Applies::{Any, Application, Link};
use ValueType::{Boolean, IconString, LocaleString, LocaleStringList, String as Str, StringList};

/// The keys of the `[Desktop Entry]` group.
pub(crate) const DESKTOP_KEYS: &[KeyDef] = &[
    key("Type", Str, Any),
    key("Version", Str, Any),
    key("Name", LocaleString, Any),
    key("GenericName", LocaleString, Any),
    key("NoDisplay", Boolean, Any),
    key("Comment", LocaleString, Any),
    key("Icon", IconString, Any),
    key("Hidden", Boolean, Any),
    key("OnlyShowIn", StringList, Any),
    key("NotShowIn", StringList, Any),
    key("DBusActivatable", Boolean, Any),
    key("TryExec", Str, Application),
    key("Exec", Str, Application),
    key("Path", Str, Application),
    key("Terminal", Boolean, Application),
    key("Actions", StringList, Application),
    key("MimeType", StringList, Application),
    key("Categories", StringList, Application),
    key("Implements", StringList, Any),
    key("Keywords", LocaleStringList, Application),
    key("StartupNotify", Boolean, Application),
    key("StartupWMClass", Str, Application),
    key("URL", Str, Link),
    key("PrefersNonDefaultGPU", Boolean, Application),
    key("SingleMainWindow", Boolean, Application),
    // Historically reserved: KDE.
    reserved("ServiceTypes", Str),
    reserved("DocPath", Str),
    reserved("InitialPreference", Str),
    // Historically reserved: GNOME (the Autostart spec's condition key).
    reserved("AutostartCondition", Str),
    // Historically reserved: KDE's FSDevice type.
    reserved("Dev", Str),
    reserved("FSType", Str),
    reserved("MountPoint", Str),
    reserved("ReadOnly", Boolean),
    reserved("UnmountIcon", IconString),
    // Deprecated.
    deprecated("Protocols", StringList),
    deprecated("Extensions", StringList),
    deprecated("BinaryPattern", StringList),
    deprecated("MapNotify", Str),
    deprecated("Patterns", StringList),
    deprecated("DefaultApp", Str),
    deprecated("MiniIcon", IconString),
    deprecated("TerminalOptions", Str),
    deprecated("Encoding", Str),
    deprecated("SwallowTitle", LocaleString),
    deprecated("SwallowExec", Str),
    deprecated("SortOrder", StringList),
    deprecated("FilePattern", StringList),
];

/// The keys of a `[Desktop Action <id>]` group.
pub(crate) const ACTION_KEYS: &[KeyDef] = &[
    key("Name", LocaleString, Any),
    key("Icon", IconString, Any),
    key("Exec", Str, Any),
];

/// The definition of `name` in `keys`.
pub(crate) fn find(keys: &[KeyDef], name: &str) -> Option<KeyDef> {
    keys.iter().copied().find(|def| def.name == name)
}
