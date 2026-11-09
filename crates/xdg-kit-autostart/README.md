# xdg-kit-autostart

The Desktop Application Autostart spec: the entries a session starts after login, with precedence and `Hidden` overrides, structured reasons for every entry that does not start, launch plans, and user overrides that enable, disable or reset an entry without losing anything in the file.

Part of [xdg-kit-rs](https://github.com/OWL-M/xdg-kit-rs), Rust crates for the
freedesktop.org XDG specifications. The
[`xdg-kit`](https://crates.io/crates/xdg-kit) crate re-exports this one as `xdg_kit::autostart` (feature `autostart`).

## Installing

```sh
cargo add xdg-kit-autostart
```

Optional feature:

- `dbus`: plans for `DBusActivatable=true` entries as D-Bus activation calls (the `dbus` feature of `xdg-kit-desktop-entry`).

The minimum supported Rust version is 1.98 (edition 2024).
The crate is Unix-only: on other systems it stops with a compile error.
It is tested only on Linux.

## Example

Disabling a login autostart entry for the user, keeping the system's file
untouched (here on given directories, not the real ones):

```rust
use xdg_kit_autostart::{AutostartEntries, Context, Desktops, Ineligible, Limits};

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

The [documentation](https://docs.rs/xdg-kit-autostart) has more.

## License

Apache-2.0 (`LICENSE`).
