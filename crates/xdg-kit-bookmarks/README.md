# xdg-kit-bookmarks

The Desktop Bookmark spec: `recently-used.xbel`, read leniently and edited losslessly — everything other applications put in the file survives byte for byte — with atomic, conflict-detecting writes.

Part of [xdg-kit-rs](https://github.com/OWL-M/xdg-kit-rs), Rust crates for the
freedesktop.org XDG specifications. The
[`xdg-kit`](https://crates.io/crates/xdg-kit) crate re-exports this one as `xdg_kit::bookmarks` (feature `bookmarks`).

## Installing

```sh
cargo add xdg-kit-bookmarks xdg-kit-mime
```

The example below also uses `xdg-kit-mime`.

The minimum supported Rust version is 1.98 (edition 2024).
The crate is Unix-only: on other systems it stops with a compile error.
It is tested only on Linux.

## Example

Recording a visit in the recently-used list, losslessly:

```rust
use xdg_kit_bookmarks::{BookmarkFile, Opened};
use xdg_kit_mime::MimeType;

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
# Ok::<(), xdg_kit_bookmarks::EditError>(())
```

The [documentation](https://docs.rs/xdg-kit-bookmarks) has more.

## License

Apache-2.0 (`LICENSE`).
