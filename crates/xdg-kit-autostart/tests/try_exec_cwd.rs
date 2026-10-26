//! Relative and empty `PATH` components are not searched for a bare
//! `TryExec`, even when the working directory holds the program. The
//! working directory is process-global, so this binary holds exactly one
//! test and nothing else runs beside it.

use std::fs;
use std::os::unix::fs::PermissionsExt;

use xdg_kit_autostart::{AutostartEntries, Context, Desktops, Ineligible, Limits, TryExecProblem};

#[test]
fn relative_search_path_components_are_skipped() {
    let root = tempfile::tempdir().unwrap();
    let (user, here) = (root.path().join("autostart"), root.path().join("here"));
    fs::create_dir_all(here.join("sub")).unwrap();
    fs::create_dir_all(&user).unwrap();
    for program in [here.join("tool"), here.join("sub/tool")] {
        fs::write(&program, "#!/bin/sh\n").unwrap();
        fs::set_permissions(&program, fs::Permissions::from_mode(0o755)).unwrap();
    }
    fs::write(
        user.join("a.desktop"),
        "[Desktop Entry]\nType=Application\nName=A\nExec=a\nTryExec=tool\n",
    )
    .unwrap();
    std::env::set_current_dir(&here).unwrap();
    let entries =
        AutostartEntries::from_dirs(&user, Vec::<std::path::PathBuf>::new(), &Limits::default())
            .unwrap();
    let a = entries.get("a.desktop").unwrap();
    for search_path in ["", ":", ".", "sub", ":.:sub:"] {
        let context = Context::new(Desktops::default(), search_path);
        assert_eq!(
            a.eligibility(&context).reasons(),
            [Ineligible::TryExec {
                program: "tool".into(),
                problem: TryExecProblem::NotFound
            }],
            "{search_path:?}"
        );
    }
    // The same directory given absolutely is searched.
    let context = Context::new(Desktops::default(), here.as_os_str());
    assert!(a.eligibility(&context).is_eligible());
}
