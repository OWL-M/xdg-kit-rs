# xdg-kit-basedir

The XDG Base Directory spec (version 0.8) and the XDG user folders (`user-dirs.dirs`).

Part of [xdg-kit-rs](https://github.com/OWL-M/xdg-kit-rs), Rust crates for the
freedesktop.org XDG specifications. The
[`xdg-kit`](https://crates.io/crates/xdg-kit) crate re-exports this one as `xdg_kit::basedir` (feature `basedir`).

## Installing

```sh
cargo add xdg-kit-basedir
```

The minimum supported Rust version is 1.98 (edition 2024).
The crate is Unix-only: on other systems it stops with a compile error.
It is tested only on Linux.

## Example

Base directories and user folders, here from given variables and file
contents rather than the real environment:

```rust
use std::path::Path;

use xdg_kit_basedir::{BaseDirs, UserDirKind, UserDirs};

let vars = |name: &str| match name {
    "HOME" => Some("/home/u".into()),
    _ => None,
};
let dirs = BaseDirs::from_vars(vars)?;
assert_eq!(dirs.config_home(), Path::new("/home/u/.config"));

let folders = UserDirs::from_bytes(Path::new("/home/u"), b"XDG_MUSIC_DIR=\"$HOME/Music\"\n");
assert_eq!(folders.get(&UserDirKind::Music), Some(Path::new("/home/u/Music")));
# Ok::<(), xdg_kit_basedir::Error>(())
```

The [documentation](https://docs.rs/xdg-kit-basedir) has more.

## License

Apache-2.0 (`LICENSE`).
