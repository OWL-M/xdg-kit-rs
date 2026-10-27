//! The Desktop Menu spec's own regression suite (`menu/tests` of the
//! pinned xdg-specs: `XDG_KIT_XDG_SPECS_SRC`), run as it is.
//!
//! The suite's `menutest` runner sets up each test in a scratch directory
//! and runs `$MENUTEST`, which must print the menu as
//! `<menu path>/<TAB><desktop-file id><TAB><file>` lines. This binary is
//! both: run as a test it drives the runner (on a writable copy of the
//! suite, `HOME` and every XDG variable pointed into temporary
//! directories, so nothing real is read or written), and the runner calls
//! it back with `XDG_KIT_MENUTEST_PRINT` set to print the menu.
//!
//! Every test must pass except those in `EXCLUDED`, each with its reason.
//! The suite's absolute-path variants (`AppDir`, `DirectoryDir`,
//! `MergeDir-absolute`, `MergeFile-absolute`) source the relative test's
//! body with absolute paths but ship no `result` file, so the runner
//! would fail them for any implementation; their expected menu is the
//! relative test's, which the copy gives them.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::process::Command;

use xdg_kit_menu::{Aliased, Menu, MenuContext, MenuItem, MenuTree};

/// (test, reason) — not run.
const EXCLUDED: &[(&str, &str)] = &[(
    "official-categories",
    "checks the menu installed on the system (and may run xdg-desktop-menu as root, \
     interactively), not an implementation",
)];

