# xdg-kit-locale

Reads the user's languages from the environment as gettext does. Supplies translation lookup keys in the order defined by the Desktop Entry spec.

Part of [xdg-kit-rs](https://github.com/OWL-M/xdg-kit-rs), Rust crates for the
freedesktop.org XDG specifications. The
[`xdg-kit`](https://crates.io/crates/xdg-kit) crate re-exports this one as `xdg_kit::locale` (feature `locale`).

## Installing

```sh
cargo add xdg-kit-locale
```

The minimum supported Rust version is 1.98 (edition 2024).
It uses only the standard library.

## Example

The keys to try for a translated value, in the order of the Desktop
Entry spec:

```rust
use xdg_kit_locale::Locales;

let vars = |name: &str| match name {
    "LANG" => Some("de_DE.UTF-8@euro".into()),
    _ => None,
};
let locales = Locales::from_vars(vars);
let keys: Vec<&str> = locales.keys().collect();
assert_eq!(keys, ["de_DE@euro", "de_DE", "de@euro", "de"]);
```

The [documentation](https://docs.rs/xdg-kit-locale) has more.

## License

Apache-2.0 (`LICENSE`).
