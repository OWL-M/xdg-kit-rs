# xdg-kit-trash

The Trash spec (version 1.0): trashing into the home trash or a volume's `.Trash/$uid` or `.Trash-$uid` by non-replacing rename, else a hard link (or, opted in, a rename over a placeholder) — never copied, never overwritten, metadata created exclusively first — with a planning query that creates nothing, caller-chosen volume exclusions checked before touching them, listing every payload and orphan with byte-exact original paths, restoring without replacing, erasing without following links or crossing mounts, and the `directorysizes` cache.

Part of [xdg-kit-rs](https://github.com/OWL-M/xdg-kit-rs), Rust crates for the
freedesktop.org XDG specifications. The
[`xdg-kit`](https://crates.io/crates/xdg-kit) crate re-exports this one as `xdg_kit::trash` (feature `trash`).

## Installing

```sh
cargo add xdg-kit-trash
```

The minimum supported Rust version is 1.98 (edition 2024).
The crate is Unix-only: on other systems it stops with a compile error.
It is tested only on Linux.

## Example

A file trashed and restored (here with a given data home, not the
user's trash):

```rust
use xdg_kit_trash::{Context, LocalZone, Origin, RestoreOptions};

let root = std::env::temp_dir().join(format!("xdg-kit-readme-trash-{}", std::process::id()));
std::fs::create_dir_all(&root)?;
let root = std::fs::canonicalize(&root)?;
std::fs::write(root.join("notes.txt"), "draft")?;
let uid = std::os::unix::fs::MetadataExt::uid(&std::fs::metadata(&root)?);
let context = Context::new(root.join("data"), uid, &root, LocalZone::utc())?;
let trashed = context.trash("notes.txt")?;
assert_eq!(trashed.trash.path(), root.join("data/Trash"));
let trash = context.open(&context.home_trash())?;
let entry = trash.entries().next().unwrap()?;
assert_eq!(entry.origin(), &Origin::Known(root.join("notes.txt")));
let restored = trash.restore(&entry, &RestoreOptions::default())?;
assert_eq!(std::fs::read_to_string(restored.destination)?, "draft");
std::fs::remove_dir_all(&root)?;
# Ok::<(), Box<dyn std::error::Error>>(())
```

The [documentation](https://docs.rs/xdg-kit-trash) has more.

## License

Apache-2.0 (`LICENSE`).
