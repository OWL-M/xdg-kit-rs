//! Writes `src/fnmatch/class_tables.rs`, the tables behind the glob
//! classes, from the Unicode Character Database, with the formulas of glibc
//! 2.44's `localedata/unicode-gen/unicode_utils.py`. An ignored test: run
//! it from the repository root with `XDG_KIT_UCD` naming the folder that
//! holds `UnicodeData.txt` and `DerivedCoreProperties.txt`, as the locked
//! nixpkgs gives it:
//!
//! ```sh
//! XDG_KIT_UCD=$(nix build --inputs-from . --no-link --print-out-paths \
//!   nixpkgs#unicode-character-database)/share/unicode \
//!   nix develop -c cargo test -p xdg-kit-mime --test classes -- --ignored
//! ```
//!
//! Rust's std must be at the same Unicode version (`char::UNICODE_VERSION`):
//! the classes take Alphabetic, Uppercase, Lowercase and the case mappings
//! from it. `glibc-classes.c` writes the reference the tables are tested
//! against.

use std::fmt::Write as _;
use std::path::Path;

/// One code point's `UnicodeData.txt` fields the formulas use.
struct Point {
    code: u32,
    name: String,
    category: String,
    decomposition: String,
}

/// The code points `UnicodeData.txt` lists, `First`/`Last` ranges
/// expanded and surrogates left out, as glibc's `fill_attributes` does.
fn read(ucd: &Path) -> Vec<Point> {
    let data = std::fs::read_to_string(ucd.join("UnicodeData.txt")).unwrap();
    let mut points = Vec::new();
    let mut first = None;
    for line in data.lines() {
        let fields: Vec<&str> = line.split(';').collect();
        let code = u32::from_str_radix(fields[0], 16).unwrap();
        let (name, category) = (fields[1], fields[2]);
        if category == "Cs" {
            first = None;
            continue;
        }
        if name.ends_with(", First>") {
            first = Some(code);
            continue;
        }
        let start = if name.ends_with(", Last>") {
            first.unwrap()
        } else {
            code
        };
        first = None;
        for code in start..=code {
            points.push(Point {
                code,
                name: name.to_owned(),
                category: category.to_owned(),
                decomposition: fields[5].to_owned(),
            });
        }
    }
    points
}

/// The Unicode version, from `DerivedCoreProperties.txt`'s first line.
fn version(ucd: &Path) -> String {
    let data = std::fs::read_to_string(ucd.join("DerivedCoreProperties.txt")).unwrap();
    let first = data.lines().next().unwrap_or("");
    first
        .strip_prefix("# DerivedCoreProperties-")
        .and_then(|rest| rest.strip_suffix(".txt"))
        .filter(|version| version.split('.').all(|part| part.parse::<u8>().is_ok()))
        .unwrap_or_else(|| panic!("no version in {first:?}"))
        .to_owned()
}

/// A table of the code points `test` takes, as rustfmt writes it: on one
/// line when that fits in 100 columns.
fn table(name: &str, doc: &[&str], points: &[Point], test: impl Fn(&Point) -> bool) -> String {
    let mut runs: Vec<(u32, u32)> = Vec::new();
    for point in points.iter().filter(|point| test(point)) {
        match runs.last_mut() {
            Some((_, high)) if *high + 1 == point.code => *high = point.code,
            _ => runs.push((point.code, point.code)),
        }
    }
    let items: Vec<String> = runs
        .iter()
        .map(|(low, high)| format!("('\\u{{{low:x}}}', '\\u{{{high:x}}}')"))
        .collect();
    let mut out = String::new();
    for line in doc {
        writeln!(out, "/// {line}").unwrap();
    }
    let head = format!("pub(crate) const {name}: &[(char, char)] = &[");
    let one_line = format!("{head}{}];", items.join(", "));
    if one_line.len() <= 100 {
        out.push_str(&one_line);
    } else {
        out.push_str(&head);
        for item in &items {
            write!(out, "\n    {item},").unwrap();
        }
        out.push_str("\n];");
    }
    out
}

#[test]
#[ignore = "writes src/fnmatch/class_tables.rs; needs XDG_KIT_UCD"]
fn class_tables() {
    let ucd = std::env::var_os("XDG_KIT_UCD").expect("XDG_KIT_UCD: the UCD folder");
    let ucd = Path::new(&ucd);
    let points = read(ucd);
    let version = version(ucd);
    let tables = [
        table(
            "ASSIGNED",
            &[
                "Code points `UnicodeData.txt` lists, but surrogates: `graph`",
                "and `print` need one.",
            ],
            &points,
            |_| true,
        ),
        table(
            "CONTROLS",
            &[
                "Code points named `<control>`: `cntrl`, never `graph` or",
                "`print`.",
            ],
            &points,
            |point| point.name == "<control>",
        ),
        table(
            "SEPARATORS",
            &[
                "Line and paragraph separators (Zl, Zp): `cntrl` and `space`,",
                "never `print`.",
            ],
            &points,
            |point| matches!(point.category.as_str(), "Zl" | "Zp"),
        ),
        table(
            "BLANKS",
            &[
                "Tab and the Zs spaces without `<noBreak>`: `blank`, and",
                "`space`.",
            ],
            &points,
            |point| {
                point.code == 0x09
                    || (point.category == "Zs" && !point.decomposition.contains("<noBreak>"))
            },
        ),
        table(
            "OTHER_DIGITS",
            &["Decimal digits (Nd) other than ASCII ones: `alpha`."],
            &points,
            |point| point.category == "Nd" && !(0x30..=0x39).contains(&point.code),
        ),
    ];
    let tuple = version.replace('.', ", ");
    let text = format!(
        "\
//! Tables behind the glob classes (sorted ranges), generated by
//! `tests/classes/main.rs` (see there to regenerate) from Unicode
//! {version}'s `UnicodeData.txt`. Do not edit by hand. `src/fnmatch.rs`
//! joins them with std's properties by the formulas of glibc 2.44's
//! `localedata/unicode-gen/unicode_utils.py`, which its C.UTF-8 classes
//! come from.
//! Unicode's data is under the Unicode License v3: see `LICENSE-UNICODE`
//! in this crate.

/// The Unicode version the tables are for: std's must be the same.
#[cfg(test)]
pub(crate) const UNICODE_VERSION: (u8, u8, u8) = ({tuple});

{}
",
        tables.join("\n\n")
    );
    let out = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/fnmatch/class_tables.rs");
    std::fs::write(out, text).unwrap();
}
