# xdg-kit-icon-theme

The Icon Theme spec: `index.theme` inheritance, the exact lookup algorithm over an in-memory index (built from validated GTK caches or directory scans), and the Icon Naming spec's fallback chains.

Part of [xdg-kit-rs](https://github.com/OWL-M/xdg-kit-rs), Rust crates for the
freedesktop.org XDG specifications. The
[`xdg-kit`](https://crates.io/crates/xdg-kit) crate re-exports this one as `xdg_kit::icon_theme` (feature `icon-theme`).

## Installing

```sh
cargo add xdg-kit-icon-theme xdg-kit-basedir
```

The example below also uses `xdg-kit-basedir`.

The minimum supported Rust version is 1.98 (edition 2024).
The crate is Unix-only: on other systems it stops with a compile error.
It is tested only on Linux.

## Example

An icon from the user's theme, at a size and scale:

```rust,no_run
use xdg_kit_basedir::BaseDirs;
use xdg_kit_icon_theme::{IconRequest, IconThemes, Formats};

let themes = IconThemes::load(&BaseDirs::from_env()?, "Adwaita");
let request = IconRequest { size: 48, scale: 2, formats: Formats::default() };
if let Some(icon) = themes.lookup_with_fallbacks("folder-music", &request) {
    println!("{}", icon.path().display());
}
# Ok::<(), xdg_kit_basedir::Error>(())
```

The [documentation](https://docs.rs/xdg-kit-icon-theme) has more.

## License

Apache-2.0 (`LICENSE`).
