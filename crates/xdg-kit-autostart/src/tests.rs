//! Discovery, eligibility, planning and overrides, on fixtures in
//! temporary directories only.

use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};

use xdg_kit_basedir::BaseDirs;
use xdg_kit_desktop_entry::{Desktops, Launch, LaunchOptions};

use crate::{
    AutostartEntries, Context, EditError, Ineligible, Limits, Origin, PrepareError, TryExecProblem,
    WriteError,
};

/// A user directory and two system directories, in precedence order.
struct Dirs {
    _root: tempfile::TempDir,
    user: PathBuf,
    high: PathBuf,
    low: PathBuf,
}

impl Dirs {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let [user, high, low] = ["user", "high", "low"].map(|name| root.path().join(name));
        for dir in [&user, &high, &low] {
            fs::create_dir_all(dir).unwrap();
        }
        Self {
            _root: root,
            user,
            high,
            low,
        }
    }

    fn load(&self) -> AutostartEntries {
        self.load_with(&Limits::default())
    }

    fn load_with(&self, limits: &Limits) -> AutostartEntries {
        AutostartEntries::from_dirs(&self.user, [&self.high, &self.low], limits).unwrap()
    }
}

fn write(dir: &Path, name: &str, body: &str) -> PathBuf {
    let path = dir.join(name);
    fs::write(&path, body).unwrap();
    path
}

fn app(extra: &str) -> String {
    format!("[Desktop Entry]\nType=Application\nName=App\nExec=app\n{extra}")
}

/// No desktops, nothing to search.
fn plain() -> Context {
    Context::new(Desktops::default(), "")
}

fn reasons(entries: &AutostartEntries, name: &str, context: &Context) -> Vec<Ineligible> {
    entries
        .get(name)
        .unwrap_or_else(|| panic!("{name} discovered"))
        .eligibility(context)
        .reasons()
        .to_vec()
}

/// Asserts that `name` starts in `context`, showing why not otherwise.
#[track_caller]
fn assert_eligible(entries: &AutostartEntries, name: &str, context: &Context) {
    assert_eq!(
        reasons(entries, name, context),
        Vec::<Ineligible>::new(),
        "{name}"
    );
}

