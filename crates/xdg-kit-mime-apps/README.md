# xdg-kit-mime-apps

The MIME Applications Associations spec (version 1.0.1): applications and defaults for a MIME type from every `mimeapps.list` and the installed desktop entries, editing `mimeapps.list` without losing its contents or formatting, writing `mimeinfo.cache`, and plans for opening a file or URI.

Part of [xdg-kit-rs](https://github.com/OWL-M/xdg-kit-rs), Rust crates for the
freedesktop.org XDG specifications. The
[`xdg-kit`](https://crates.io/crates/xdg-kit) crate re-exports this one as `xdg_kit::mime_apps` (feature `mime-apps`).

## Installing

```sh
cargo add xdg-kit-mime-apps
```

The minimum supported Rust version is 1.98 (edition 2024).
The crate is Unix-only: on other systems it stops with a compile error.
It is tested only on Linux.

## Example

Setting a default application in a `mimeapps.list`, keeping the rest of
the file as it was:

```rust
use xdg_kit_mime_apps::{Group, MimeAppsList, TypePattern};

let mut list = MimeAppsList::parse(b"# mine\n[Default Applications]\n", false);
let text = TypePattern::parse("text/plain").unwrap();
list.set_default(&text, "org.example.Viewer.desktop")?;
assert_eq!(list.ids(Group::Defaults, &text), ["org.example.Viewer.desktop"]);
assert!(list.to_bytes().starts_with(b"# mine\n"));
# Ok::<(), xdg_kit_mime_apps::EditError>(())
```

The [documentation](https://docs.rs/xdg-kit-mime-apps) has more.

## License

Apache-2.0 (`LICENSE`).
