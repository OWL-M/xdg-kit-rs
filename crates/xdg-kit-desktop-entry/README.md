# xdg-kit-desktop-entry

Implements the Desktop Entry spec (version 1.5, with the latest text of xdg-specs). Reads and writes `.desktop` and `.directory` files without losing contents or formatting. Supports typed keys, translations, actions, validation, lookup by desktop file ID, and application launching.

Part of [xdg-kit-rs](https://github.com/OWL-M/xdg-kit-rs), Rust crates for the
freedesktop.org XDG specifications. The
[`xdg-kit`](https://crates.io/crates/xdg-kit) crate re-exports this one as `xdg_kit::desktop_entry` (feature `desktop-entry`).

## Installing

```sh
cargo add xdg-kit-desktop-entry
```

Optional feature:

- `dbus`: launching applications through D-Bus activation (`DBusActivatable=true`).

The minimum supported Rust version is 1.98 (edition 2024).
The crate is Unix-only: on other systems it stops with a compile error.
It is tested only on Linux.

## Example

A desktop entry, with its name in the user's language:

```rust
use xdg_kit_desktop_entry::{DesktopEntry, Locales};

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

The [documentation](https://docs.rs/xdg-kit-desktop-entry) has more.

## License

Apache-2.0 (`LICENSE`).