fn executable(path: &Path) {
    fs::write(path, "#!/bin/sh\n").unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

/// Whether this process ignores permission bits (root), which defeats
/// the unreadable-file fixtures.
fn reads_anything(path: &Path) -> bool {
    fs::read(path).is_ok()
}

mod precedence {
    use super::*;

    #[test]
    fn the_specs_example_uses_the_users_file() {
        // The spec's example: with $XDG_CONFIG_HOME and $XDG_CONFIG_DIRS
        // unset, ~/.config/autostart/foo.desktop beats
        // /etc/xdg/autostart/foo.desktop. The system directory is given
        // here, as $XDG_CONFIG_DIRS, so no real one is read.
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        let etc = root.path().join("etc/xdg");
        fs::create_dir_all(home.join(".config/autostart")).unwrap();
        fs::create_dir_all(etc.join("autostart")).unwrap();
        write(
            &etc.join("autostart"),
            "foo.desktop",
            &app("Comment=system\n"),
        );
        let mine = write(
            &home.join(".config/autostart"),
            "foo.desktop",
            &app("Comment=mine\n"),
        );
        let (home_var, etc_var) = (home.clone(), etc.clone());
        let base = BaseDirs::from_vars(move |name| match name {
            "HOME" => Some(home_var.clone().into_os_string()),
            "XDG_CONFIG_DIRS" => Some(etc_var.clone().into_os_string()),
            _ => None,
        })
        .unwrap();
        let entries = AutostartEntries::load(&base);
        let foo = entries.get("foo.desktop").unwrap();
        assert_eq!(foo.source().path(), mine);
        assert_eq!(foo.source().origin(), Origin::User);
        assert_eq!(
            foo.entry().unwrap().string("Comment").as_deref(),
            Some("mine")
        );
        assert_eq!(foo.shadowed().len(), 1);
        assert_eq!(foo.shadowed()[0].path(), etc.join("autostart/foo.desktop"));
        assert!(foo.is_user_override());
        assert!(foo.eligibility(&plain()).is_eligible());
        assert_eq!(entries.user_dir(), home.join(".config/autostart"));
    }

    #[test]
    fn a_more_important_hidden_file_hides_every_copy() {
        let dirs = Dirs::new();
        write(&dirs.low, "a.desktop", &app(""));
        write(&dirs.high, "a.desktop", &app("Hidden=true\n"));
        write(&dirs.low, "b.desktop", &app("Hidden=true\n"));
        write(&dirs.high, "b.desktop", &app(""));
        write(&dirs.user, "c.desktop", &app("Hidden=true\n"));
        write(&dirs.high, "c.desktop", &app(""));
        write(&dirs.low, "c.desktop", &app(""));
        let entries = dirs.load();
        assert_eq!(
            reasons(&entries, "a.desktop", &plain()),
            [Ineligible::Hidden]
        );
        // A less important Hidden=true is shadowed like everything else.
        assert_eligible(&entries, "b.desktop", &plain());
        let c = entries.get("c.desktop").unwrap();
        assert_eq!(
            reasons(&entries, "c.desktop", &plain()),
            [Ineligible::Hidden]
        );
        // Disabled entries stay inspectable, with every path.
        let paths: Vec<_> = c.shadowed().iter().map(crate::SourceFile::origin).collect();
        assert_eq!(paths, [Origin::System(0), Origin::System(1)]);
        assert_eq!(c.system_default().unwrap().origin(), Origin::System(0));
        let names: Vec<_> = entries.iter().map(|e| e.name().to_owned()).collect();
        assert_eq!(names, ["a.desktop", "b.desktop", "c.desktop"]);
    }

    #[test]
    fn non_files_and_hidden_names_claim_nothing() {
        let dirs = Dirs::new();
        for name in ["dir.desktop", "dangling.desktop", "x.desktop"] {
            write(&dirs.high, name, &app(""));
        }
        fs::create_dir(dirs.user.join("dir.desktop")).unwrap();
        symlink(
            dirs.user.join("nowhere"),
            dirs.user.join("dangling.desktop"),
        )
        .unwrap();
        // Hidden names, names without the suffix, the bare suffix.
        for name in [".x.desktop", "x.desktop~", "notes", ".desktop"] {
            write(&dirs.user, name, &app("Hidden=true\n"));
        }
        let entries = dirs.load();
        for name in ["dir.desktop", "dangling.desktop", "x.desktop"] {
            let entry = entries.get(name).unwrap();
            assert_eq!(entry.source().origin(), Origin::System(0), "{name}");
            assert!(entry.eligibility(&plain()).is_eligible(), "{name}");
        }
        assert_eq!(entries.iter().count(), 3);
    }

    #[test]
    fn a_symlink_to_a_file_is_that_file() {
        let dirs = Dirs::new();
        let target = write(&dirs.low, "real.desktop", &app("Comment=linked\n"));
        symlink(&target, dirs.user.join("linked.desktop")).unwrap();
        let entries = dirs.load();
        let linked = entries.get("linked.desktop").unwrap();
        assert_eq!(linked.source().origin(), Origin::User);
        assert_eq!(
            linked.entry().unwrap().string("Comment").as_deref(),
            Some("linked")
        );
    }

    #[test]
    fn refresh_sees_new_changed_and_removed_files() {
        let dirs = Dirs::new();
        let system = write(&dirs.high, "a.desktop", &app(""));
        let first = dirs.load();
        assert!(first.refresh().is_none(), "nothing changed");
        // A new override in the user's directory.
        write(&dirs.user, "a.desktop", &app("Hidden=true\n"));
        let second = first.refresh().expect("a new file");
        assert_eq!(
            reasons(&second, "a.desktop", &plain()),
            [Ineligible::Hidden]
        );
        assert!(second.refresh().is_none());
        // An in-place edit of a system file, the directory untouched.
        fs::write(&system, app("Comment=edited\n")).unwrap();
        let third = second.refresh().expect("an edited file");
        assert_eq!(
            third.get("a.desktop").unwrap().shadowed()[0]
                .entry()
                .unwrap()
                .string("Comment")
                .as_deref(),
            Some("edited")
        );
        // The override removed: the system file is back in effect.
        fs::remove_file(dirs.user.join("a.desktop")).unwrap();
        let fourth = third.refresh().expect("a removed file");
        assert_eligible(&fourth, "a.desktop", &plain());
    }

    #[test]
    fn a_repeated_directory_is_read_once() {
        let dirs = Dirs::new();
        write(&dirs.user, "a.desktop", &app(""));
        let entries = AutostartEntries::from_dirs(
            &dirs.user,
            [dirs.user.clone(), dirs.high.clone(), dirs.high.clone()],
            &Limits::default(),
        )
        .unwrap();
        assert_eq!(entries.system_dirs(), std::slice::from_ref(&dirs.high));
        assert_eq!(entries.get("a.desktop").unwrap().shadowed(), []);
    }
}

mod eligibility {
    use super::*;

    #[test]
    fn no_display_does_not_stop_autostart() {
        let dirs = Dirs::new();
        write(&dirs.high, "a.desktop", &app("NoDisplay=true\n"));
        let entries = dirs.load();
        assert_eligible(&entries, "a.desktop", &plain());
    }

    #[test]
    fn desktops_filter_as_the_desktop_entry_spec_says() {
        let dirs = Dirs::new();
        write(&dirs.high, "only.desktop", &app("OnlyShowIn=GNOME;KDE;\n"));
        write(&dirs.high, "not.desktop", &app("NotShowIn=XFCE;\n"));
        write(&dirs.high, "empty.desktop", &app("OnlyShowIn=\n"));
        let entries = dirs.load();
        let on = |desktops: &[&str]| Context::new(Desktops::new(desktops.iter().copied()), "");
        assert_eligible(&entries, "only.desktop", &on(&["KDE"]));
        assert_eligible(&entries, "only.desktop", &on(&["ubuntu", "GNOME"]));
        assert_eq!(
            reasons(&entries, "only.desktop", &on(&["XFCE"])),
            [Ineligible::OnlyShowIn {
                listed: vec!["GNOME".into(), "KDE".into()]
            }]
        );
        assert_eq!(
            reasons(&entries, "only.desktop", &plain()),
            [Ineligible::OnlyShowIn {
                listed: vec!["GNOME".into(), "KDE".into()]
            }],
            "no desktop matches nothing"
        );
        assert_eq!(
            reasons(&entries, "not.desktop", &on(&["X-Cinnamon", "XFCE"])),
            [Ineligible::NotShowIn {
                desktop: "XFCE".into()
            }]
        );
        assert_eligible(&entries, "not.desktop", &plain());
        // An empty OnlyShowIn lists nothing, as the Desktop Entry crate
        // reads it.
        assert_eq!(
            reasons(&entries, "empty.desktop", &on(&["GNOME"])),
            [Ineligible::OnlyShowIn { listed: vec![] }]
        );
    }

    #[test]
    fn the_verdict_agrees_with_the_desktop_entry_rule() {
        use std::fmt::Write as _;
        // Every combination of a two-desktop session against both lists.
        let dirs = Dirs::new();
        let lists = ["", "A;", "B;", "A;B;"];
        let mut cases = Vec::new();
        for (o, only) in lists.iter().enumerate() {
            for (n, not) in lists.iter().enumerate() {
                let mut extra = String::new();
                if !only.is_empty() {
                    let _ = writeln!(extra, "OnlyShowIn={only}");
                }
                if !not.is_empty() {
                    let _ = writeln!(extra, "NotShowIn={not}");
                }
                let name = format!("c{o}{n}.desktop");
                write(&dirs.high, &name, &app(&extra));
                cases.push(name);
            }
        }
        let entries = dirs.load();
        for session in [vec![], vec!["A"], vec!["B"], vec!["A", "B"], vec!["B", "A"]] {
            let context = Context::new(Desktops::new(session.clone()), "");
            for name in &cases {
                let entry = entries.get(name).unwrap();
                let shown = entry.entry().unwrap().shown_in(context.desktops());
                assert_eq!(
                    entry.eligibility(&context).is_eligible(),
                    shown,
                    "{name} in {session:?}"
                );
            }
        }
    }

    #[test]
    fn try_exec_follows_the_autostart_spec() {
        let dirs = Dirs::new();
        let bin = dirs.low.join("bin");
        fs::create_dir(&bin).unwrap();
        executable(&bin.join("tool"));
        fs::write(bin.join("plain"), "not executable").unwrap();
        fs::create_dir(bin.join("folder")).unwrap();
        let tool = bin.join("tool");
        let cases = [
            ("bare", "tool".to_owned(), None),
            (
                "missing",
                "absent".to_owned(),
                Some(TryExecProblem::NotFound),
            ),
            ("absolute", tool.display().to_string(), None),
            (
                "absolute-missing",
                bin.join("absent").display().to_string(),
                Some(TryExecProblem::NotFound),
            ),
            (
                "not-executable",
                "plain".to_owned(),
                Some(TryExecProblem::NotFound),
            ),
            (
                "a-folder",
                "folder".to_owned(),
                Some(TryExecProblem::NotFound),
            ),
            (
                "relative",
                "bin/tool".to_owned(),
                Some(TryExecProblem::PathComponents),
            ),
        ];
        for (name, program, _) in &cases {
            write(
                &dirs.high,
                &format!("{name}.desktop"),
                &app(&format!("TryExec={program}\n")),
            );
        }
        // `Path=` is not consulted: the bare name in it is not found.
        write(
            &dirs.high,
            "working-dir.desktop",
            &app(&format!("TryExec=tool\nPath={}\n", bin.display())),
        );
        write(&dirs.high, "empty.desktop", &app("TryExec=\n"));
        let entries = dirs.load();
        let searched = Context::new(
            Desktops::default(),
            format!("relative:/nowhere:{}", bin.display()),
        );
        for (name, program, problem) in cases {
            let expected: Vec<_> = problem
                .map(|problem| Ineligible::TryExec { program, problem })
                .into_iter()
                .collect();
            assert_eq!(
                reasons(&entries, &format!("{name}.desktop"), &searched),
                expected,
                "{name}"
            );
        }
        assert_eq!(
            reasons(&entries, "working-dir.desktop", &plain()),
            [Ineligible::TryExec {
                program: "tool".into(),
                problem: TryExecProblem::NotFound
            }]
        );
        assert_eligible(&entries, "empty.desktop", &plain());
    }

    #[test]
    fn every_failing_check_is_reported() {
        let dirs = Dirs::new();
        write(
            &dirs.high,
            "all.desktop",
            &app("Hidden=true\nOnlyShowIn=KDE;\nTryExec=absent\n"),
        );
        let entries = dirs.load();
        assert_eq!(
            reasons(&entries, "all.desktop", &plain()),
            [
                Ineligible::Hidden,
                Ineligible::OnlyShowIn {
                    listed: vec!["KDE".into()]
                },
                Ineligible::TryExec {
                    program: "absent".into(),
                    problem: TryExecProblem::NotFound
                },
            ]
        );
    }
}

mod malformed {
    use super::*;

    #[test]
    fn an_undecodable_line_fails_closed_and_stays_editable() {
        let dirs = Dirs::new();
        write(&dirs.high, "a.desktop", &app(""));
        let mine = b"[Desktop Entry]\nType=Application\nExec=app\nHidden=tr\xffue\n";
        fs::write(dirs.user.join("a.desktop"), mine).unwrap();
        let entries = dirs.load();
        assert_eq!(
            reasons(&entries, "a.desktop", &plain()),
            [Ineligible::Undecodable { line: 4 }]
        );
        assert!(matches!(
            entries
                .get("a.desktop")
                .unwrap()
                .prepare(&plain(), &LaunchOptions::default()),
            Err(PrepareError::Ineligible(_))
        ));
        // The bytes stay whole: an edit keeps the undecodable line (the
        // new key goes after the group's last readable entry).
        let mut edit = entries.edit("a.desktop").unwrap();
        edit.disable().unwrap();
        assert_eq!(
            edit.to_bytes().unwrap(),
            b"[Desktop Entry]\nType=Application\nExec=app\nHidden=true\nHidden=tr\xffue\n"
        );
        // A comment that is not UTF-8 is no dropped line.
        fs::write(
            dirs.user.join("a.desktop"),
            b"# caf\xe9\n[Desktop Entry]\nType=Application\nExec=a\n",
        )
        .unwrap();
        assert_eligible(&dirs.load(), "a.desktop", &plain());
    }

    #[test]
    fn broken_overrides_fail_closed() {
        let dirs = Dirs::new();
        for name in ["group", "bool", "big", "encoding", "garbage"] {
            write(&dirs.high, &format!("{name}.desktop"), &app(""));
        }
        write(&dirs.user, "group.desktop", "[Other Group]\nHidden=false\n");
        write(&dirs.user, "bool.desktop", &app("Hidden=yes\n"));
        write(
            &dirs.user,
            "big.desktop",
            &app(&"X-Pad=padding\n".repeat(100)),
        );
        write(
            &dirs.user,
            "encoding.desktop",
            &app("Encoding=Legacy-Mixed\n"),
        );
        // Lines that are no key file at all: the lenient parse keeps the
        // valid ones, so the entry stands (with diagnostics).
        write(
            &dirs.user,
            "garbage.desktop",
            &app("no equals sign\n\u{1}\n"),
        );
        let limits = Limits {
            max_file_len: Some(512),
            ..Limits::default()
        };
        let entries = dirs.load_with(&limits);
        let only = |name: &str| reasons(&entries, name, &plain());
        assert_eq!(only("group.desktop"), [Ineligible::NoMainGroup]);
        assert_eq!(
            only("bool.desktop"),
            [Ineligible::InvalidHidden {
                value: "yes".into()
            }]
        );
        assert_eq!(only("big.desktop"), [Ineligible::Unreadable]);
        let big = entries.get("big.desktop").unwrap();
        assert_eq!(
            big.source().problem().unwrap().kind(),
            std::io::ErrorKind::FileTooLarge
        );
        assert_eq!(only("encoding.desktop"), [Ineligible::UnsupportedEncoding]);
        let garbage = entries.get("garbage.desktop").unwrap();
        assert_eq!(only("garbage.desktop"), Vec::<Ineligible>::new());
        assert_ne!(garbage.entry().unwrap().key_file().diagnostics(), []);
        // Each still hides the system copy behind it.
        for entry in entries.iter() {
            assert_eq!(entry.source().origin(), Origin::User, "{:?}", entry.name());
            assert_eq!(entry.shadowed().len(), 1);
        }
        // The deprecated KDE group is a main group.
        write(
            &dirs.low,
            "kde.desktop",
            "[KDE Desktop Entry]\nType=Application\nExec=x\n",
        );
        assert_eligible(&dirs.load(), "kde.desktop", &plain());
    }

    #[test]
    fn an_unreadable_override_still_hides() {
        let dirs = Dirs::new();
        write(&dirs.high, "a.desktop", &app(""));
        let mine = write(&dirs.user, "a.desktop", &app("Hidden=true\n"));
        fs::set_permissions(&mine, fs::Permissions::from_mode(0o000)).unwrap();
        if reads_anything(&mine) {
            return;
        }
        let entries = dirs.load();
        let a = entries.get("a.desktop").unwrap();
        assert_eq!(a.source().origin(), Origin::User);
        assert_eq!(
            a.source().problem().unwrap().kind(),
            std::io::ErrorKind::PermissionDenied
        );
        assert_eq!(
            reasons(&entries, "a.desktop", &plain()),
            [Ineligible::Unreadable]
        );
        assert!(
            entries
                .diagnostics()
                .iter()
                .any(|d| d.path.as_deref() == Some(mine.as_path())),
            "{:?}",
            entries.diagnostics()
        );
    }

    #[test]
    fn an_unlistable_directory_makes_what_it_could_hide_uncertain() {
        let dirs = Dirs::new();
        write(&dirs.user, "mine.desktop", &app(""));
        write(&dirs.high, "high.desktop", &app(""));
        write(&dirs.low, "low.desktop", &app(""));
        fs::set_permissions(&dirs.high, fs::Permissions::from_mode(0o000)).unwrap();
        let listable = fs::read_dir(&dirs.high).is_ok();
        let entries = dirs.load();
        fs::set_permissions(&dirs.high, fs::Permissions::from_mode(0o755)).unwrap();
        if listable {
            return;
        }
        assert_eligible(&entries, "mine.desktop", &plain());
        assert!(entries.get("high.desktop").is_none());
        assert_eq!(
            reasons(&entries, "low.desktop", &plain()),
            [Ineligible::OverridesUnknown {
                dir: dirs.high.clone()
            }]
        );
    }
}

mod limits {
    use super::*;

    #[test]
    fn every_listed_entry_counts_and_the_budget_stops_listing() {
        let dirs = Dirs::new();
        write(&dirs.user, "a.desktop", &app(""));
        for name in ["notes", "README", "backup~"] {
            write(&dirs.user, name, "");
        }
        write(&dirs.high, "s.desktop", &app(""));
        let limits = Limits {
            max_listed: 3,
            ..Limits::default()
        };
        let entries = dirs.load_with(&limits);
        assert!(
            entries.get("a.desktop").is_none(),
            "unrelated names count too"
        );
        assert_eq!(
            reasons(&entries, "s.desktop", &plain()),
            [Ineligible::OverridesUnknown {
                dir: dirs.user.clone()
            }]
        );
        // Over the candidate budget, less important directories are not
        // even listed: an unlistable one there reports nothing.
        let fresh = Dirs::new();
        for name in ["a.desktop", "b.desktop"] {
            write(&fresh.user, name, &app(""));
        }
        fs::set_permissions(&fresh.low, fs::Permissions::from_mode(0o000)).unwrap();
        let tight = Limits {
            max_entries: 1,
            ..Limits::default()
        };
        let cut = fresh.load_with(&tight);
        fs::set_permissions(&fresh.low, fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(
            cut.diagnostics().len(),
            1,
            "only the cut: {:?}",
            cut.diagnostics()
        );
    }

    #[test]
    fn an_over_full_directory_is_skipped_and_hides_what_follows() {
        let dirs = Dirs::new();
        for at in 0..3 {
            write(&dirs.user, &format!("u{at}.desktop"), &app(""));
        }
        write(&dirs.high, "s.desktop", &app(""));
        let limits = Limits {
            max_listed: 2,
            ..Limits::default()
        };
        let entries = dirs.load_with(&limits);
        assert!(entries.get("u0.desktop").is_none(), "the whole directory");
        assert_eq!(
            reasons(&entries, "s.desktop", &plain()),
            [Ineligible::OverridesUnknown {
                dir: dirs.user.clone()
            }]
        );
        assert!(matches!(
            entries.edit("s.desktop"),
            Err(EditError::UserDirUnknown(_))
        ));
    }

    #[test]
    fn the_budget_cuts_in_precedence_order_and_bounds_what_is_kept() {
        let dirs = Dirs::new();
        write(&dirs.user, "b.desktop", &app("Hidden=true\n"));
        write(&dirs.user, "a.desktop", &app(""));
        write(&dirs.high, "b.desktop", &app(""));
        write(&dirs.high, "c.desktop", &app(""));
        let limits = Limits {
            max_entries: 2,
            ..Limits::default()
        };
        let entries = dirs.load_with(&limits);
        // The user's two files fit; the system ones are not considered,
        // so the hidden b cannot be un-hidden and c is simply absent.
        let names: Vec<_> = entries.iter().map(|e| e.name().to_owned()).collect();
        assert_eq!(names, ["a.desktop", "b.desktop"]);
        assert_eq!(entries.get("b.desktop").unwrap().shadowed(), []);
        assert_eq!(
            entries.diagnostics().len(),
            1,
            "{:?}",
            entries.diagnostics()
        );
    }

    #[test]
    fn discovery_is_linear_in_the_files() {
        crate::work::assert_linear(
            300,
            |n| {
                let dirs = Dirs::new();
                let mut bytes = 0;
                for at in 0..n {
                    let name = format!("app-{at:05}.desktop");
                    let body = app("Comment=a fixture\n");
                    bytes += 2 * (name.len() + body.len());
                    write(&dirs.user, &name, &body);
                    write(&dirs.high, &name, &body);
                }
                let (entries, steps) = crate::work::steps(|| dirs.load());
                assert_eq!(entries.iter().count(), n);
                (steps, bytes)
            },
            // Per file: the listing, the sort (a log factor of name
            // comparisons, small at these sizes), the path and stamp, the
            // read and the key file's parse.
            |n, bytes| 2 * (200 * n + 30 * bytes),
        );
    }
}

mod planning {
    use super::*;

    #[test]
    fn plans_know_the_file_they_came_from() {
        let dirs = Dirs::new();
        let path = write(&dirs.high, "located.desktop", &app("Exec=tool %k\n"));
        let entries = dirs.load();
        let entry = entries.get("located.desktop").unwrap();
        assert_eq!(entry.entry().unwrap().path(), Some(path.as_path()));
        let plan = entry.prepare(&plain(), &LaunchOptions::default()).unwrap();
        let Launch::Commands(commands) = plan.launch else {
            panic!("a command")
        };
        assert_eq!(commands[0].args, [path.as_os_str()], "%k is the file");
    }

    /// The bus name of a D-Bus activation is the file's name.
    #[cfg(feature = "dbus")]
    #[test]
    fn a_dbus_activatable_entry_plans_a_call() {
        let dirs = Dirs::new();
        write(
            &dirs.high,
            "org.example.Bus.desktop",
            "[Desktop Entry]\nType=Application\nName=B\nDBusActivatable=true\n",
        );
        let entries = dirs.load();
        let mut options = LaunchOptions::default();
        options.dbus = true;
        let plan = entries
            .get("org.example.Bus.desktop")
            .unwrap()
            .prepare(&plain(), &options)
            .unwrap();
        let Launch::DBus(call) = plan.launch else {
            panic!("a D-Bus call, not {:?}", plan.launch)
        };
        assert_eq!(call.bus_name, "org.example.Bus");
    }

    #[test]
    fn eligibility_and_launch_failures_stay_apart() {
        let dirs = Dirs::new();
        write(&dirs.high, "ok.desktop", &app("Exec=tool --flag\n"));
        write(&dirs.high, "hidden.desktop", &app("Hidden=true\n"));
        write(
            &dirs.high,
            "no-exec.desktop",
            "[Desktop Entry]\nType=Application\nName=A\n",
        );
        write(
            &dirs.high,
            "link.desktop",
            "[Desktop Entry]\nType=Link\nName=L\nURL=https://example.org/\n",
        );
        write(
            &dirs.high,
            "untyped.desktop",
            "[Desktop Entry]\nName=A\nExec=a\n",
        );
        let entries = dirs.load();
        let options = LaunchOptions::default();
        let plan = entries
            .get("ok.desktop")
            .unwrap()
            .prepare(&plain(), &options)
            .unwrap();
        let Launch::Commands(commands) = plan.launch else {
            panic!("a command")
        };
        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0].args, ["--flag"]);
        assert!(matches!(
            entries
                .get("hidden.desktop")
                .unwrap()
                .prepare(&plain(), &options),
            Err(PrepareError::Ineligible(_))
        ));
        for name in ["no-exec.desktop", "link.desktop", "untyped.desktop"] {
            let err = entries
                .get(name)
                .unwrap()
                .prepare(&plain(), &options)
                .unwrap_err();
            assert!(matches!(err, PrepareError::Launch(_)), "{name}: {err}");
        }
    }

    #[test]
    fn an_action_in_the_options_is_ignored() {
        let dirs = Dirs::new();
        write(
            &dirs.high,
            "a.desktop",
            &app("Actions=other;\n[Desktop Action other]\nName=O\nExec=other\n"),
        );
        let entries = dirs.load();
        let mut options = LaunchOptions::default();
        options.action = Some("other".into());
        let plan = entries
            .get("a.desktop")
            .unwrap()
            .prepare(&plain(), &options)
            .unwrap();
        let Launch::Commands(commands) = plan.launch else {
            panic!("a command")
        };
        assert_eq!(commands[0].program, "app");
    }
}

