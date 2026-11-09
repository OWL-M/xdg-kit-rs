# xdg-kit-menu

The Desktop Menu spec (version 1.1): `.menu` files resolved into the application menu tree — `XDG_MENU_PREFIX` and user overrides, merges, moves, deletions, `<Include>`/`<Exclude>` rules with `<OnlyUnallocated>`, legacy hierarchies, localized `.directory` captions, and layouts with separators and inlining — every step bounded, with each entry's desktop file and path kept for launching.

Part of [xdg-kit-rs](https://github.com/OWL-M/xdg-kit-rs), Rust crates for the
freedesktop.org XDG specifications. The
[`xdg-kit`](https://crates.io/crates/xdg-kit) crate re-exports this one as `xdg_kit::menu` (feature `menu`).

## Installing

```sh
cargo add xdg-kit-menu xdg-kit-basedir
```

The example below also uses `xdg-kit-basedir`.

The minimum supported Rust version is 1.98 (edition 2024).
The crate is Unix-only: on other systems it stops with a compile error.
It is tested only on Linux.

## Example

An application menu from a menu file (here a given one, not the user's):

```rust
use xdg_kit_basedir::BaseDirs;
use xdg_kit_menu::{MenuContext, MenuTree};

let root = std::env::temp_dir().join(format!("xdg-kit-readme-menu-{}", std::process::id()));
let apps = root.join("applications");
std::fs::create_dir_all(&apps)?;
std::fs::write(
    apps.join("org.example.Editor.desktop"),
    "[Desktop Entry]\nType=Application\nName=Editor\nExec=editor %f\nCategories=Utility;\n",
)?;
std::fs::write(
    root.join("applications.menu"),
    "<Menu><Name>Applications</Name><AppDir>applications</AppDir>\
     <Menu><Name>Accessories</Name><Include><Category>Utility</Category></Include></Menu></Menu>",
)?;
let vars = |name: &str| (name == "HOME").then(|| root.clone().into_os_string());
let context = MenuContext::new(BaseDirs::from_vars(vars)?);
let tree = MenuTree::load_file(&context, root.join("applications.menu"))?;
let accessories = tree.root().submenu("Accessories").unwrap();
let editor = accessories.entries().next().unwrap();
assert_eq!((editor.label(), editor.id()), ("Editor", "org.example.Editor.desktop"));
std::fs::remove_dir_all(&root)?;
# Ok::<(), Box<dyn std::error::Error>>(())
```

The [documentation](https://docs.rs/xdg-kit-menu) has more.

## License

Apache-2.0 (`LICENSE`).
