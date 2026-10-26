# xdg-kit-rs

Freedesktop.org XDG specifications for Rust: MIME types, default
applications, desktop entries, base directories, the user's languages,
recently-used bookmarks, thumbnails, icon themes and login autostart.
Pure Rust, no `unsafe` code.

## Crates

| Crate | What it does |
| --- | --- |
| [`xdg-kit`](https://docs.rs/xdg-kit) | Re-exports the crates below, one Cargo feature each. |
| [`xdg-kit-basedir`](https://docs.rs/xdg-kit-basedir) | The XDG Base Directory spec (version 0.8) and the XDG user folders (`user-dirs.dirs`). |
| [`xdg-kit-locale`](https://docs.rs/xdg-kit-locale) | Reads the user's languages from the environment as gettext does. Supplies translation lookup keys in the order defined by the Desktop Entry spec. |
| [`xdg-kit-desktop-entry`](https://docs.rs/xdg-kit-desktop-entry) | Implements the Desktop Entry spec (version 1.5, with the latest text of xdg-specs). Reads and writes `.desktop` and `.directory` files without losing contents or formatting. Supports typed keys, translations, actions, validation, lookup by desktop file ID, and application launching. |
| [`xdg-kit-mime`](https://docs.rs/xdg-kit-mime) | The Shared MIME-info database (spec version 0.22, whose text still calls itself 0.21): file types from names, content and file kinds, their relationships, icons and descriptions, and content types of mounted volumes. |
| [`xdg-kit-mime-apps`](https://docs.rs/xdg-kit-mime-apps) | The MIME Applications Associations spec (version 1.0.1): applications and defaults for a MIME type from every `mimeapps.list` and the installed desktop entries, editing `mimeapps.list` without losing its contents or formatting, writing `mimeinfo.cache`, and plans for opening a file or URI. |
| [`xdg-kit-bookmarks`](https://docs.rs/xdg-kit-bookmarks) | The Desktop Bookmark spec: `recently-used.xbel`, read leniently and edited losslessly — everything other applications put in the file survives byte for byte — with atomic, conflict-detecting writes. |
| [`xdg-kit-thumbnails`](https://docs.rs/xdg-kit-thumbnails) | The Thumbnail Managing Standard: `.thumbnailer` discovery and commands, the thumbnail cache with its keys, freshness and failure markers, and the shared per-directory repositories. |
| [`xdg-kit-icon-theme`](https://docs.rs/xdg-kit-icon-theme) | The Icon Theme spec: `index.theme` inheritance, the exact lookup algorithm over an in-memory index (built from validated GTK caches or directory scans), and the Icon Naming spec's fallback chains. |
| [`xdg-kit-autostart`](https://docs.rs/xdg-kit-autostart) | The Desktop Application Autostart spec: the entries a session starts after login, with precedence and `Hidden` overrides, structured reasons for every entry that does not start, launch plans, and user overrides that enable, disable or reset an entry without losing anything in the file. |

## Installing

Install all crates through the umbrella crate:

```sh
cargo add xdg-kit
```

Or select the crates you need through Cargo features:

```sh
cargo add xdg-kit --no-default-features --features mime
```

Or one crate on its own:

```sh
cargo add xdg-kit-mime
```

The minimum supported Rust version is 1.98 (edition 2024).

## Features of `xdg-kit`

Each crate has a feature, enabled by default. Enabling a feature also
enables the features that provide types used by its API. The exception is
`locale`: `desktop-entry` and `mime` use only `Locales` from that crate and
re-export it.

| Feature | Default | Gives | Also turns on |
| --- | --- | --- | --- |
| `basedir` | yes | `xdg_kit::basedir`: the XDG Base Directory spec and user folders | |
| `desktop-entry` | yes | `xdg_kit::desktop_entry`: desktop entries: reading, writing, validation, installed entries by desktop file ID, launching | `basedir` |
| `locale` | yes | `xdg_kit::locale`: the user's languages, for translated values | |
| `mime` | yes | `xdg_kit::mime`: the Shared MIME-info database | `basedir` |
| `mime-apps` | yes | `xdg_kit::mime_apps`: applications for MIME types: defaults, associations, `mimeapps.list`, opening files and URIs | `desktop-entry`, `mime`, `basedir` |
| `bookmarks` | yes | `xdg_kit::bookmarks`: `recently-used.xbel`, read leniently, edited losslessly, written with conflict detection | `basedir`, `mime` |
| `thumbnails` | yes | `xdg_kit::thumbnails`: thumbnailer discovery and commands, the thumbnail cache and the shared repositories | `desktop-entry`, `mime`, `basedir` |
| `icon-theme` | yes | `xdg_kit::icon_theme`: icon lookup over theme inheritance, from GTK caches or directory scans | `desktop-entry`, `locale`, `basedir` |
| `autostart` | yes | `xdg_kit::autostart`: login autostart entries, why each does or does not start, launch plans, user overrides | `desktop-entry`, `basedir` |
| `desktop-entry-dbus` | no | D-Bus activation when launching `DBusActivatable` applications | `desktop-entry` |

When using the crates directly, enable `dbus` on `xdg-kit-desktop-entry`
for this addition.

## Examples

Base directories and user folders, here from given variables and file
contents rather than the real environment:

```rust
use std::path::Path;

use xdg_kit::basedir::{BaseDirs, UserDirKind, UserDirs};

let vars = |name: &str| match name {
    "HOME" => Some("/home/u".into()),
    _ => None,
};
let dirs = BaseDirs::from_vars(vars)?;
assert_eq!(dirs.config_home(), Path::new("/home/u/.config"));

let folders = UserDirs::from_bytes(Path::new("/home/u"), b"XDG_MUSIC_DIR=\"$HOME/Music\"\n");
assert_eq!(folders.get(&UserDirKind::Music), Some(Path::new("/home/u/Music")));
# Ok::<(), xdg_kit::basedir::Error>(())
```

A desktop entry, with its name in the user's language:

```rust
use xdg_kit::desktop_entry::DesktopEntry;
use xdg_kit::locale::Locales;

let entry = DesktopEntry::parse(
    b"[Desktop Entry]\n\
      Type=Application\n\
      Name=Viewer\n\
      Name[de]=Betrachter\n\
      Exec=viewer %f\n\
      MimeType=text/plain;\n",
);
let locales = Locales::from_vars(|name: &str| match name {
    "LANG" => Some("de_DE.UTF-8".into()),
    _ => None,
});
assert_eq!(entry.name(&locales).as_deref(), Some("Betrachter"));
assert_eq!(entry.mime_types(), ["text/plain"]);
```

The type of a file, from the installed MIME database:

```rust,no_run
use xdg_kit::basedir::BaseDirs;
use xdg_kit::locale::Locales;
use xdg_kit::mime::{MimeDatabase, PathOptions};

let db = MimeDatabase::load(&BaseDirs::from_env()?);
let found = db.type_for_path("report.pdf", &PathOptions::default())?;
let description = db.description(&found.mime, &Locales::from_env());
println!("{} ({})", found.mime, description.unwrap_or_default());
# Ok::<(), Box<dyn std::error::Error>>(())
```

Setting a default application in a `mimeapps.list`, keeping the rest of
the file as it was:

```rust
use xdg_kit::mime_apps::{Group, MimeAppsList, TypePattern};

let mut list = MimeAppsList::parse(b"# mine\n[Default Applications]\n", false);
let text = TypePattern::parse("text/plain").unwrap();
list.set_default(&text, "org.example.Viewer.desktop")?;
assert_eq!(list.ids(Group::Defaults, &text), ["org.example.Viewer.desktop"]);
assert!(list.to_bytes().starts_with(b"# mine\n"));
# Ok::<(), xdg_kit::mime_apps::EditError>(())
```

Opening a file with the user's default application:

```rust,no_run
use std::sync::Arc;

use xdg_kit::basedir::BaseDirs;
use xdg_kit::desktop_entry::{AppIndex, Desktops, LaunchOptions, Target};
use xdg_kit::mime::MimeDatabase;
use xdg_kit::mime_apps::{MimeApps, open_plan};

let base_dirs = BaseDirs::from_env()?;
let apps = MimeApps::load(
    &base_dirs,
    &Desktops::from_env(),
    Arc::new(AppIndex::load(&base_dirs)),
    Arc::new(MimeDatabase::load(&base_dirs)),
);
let plan = open_plan(&Target::Path("notes.txt".into()), &apps, &LaunchOptions::default())?;
for mut child in plan.spawn()? {
    child.wait()?;
}
# Ok::<(), Box<dyn std::error::Error>>(())
```

Recording a visit in the recently-used list, losslessly:

```rust
use xdg_kit::bookmarks::{BookmarkFile, Opened};
use xdg_kit::mime::MimeType;

let mut recent = BookmarkFile::from_bytes(
    b"<?xml version=\"1.0\"?>\n<xbel version=\"1.0\">\n</xbel>\n",
);
recent.open("file:///home/u/notes.txt", &Opened {
    name: "viewer".into(),
    exec: "'viewer %u'".into(),
    mime: MimeType::new("text/plain").unwrap(),
    groups: Vec::new(),
    private: false,
    when: std::time::SystemTime::now(),
})?;
assert!(recent.get("file:///home/u/notes.txt").is_some());
# Ok::<(), xdg_kit::bookmarks::EditError>(())
```

The thumbnail cache key for a file, as every generator names it:

```rust
use xdg_kit::thumbnails::ThumbnailSource;

let source = ThumbnailSource::local("/home/u/photos/cat.png")?;
assert_eq!(source.uri(), "file:///home/u/photos/cat.png");
assert!(source.cache_name().ends_with(".png"));
# Ok::<(), std::io::Error>(())
```

Disabling a login autostart entry for the user, keeping the system's file
untouched (here on given directories, not the real ones):

```rust
use xdg_kit::autostart::{AutostartEntries, Context, Desktops, Ineligible, Limits};

let root = std::env::temp_dir().join(format!("xdg-kit-readme-{}", std::process::id()));
let (user, system) = (root.join("user"), root.join("system"));
std::fs::create_dir_all(&system)?;
std::fs::write(
    system.join("tray.desktop"),
    "[Desktop Entry]\nType=Application\nName=Tray\nExec=tray\n",
)?;
let entries = AutostartEntries::from_dirs(&user, [&system], &Limits::default())?;
let mut tray = entries.edit("tray.desktop")?;
tray.disable()?;
let _durability = tray.write()?;
let entries = entries.refresh().expect("the override appeared");
let session = Context::new(Desktops::new(["GNOME"]), "/usr/bin");
assert_eq!(
    entries.get("tray.desktop").unwrap().eligibility(&session).reasons(),
    [Ineligible::Hidden]
);
std::fs::remove_dir_all(&root)?;
# Ok::<(), Box<dyn std::error::Error>>(())
```

An icon from the user's theme, at a size and scale:

```rust,no_run
use xdg_kit::basedir::BaseDirs;
use xdg_kit::icon_theme::{IconRequest, IconThemes, Formats};

let themes = IconThemes::load(&BaseDirs::from_env()?, "Adwaita");
let request = IconRequest { size: 48, scale: 2, formats: Formats::default() };
if let Some(icon) = themes.lookup_with_fallbacks("folder-music", &request) {
    println!("{}", icon.path().display());
}
# Ok::<(), xdg_kit::basedir::Error>(())
```

Each crate's documentation has more.

## Platforms

The crates are tested only on Linux, but the code targets Unix in general.
System-specific calls are selected with `cfg` where they are used. Other
systems fall back to portable calls or fixed values: `fstat` instead of
`statx`, `O_SEARCH` or nothing instead of `O_PATH`, and a link limit of 40. The `user.mime_type` extended attribute is
read only on Linux, Android, Hurd and Apple's systems.
These fallbacks have not been built or tested on any system but Linux.

`xdg-kit-autostart`, `xdg-kit-basedir`, `xdg-kit-bookmarks`,
`xdg-kit-desktop-entry`, `xdg-kit-icon-theme`, `xdg-kit-mime`,
`xdg-kit-mime-apps` and `xdg-kit-thumbnails` stop with a compile error on
systems that are not Unix.
`xdg-kit-locale` uses only the standard library and has no such check.

## Tests

The tests compare the crates with reference data and tools pinned by the
Nix flake, so run them in its dev shell:

```sh
nix develop -c cargo test --workspace --all-features
```

The dev shell provides:

- `XDG_KIT_SMI_SRC`, the shared-mime-info source at the commit pinned in
  `flake.lock` (its reference database and detection test suite), and
  `update-mime-database` built from that same commit;
- `XDG_KIT_XDG_SPECS_SRC`, the xdg-specs source at its pinned commit (the
  Desktop Entry and Desktop Menu specs);
- `desktop-file-validate` and `update-desktop-database` built from the
  pinned desktop-file-utils commit;
- `xdg-user-dir` (xdg-user-dirs), `dbus-daemon` (for the D-Bus activation
  test, which runs on Linux with the `dbus` feature) and `gio` (GLib), from
  the locked nixpkgs;
- `XDG_KIT_SYSTEMD`, systemd from the locked nixpkgs, whose
  `systemd-xdg-autostart-generator` the autostart tests compare with.

None of this reference material is copied into the repository. Outside the
dev shell, the tests that need it fail with a message that says to run
them there. One test sets the `user.mime_type` extended attribute on a
temporary file, so the temporary folder must be on a filesystem that holds
user extended attributes; `XDG_KIT_SKIP_XATTR_TEST=1` skips it.

`nix flake check` runs the formatting, Clippy, test and documentation
checks (`checks.workspace`) and a short run of each fuzz target
(`checks.fuzz`). The fuzz targets in `fuzz/` are outside the workspace and
need nightly Rust and cargo-fuzz, provided by `nix develop .#fuzz`.
Generate their seeds first with `nix develop -c fuzz/make-seeds.sh`.

Two more dev shells cover other systems:

- `nix develop .#cross -c ci/clippy-target.sh <target>` runs Clippy on each
  crate's library and tests for another target, with default and with all
  features. It uses the latest stable Rust at the fenix commit in
  `flake.lock`, which can be newer than the default shell's, with the
  standard library of the targets in `XDG_KIT_CROSS_TARGETS`; those in
  `XDG_KIT_CROSS_SOURCE_TARGETS` build it from source.
- `nix develop .#i686 -c cargo test --workspace --all-features` runs the
  tests built for 32-bit x86 (on x86-64 Linux only).

The GitHub Actions workflow (`.github/workflows/ci.yml`) runs on pushes to
`master`, on pull requests and when started by hand: `nix flake check`; in
the dev shell the extended attribute test, the MIME crate's tests with
quick-xml's `encoding` and `escape-html` features, and each crate built
with default and with no features (`ci/build-features.sh`);
`ci/clippy-target.sh` for every target of the dev shell `cross`; and the
i686 tests.

## License

Apache-2.0 (`LICENSE`), except `xdg-kit-mime`, which is
`Apache-2.0 AND Unicode-3.0`: its glob character class tables are generated
from Unicode's `UnicodeData.txt`, which is under the Unicode License v3
(`crates/xdg-kit-mime/LICENSE-UNICODE`).