mod overrides {
    use super::*;

    #[test]
    fn enabling_a_richer_user_file_keeps_everything_in_it() {
        let dirs = Dirs::new();
        write(&dirs.high, "a.desktop", SYSTEM);
        let mine = "# Keep this configuration\n[Desktop Entry]\nHidden=true\nOnlyShowIn=KDE;\n\
                    X-Custom=keep\nExec=my-tool\n";
        write(&dirs.user, "a.desktop", mine);
        let entries = dirs.load();
        let mut edit = entries.edit("a.desktop").unwrap();
        edit.enable().unwrap();
        let _ = edit.write().unwrap();
        assert_eq!(
            fs::read_to_string(dirs.user.join("a.desktop")).unwrap(),
            mine.replace("Hidden=true", "Hidden=false"),
            "edited in place, every line kept"
        );
        // The user's restriction stays in force; the file is the user's,
        // incomplete as it is (no Type), so it still cannot be launched.
        let after = entries.refresh().unwrap();
        let gnome = Context::new(Desktops::new(["GNOME"]), "");
        let a = after.get("a.desktop").unwrap();
        assert_eq!(
            a.eligibility(&gnome).reasons(),
            [Ineligible::OnlyShowIn {
                listed: vec!["KDE".into()]
            }]
        );
        let kde = Context::new(Desktops::new(["KDE"]), "");
        assert!(matches!(
            a.prepare(&kde, &LaunchOptions::default()),
            Err(PrepareError::Launch(_))
        ));
        // Discarding it for the system file is a separate, explicit step.
        let mut restore = after.edit("a.desktop").unwrap();
        restore.reset().unwrap();
        restore.enable().unwrap();
        assert_eq!(
            restore.to_bytes().unwrap(),
            format!("{SYSTEM}Hidden=false\n").as_bytes()
        );
    }