fn main() {
    if std::env::var_os("XDG_KIT_MENUTEST_PRINT").is_some() {
        print_menu();
        return;
    }
    let source =
        Path::new(&std::env::var_os("XDG_KIT_XDG_SPECS_SRC").expect(
            "XDG_KIT_XDG_SPECS_SRC is not set: run the tests in the dev shell (nix develop)",
        ))
        .join("menu/tests");
    let scratch = tempfile::tempdir().unwrap();
    let suite = scratch.path().join("suite");
    copy_tree(&source, &suite);
    give_variants_their_results(&suite);
    use_perl_from_path(&suite);
    let tests: Vec<String> = fs::read_dir(suite.join("tests"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .filter(|name| suite.join("tests").join(name).join("test").is_file())
        .collect();
    let mut results = BTreeMap::new();
    for test in &tests {
        if EXCLUDED.iter().any(|(name, _)| name == test) {
            continue;
        }
        let output = run(&suite, scratch.path(), test);
        let passed = output.lines().any(|line| line.starts_with(">>> OK"));
        // An empty expectation compared with an empty menu would pass
        // vacuously (a helper that failed to run leaves both empty): every
        // test here expects entries, so both must have some.
        let tested = scratch.path().join("menutestdir");
        let nonempty = |name: &str| fs::metadata(tested.join(name)).is_ok_and(|m| m.len() > 0);
        let real = nonempty("required-result") && nonempty("run-result");
        let output = if real {
            output
        } else {
            format!("{output}\n(vacuous: the expected result or the menu printed is empty)")
        };
        results.insert(test.clone(), (passed && real, output));
    }
    let failed: Vec<_> = results
        .iter()
        .filter(|(_, (passed, _))| !passed)
        .map(|(test, (_, output))| format!("=== {test}\n{output}"))
        .collect();
    println!(
        "{} of {} spec tests passed ({} excluded)",
        results.len() - failed.len(),
        results.len(),
        EXCLUDED.len()
    );
    assert!(
        failed.is_empty(),
        "failed spec tests:\n{}",
        failed.join("\n")
    );
    assert!(
        results.len() >= 35,
        "the suite was found: {} tests",
        results.len()
    );
}

/// The suite's `expand` helper starts with `#!/usr/bin/perl`, which not
/// every system has (NixOS and the Nix build sandbox do not): it is made
/// to start with the `perl` found on `PATH` instead.
fn use_perl_from_path(suite: &Path) {
    let perl = std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .map(|dir| dir.join("perl"))
        .find(|path| path.is_file())
        .expect("perl on PATH: run the tests in the dev shell (nix develop)");
    let expand = suite.join("expand");
    let script = fs::read_to_string(&expand).unwrap();
    let body = script.split_once('\n').map_or("", |(_, body)| body);
    fs::write(&expand, format!("#!{}\n{body}", perl.display())).unwrap();
}

/// A test without `result` that sources another test (`. tests/X/test`)
/// gets that test's result: the same menu, its paths only written
/// differently.
fn give_variants_their_results(suite: &Path) {
    for entry in fs::read_dir(suite.join("tests")).unwrap() {
        let dir = entry.unwrap().path();
        if dir.join("result").exists() {
            continue;
        }
        let Ok(test) = fs::read_to_string(dir.join("test")) else {
            continue;
        };
        let sourced = test.lines().find_map(|line| {
            line.strip_prefix(". tests/")
                .and_then(|rest| rest.strip_suffix("/test"))
        });
        if let Some(sourced) = sourced {
            let result = suite.join("tests").join(sourced).join("result");
            if result.exists() {
                fs::copy(result, dir.join("result")).unwrap();
            }
        }
    }
}

/// Runs one test of the suite through its runner, this binary as
/// `$MENUTEST`; the runner's output.
fn run(suite: &Path, scratch: &Path, test: &str) -> String {
    let home = scratch.join("home");
    fs::create_dir_all(&home).unwrap();
    let output = Command::new("bash")
        .arg("menutest")
        .current_dir(suite)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", &home)
        .env("USER", "test")
        .env("TESTS", test)
        .env("MENUTEST", std::env::current_exe().unwrap())
        .env("MENUTESTDIR", scratch.join("menutestdir"))
        .env("XDG_KIT_MENUTEST_PRINT", "1")
        // The runner appends these to its own: point them nowhere real.
        .env("XDG_CONFIG_DIRS", scratch.join("no-config"))
        .env("XDG_DATA_DIRS", scratch.join("no-data"))
        .output()
        .unwrap();
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

/// Prints the menu the environment describes in the suite's format.
fn print_menu() {
    let context = MenuContext::from_env().expect("the runner sets HOME");
    match MenuTree::load(&context) {
        Ok(tree) => {
            for diagnostic in tree.diagnostics() {
                eprintln!("{diagnostic}");
            }
            print_items(tree.root(), "");
        }
        Err(err) => eprintln!("{err}"),
    }
}

/// The suite's "full menu name" is the path of captions (`.directory`
/// names where there are), not of `<Name>`s.
fn print_items(menu: &Menu, path: &str) {
    let here = if path.is_empty() {
        "/".to_owned()
    } else {
        format!("{path}/")
    };
    let below = |child: &Menu| {
        if path.is_empty() {
            child.label().to_owned()
        } else {
            format!("{path}/{}", child.label())
        }
    };
    for item in menu.items() {
        match item {
            MenuItem::Entry(entry) => {
                println!("{here}\t{}\t{}", entry.id(), entry.path().display());
            }
            MenuItem::Menu(child) => print_items(child, &below(child)),
            MenuItem::Alias(alias) => match alias.item() {
                Aliased::Entry(entry) => {
                    println!("{here}\t{}\t{}", entry.id(), entry.path().display());
                }
                Aliased::Menu(child) => print_items(child, &below(child)),
            },
            MenuItem::Separator | MenuItem::Header(_) => {}
        }
    }
}

/// Copies the suite, keeping file modes (its scripts are executable).
fn copy_tree(from: &Path, to: &Path) {
    use std::os::unix::fs::PermissionsExt;
    fs::create_dir_all(to).unwrap();
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), &target).unwrap();
            let mode = fs::metadata(entry.path()).unwrap().permissions().mode();
            fs::set_permissions(&target, fs::Permissions::from_mode(mode | 0o200)).unwrap();
        }
    }
    let mode = fs::metadata(from).unwrap().permissions().mode();
    fs::set_permissions(to, fs::Permissions::from_mode(mode | 0o700)).unwrap();
}
