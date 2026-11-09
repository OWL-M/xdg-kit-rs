# xdg-kit-mime

The Shared MIME-info database (spec version 0.22, whose text still calls itself 0.21): file types from names, content and file kinds, their relationships, icons and descriptions, and content types of mounted volumes.

Part of [xdg-kit-rs](https://github.com/OWL-M/xdg-kit-rs), Rust crates for the
freedesktop.org XDG specifications. The
[`xdg-kit`](https://crates.io/crates/xdg-kit) crate re-exports this one as `xdg_kit::mime` (feature `mime`).

## Installing

```sh
cargo add xdg-kit-mime xdg-kit-basedir
```

The example below also uses `xdg-kit-basedir`.

The minimum supported Rust version is 1.98 (edition 2024).
The crate is Unix-only: on other systems it stops with a compile error.
It is tested only on Linux.

## Example

The type of a file, from the installed MIME database:

```rust,no_run
use xdg_kit_basedir::BaseDirs;
use xdg_kit_mime::{Locales, MimeDatabase, PathOptions};

let db = MimeDatabase::load(&BaseDirs::from_env()?);
let found = db.type_for_path("report.pdf", &PathOptions::default())?;
let description = db.description(&found.mime, &Locales::from_env());
println!("{} ({})", found.mime, description.unwrap_or_default());
# Ok::<(), Box<dyn std::error::Error>>(())
```

The [documentation](https://docs.rs/xdg-kit-mime) has more.

## License

`Apache-2.0 AND Unicode-3.0`: Apache-2.0 (`LICENSE`), and the Unicode
License v3 (`LICENSE-UNICODE`) for the glob character class tables,
which are generated from Unicode's `UnicodeData.txt`.