    #[test]
    fn a_damaged_override_is_never_taken_for_a_minimal_one() {
        // Lines the parser drops — not UTF-8, or not readable as an entry —
        // could have said anything: such a file is edited in place, its
        // bytes kept, and stays as blocked as before.
        let dirs = Dirs::new();
        write(&dirs.high, "a.desktop", SYSTEM);
        write(&dirs.high, "b.desktop", SYSTEM);
        let undecodable = b"[Desktop Entry]\nHidden=true\nOnlyShowIn=KD\xffE;\n".to_vec();
        fs::write(dirs.user.join("a.desktop"), &undecodable).unwrap();
        let unparsable = "[Desktop Entry]\nHidden=true\nOnlyShowIn KDE;\n";
        write(&dirs.user, "b.desktop", unparsable);
        let entries = dirs.load();
        assert_eq!(
            reasons(&entries, "a.desktop", &plain()),
            [Ineligible::Undecodable { line: 3 }]
        );
        for (name, before) in [
            ("a.desktop", undecodable),
            ("b.desktop", unparsable.as_bytes().to_vec()),
        ] {
            let mut edit = entries.edit(name).unwrap();
            edit.enable().unwrap();
            let after = edit.to_bytes().unwrap();
            assert_ne!(
                after,
                format!("{SYSTEM}Hidden=false\n").as_bytes(),
                "{name}: replaced"
            );
            let kept = String::from_utf8_lossy(&before).replace("Hidden=true", "Hidden=false");
            assert_eq!(
                String::from_utf8_lossy(&after),
                kept,
                "{name}: edited in place"
            );
            let _ = edit.write().unwrap();
        }
        let after = entries.refresh().unwrap();
        let gnome = Context::new(Desktops::new(["GNOME"]), "");
        assert_eq!(
            reasons(&after, "a.desktop", &gnome),
            [Ineligible::Undecodable { line: 3 }],
            "still blocked"
        );
        assert!(
            after
                .get("b.desktop")
                .unwrap()
                .prepare(&gnome, &LaunchOptions::default())
                .is_err(),
            "the user's damaged file, not the system entry"
        );
    }

