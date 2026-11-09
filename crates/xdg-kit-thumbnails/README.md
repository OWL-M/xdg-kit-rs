# xdg-kit-thumbnails

The Thumbnail Managing Standard: `.thumbnailer` discovery and commands, the thumbnail cache with its keys, freshness and failure markers, and the shared per-directory repositories.

Part of [xdg-kit-rs](https://github.com/OWL-M/xdg-kit-rs), Rust crates for the
freedesktop.org XDG specifications. The
[`xdg-kit`](https://crates.io/crates/xdg-kit) crate re-exports this one as `xdg_kit::thumbnails` (feature `thumbnails`).

## Installing

```sh
cargo add xdg-kit-thumbnails
```

The minimum supported Rust version is 1.98 (edition 2024).
The crate is Unix-only: on other systems it stops with a compile error.
It is tested only on Linux.

## Example

The thumbnail cache key for a file, as every generator names it:

```rust
use xdg_kit_thumbnails::ThumbnailSource;

let source = ThumbnailSource::local("/home/u/photos/cat.png")?;
assert_eq!(source.uri(), "file:///home/u/photos/cat.png");
assert!(source.cache_name().ends_with(".png"));
# Ok::<(), std::io::Error>(())
```

The [documentation](https://docs.rs/xdg-kit-thumbnails) has more.

## License

Apache-2.0 (`LICENSE`).
