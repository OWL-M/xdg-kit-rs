//! The deepest nesting the limits allow is loaded and resolved without
//! overflowing the stack of a thread spawned with the default size
//! (2 MiB), unoptimized. An overflow aborts the process, so each case runs
//! in a child process — this binary again, with `XDG_KIT_STACK_CASE` set —
//! on such a thread, and the parent checks how it exited.

use std::fmt::Write as _;
use std::fs;
use std::path::Path;
use std::process::Command;

use xdg_kit_menu::{Limits, MenuContext, MenuError, MenuTree};

/// Every recursive shape: menus in menus (each with rules, a layout and a
/// move, so every pass walks them), rules in rules, unknown elements in
/// unknown elements, a move that builds a deep path, and a chain of
/// merged files ending in nested menus.
const CASES: &[&str] = &["menus", "rules", "unknown", "moves", "merges"];

/// Writes the case's menu files under `dir`, nested `depth` deep;
/// `menu.menu` is the root.
fn write_case(dir: &Path, case: &str, depth: usize) {
    let mut menu = String::from("<Menu><Name>R</Name><AppDir>apps</AppDir>");
    match case {
        "menus" => menu.push_str(&nested_menus(depth)),
        "rules" => {
            menu.push_str("<Include>");
            menu.push_str(&"<Not>".repeat(depth));
            menu.push_str("<All/>");
            menu.push_str(&"</Not>".repeat(depth));
            menu.push_str("</Include>");
        }
        "unknown" => {
            menu.push_str(&"<Unknown>".repeat(depth));
            menu.push_str(&"</Unknown>".repeat(depth));
        }
        "moves" => {
            let path: Vec<String> = (0..depth).map(|at| format!("P{at}")).collect();
            write!(
                menu,
                "<Menu><Name>A</Name><Include><All/></Include></Menu>\
                 <Move><Old>A</Old><New>{}</New></Move>",
                path.join("/")
            )
            .unwrap();
        }
        "merges" => {
            menu.push_str("<MergeFile>m0.menu</MergeFile>");
            for at in 0..depth {
                let next = if at + 1 < depth {
                    format!("<MergeFile>m{}.menu</MergeFile>", at + 1)
                } else {
                    nested_menus(depth)
                };
                fs::write(
                    dir.join(format!("m{at}.menu")),
                    format!("<Menu><Name>R</Name>{next}</Menu>"),
                )
                .unwrap();
            }
        }
        _ => unreachable!("{case}"),
    }
    menu.push_str("</Menu>");
    fs::write(dir.join("menu.menu"), menu).unwrap();
}

/// `depth` menus, each in the one before, with rules, a layout and a move.
fn nested_menus(depth: usize) -> String {
    let mut menus = String::new();
    for at in 0..depth {
        write!(
            menus,
            "<Menu><Name>M{at}</Name><Include><Not><Category>X</Category></Not></Include>\
             <Layout><Merge type=\"all\"/></Layout><Move><Old>A</Old><New>B</New></Move>"
        )
        .unwrap();
    }
    menus.push_str(&"</Menu>".repeat(depth));
    menus
}

/// Loads the case at `XDG_KIT_STACK_DEPTH` with every limit off: just
/// under the ceiling it must load; well past it, stop at the limit.
fn run_case(case: &str) {
    let depth: usize = std::env::var("XDG_KIT_STACK_DEPTH")
        .unwrap()
        .parse()
        .unwrap();
    let root = tempfile::tempdir().unwrap();
    fs::create_dir_all(root.path().join("apps")).unwrap();
    fs::write(
        root.path().join("apps/a.desktop"),
        "[Desktop Entry]\nType=Application\nExec=true\nName=A\n",
    )
    .unwrap();
    write_case(root.path(), case, depth);
    let path = root.path().join("menu.menu");
    let home = root.path().join("home");
    let vars = |name: &str| (name == "HOME").then(|| home.clone().into_os_string());
    let mut context = MenuContext::from_vars(vars).unwrap();
    context.limits = Limits::unlimited();
    let refused = depth > Limits::MAX_DEPTH;
    let case = case.to_owned();
    std::thread::spawn(move || match MenuTree::load_file(&context, &path) {
        Ok(_) if !refused => {}
        Err(MenuError::Limit { .. }) if refused => {}
        other => panic!("{case} at {depth}: {:?}", other.map(|_| ())),
    })
    .join()
    .unwrap();
}

#[test]
fn the_deepest_nesting_allowed_fits_a_default_thread_stack() {
    if let Ok(case) = std::env::var("XDG_KIT_STACK_CASE") {
        run_case(&case);
        return;
    }
    // The root and the elements around the nesting take a few levels.
    for depth in [Limits::MAX_DEPTH - 4, 2 * Limits::MAX_DEPTH] {
        for case in CASES {
            let output = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "the_deepest_nesting_allowed_fits_a_default_thread_stack",
                    "--nocapture",
                ])
                .env("XDG_KIT_STACK_CASE", case)
                .env("XDG_KIT_STACK_DEPTH", depth.to_string())
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "case {case} at {depth}: {:?}\n{}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }
}