    #[test]
    fn enabling_a_minimal_override_brings_the_system_entry_back() {
        let dirs = Dirs::new();
        write(&dirs.high, "a.desktop", SYSTEM);
        // The spec's minimal override; a comment is all it holds besides.
        write(
            &dirs.user,
            "a.desktop",
            "# off\n[Desktop Entry]\nHidden=true\n",
        );
        let entries = dirs.load();
        assert_eq!(
            reasons(&entries, "a.desktop", &plain()),
            [Ineligible::Hidden]
        );
        let mut edit = entries.edit("a.desktop").unwrap();
        edit.enable().unwrap();
        assert_eq!(
            edit.to_bytes().unwrap(),
            format!("{SYSTEM}Hidden=false\n").as_bytes(),
            "the system file's copy, not the stub"
        );
        let _ = edit.write().unwrap();
        let after = entries.refresh().unwrap();
        assert!(
            after
                .get("a.desktop")
                .unwrap()
                .prepare(&plain(), &LaunchOptions::default())
                .is_ok()
        );
        // With no system file, the stub itself is what there is.
        write(&dirs.user, "own.desktop", "[Desktop Entry]\nHidden=true\n");
        let mut own = dirs.load().edit("own.desktop").unwrap();
        own.enable().unwrap();
        assert_eq!(own.to_bytes().unwrap(), b"[Desktop Entry]\nHidden=false\n");
    }

    const SYSTEM: &str = "# shipped\n[Desktop Entry]\nType=Application\nName=App\nExec=app\n\
        X-GNOME-Autostart-Phase=Applications\n";

    #[test]
    fn disabling_copies_the_default_losslessly_and_never_touches_it() {
        let dirs = Dirs::new();
        fs::remove_dir(&dirs.user).unwrap();
        let system = write(&dirs.high, "a.desktop", SYSTEM);
        let entries = dirs.load();
        let mut edit = entries.edit("a.desktop").unwrap();
        edit.disable().unwrap();
        let expected = format!("{SYSTEM}Hidden=true\n");
        assert_eq!(edit.to_bytes().unwrap(), expected.as_bytes());
        let _ = edit.write().unwrap();
        let mode = |path: &Path| fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&dirs.user), 0o700, "a fresh directory is private");
        assert_eq!(mode(&dirs.user.join("a.desktop")), 0o644);
        assert_eq!(
            fs::read_to_string(dirs.user.join("a.desktop")).unwrap(),
            expected
        );
        assert_eq!(fs::read_to_string(&system).unwrap(), SYSTEM, "untouched");
        let after = entries.refresh().expect("the override appeared");
        assert_eq!(reasons(&after, "a.desktop", &plain()), [Ineligible::Hidden]);
        assert!(after.get("a.desktop").unwrap().is_user_override());
    }

    #[test]
    fn editing_the_users_file_changes_only_hidden() {
        let dirs = Dirs::new();
        write(&dirs.high, "a.desktop", SYSTEM);
        let mine = "[Desktop Entry]\r\n# keep me\r\nType=Application\r\nExec=mine\r\nHidden=false\r\nX-Unknown=1\r\n";
        write(&dirs.user, "a.desktop", mine);
        let entries = dirs.load();
        let mut edit = entries.edit("a.desktop").unwrap();
        edit.disable().unwrap();
        let _ = edit.write().unwrap();
        assert_eq!(
            fs::read_to_string(dirs.user.join("a.desktop")).unwrap(),
            mine.replace("Hidden=false", "Hidden=true")
        );
        // Enabled again, without a new snapshot: the override moved on.
        edit.enable().unwrap();
        let _ = edit.write().unwrap();
        assert_eq!(
            fs::read_to_string(dirs.user.join("a.desktop")).unwrap(),
            mine
        );
    }

    #[test]
    fn explicit_enablement_and_reset_differ() {
        let dirs = Dirs::new();
        let system = write(&dirs.high, "a.desktop", &app("Hidden=true\n"));
        let entries = dirs.load();
        assert_eq!(
            reasons(&entries, "a.desktop", &plain()),
            [Ineligible::Hidden]
        );
        let mut edit = entries.edit("a.desktop").unwrap();
        edit.enable().unwrap();
        let _ = edit.write().unwrap();
        let enabled = entries.refresh().unwrap();
        assert_eligible(&enabled, "a.desktop", &plain());
        assert!(
            fs::read_to_string(dirs.user.join("a.desktop"))
                .unwrap()
                .contains("Hidden=false")
        );
        // The reset removes the pin: the system default (hidden) is back.
        let mut reset = enabled.edit("a.desktop").unwrap();
        assert!(reset.has_default());
        reset.reset().unwrap();
        assert!(matches!(reset.write().unwrap(), crate::Durability::Synced));
        assert!(!dirs.user.join("a.desktop").exists());
        let back = enabled.refresh().unwrap();
        assert_eq!(reasons(&back, "a.desktop", &plain()), [Ineligible::Hidden]);
        assert_eq!(fs::read_to_string(&system).unwrap(), app("Hidden=true\n"));
        // Resetting with no user file does nothing.
        let mut again = back.edit("a.desktop").unwrap();
        again.reset().unwrap();
        let _ = again.write().unwrap();
        assert!(!dirs.user.join("a.desktop").exists());
    }

    #[test]
    fn what_cannot_be_reset_or_enabled() {
        let dirs = Dirs::new();
        write(&dirs.user, "own.desktop", &app(""));
        let unreadable = write(&dirs.high, "locked.desktop", &app(""));
        fs::set_permissions(&unreadable, fs::Permissions::from_mode(0o000)).unwrap();
        let entries = dirs.load();
        let mut own = entries.edit("own.desktop").unwrap();
        assert!(!own.has_default());
        assert_eq!(own.reset(), Err(EditError::NoDefault));
        assert!(matches!(
            entries.edit("nope.desktop"),
            Err(EditError::NotFound(_))
        ));
        for bad in ["a/b.desktop", ".a.desktop", "a.txt", ".desktop"] {
            assert!(
                matches!(entries.edit(bad), Err(EditError::BadName(_))),
                "{bad}"
            );
        }
        if reads_anything(&unreadable) {
            return;
        }
        let mut locked = entries.edit("locked.desktop").unwrap();
        assert_eq!(locked.enable(), Err(EditError::NothingToEnable));
        // Disabling needs no copy: the spec's minimal override.
        locked.disable().unwrap();
        assert_eq!(
            locked.to_bytes().unwrap(),
            b"[Desktop Entry]\nHidden=true\n"
        );
    }

    #[test]
    fn concurrent_changes_are_conflicts() {
        let dirs = Dirs::new();
        write(&dirs.high, "a.desktop", SYSTEM);
        write(&dirs.user, "a.desktop", &app("Comment=one\n"));
        let entries = dirs.load();
        let mut edit = entries.edit("a.desktop").unwrap();
        edit.disable().unwrap();
        // Same size, same tick: only the bytes can tell.
        write(&dirs.user, "a.desktop", &app("Comment=two\n"));
        assert!(matches!(edit.write(), Err(WriteError::Conflict)));
        assert!(edit.to_bytes().is_some(), "the change stays pending");
        let mut reset = entries.edit("a.desktop").unwrap();
        reset.reset().unwrap();
        assert!(matches!(reset.write(), Err(WriteError::Conflict)));
        assert!(
            fs::read_to_string(dirs.user.join("a.desktop"))
                .unwrap()
                .contains("two"),
            "nothing replaced or removed"
        );
        // The named escape hatch wins.
        let _ = edit.write_overwriting().unwrap();
        assert!(
            fs::read_to_string(dirs.user.join("a.desktop"))
                .unwrap()
                .contains("Hidden=true")
        );
        // A competing creator of a free name is a conflict too.
        write(&dirs.high, "b.desktop", SYSTEM);
        let fresh = dirs.load();
        let mut create = fresh.edit("b.desktop").unwrap();
        create.disable().unwrap();
        write(&dirs.user, "b.desktop", "theirs");
        assert!(matches!(create.write(), Err(WriteError::Conflict)));
        assert_eq!(
            fs::read_to_string(dirs.user.join("b.desktop")).unwrap(),
            "theirs"
        );
    }

    #[test]
    fn links_in_the_users_directory_are_replaced_not_followed() {
        let dirs = Dirs::new();
        let system = write(&dirs.high, "a.desktop", SYSTEM);
        // A user "override" that is a link to the system file itself.
        symlink(&system, dirs.user.join("a.desktop")).unwrap();
        let entries = dirs.load();
        let mut edit = entries.edit("a.desktop").unwrap();
        edit.disable().unwrap();
        let _ = edit.write().unwrap();
        assert_eq!(
            fs::read_to_string(&system).unwrap(),
            SYSTEM,
            "never written through"
        );
        let written = dirs.user.join("a.desktop");
        assert!(fs::symlink_metadata(&written).unwrap().is_file());
        assert!(
            fs::read_to_string(&written)
                .unwrap()
                .ends_with("Hidden=true\n")
        );
        // A dangling link in the way: only a deliberate overwrite.
        write(&dirs.high, "b.desktop", SYSTEM);
        symlink(dirs.user.join("gone"), dirs.user.join("b.desktop")).unwrap();
        let entries = dirs.load();
        let mut blocked = entries.edit("b.desktop").unwrap();
        blocked.disable().unwrap();
        assert!(matches!(blocked.write(), Err(WriteError::Conflict)));
        let _ = blocked.write_overwriting().unwrap();
        assert!(
            fs::symlink_metadata(dirs.user.join("b.desktop"))
                .unwrap()
                .is_file()
        );
        assert!(
            !dirs.user.join("gone").exists(),
            "the link's target was not created"
        );
    }

    #[test]
    fn an_override_over_the_limits_is_refused() {
        let dirs = Dirs::new();
        write(&dirs.high, "a.desktop", SYSTEM);
        let limits = Limits {
            max_file_len: Some(SYSTEM.len() as u64),
            ..Limits::default()
        };
        let entries = dirs.load_with(&limits);
        let mut edit = entries.edit("a.desktop").unwrap();
        edit.disable().unwrap();
        assert!(matches!(edit.write(), Err(WriteError::OverLimits)));
        assert!(!dirs.user.join("a.desktop").exists());
    }
}
