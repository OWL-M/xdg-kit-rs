//! Which entries start, against systemd's `systemd-xdg-autostart-generator`
//! and its `systemd-xdg-autostart-condition` (from the dev shell:
//! `XDG_KIT_SYSTEMD`, the pinned systemd).
//!
//! One fixture tree holds a case per behavior. The generator writes a
//! unit for each entry it would start, with an `ExecCondition` for the
//! desktop filter, which is run for each session's desktops; this crate
//! answers with `Autostart::prepare`. Both must agree on every case and
//! session, except the deliberate differences listed in `DIFFERENCES`,
//! each with its reason; a listed difference that no longer happens fails
//! the test too, so the list cannot go stale.

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::Command;

use xdg_kit_autostart::{AutostartEntries, Context, Desktops, Limits};
use xdg_kit_desktop_entry::LaunchOptions;

/// (case, a desktop the session runs or "*" for every session, our
/// answer, reason).
const DIFFERENCES: &[(&str, &str, bool, &str)] = &[
    (
        "emptyonly",
        "*",
        false,
        "an empty OnlyShowIn lists no desktop, as the Desktop Entry crate (and \
         menus) read it; systemd treats it as unset",
    ),
    (
        "kdegroup",
        "*",
        true,
        "the deprecated [KDE Desktop Entry] group is a main group for the Desktop \
         Entry crate; systemd reads only [Desktop Entry]",
    ),
    (
        "plainname",
        "*",
        false,
        "only files named *.desktop are autostart entries; systemd takes every \
         regular file",
    ),
    (
        "legacy",
        "*",
        false,
        "the Desktop Entry spec says to ignore a file in an unsupported Encoding; \
         systemd does not read Encoding",
    ),
    (
        "sdskip",
        "*",
        true,
        "X-systemd-skip is systemd's own extension, not interpreted here",
    ),
    (
        "phase",
        "GNOME",
        true,
        "X-GNOME-Autostart-Phase is a startup-phase extension, not interpreted \
         here; systemd leaves phased entries to GNOME's own startup",
    ),
    (
        "ghostexec",
        "*",
        true,
        "a plan names the program; finding it is the spawn's job (which reports \
         it missing), while systemd checks at generation",
    ),
    (
        "tryempty",
        "*",
        true,
        "the spec checks only a non-empty TryExec; systemd looks for a program \
         with the empty name and refuses the entry",
    ),
    (
        "cwdtry",
        "*",
        false,
        "TryExec may be an absolute path or a bare name only; systemd resolves a \
         relative path against its own working directory",
    ),
];

/// The sessions each case is judged in.
const SESSIONS: &[&[&str]] = &[&["GNOME"], &["KDE"], &[], &["X-Cinnamon", "GNOME"]];

struct Tree {
    root: tempfile::TempDir,
    config: PathBuf,
    high: PathBuf,
    low: PathBuf,
    bin: PathBuf,
    /// The generator's working directory, holding `rel/tool`.
    cwd: PathBuf,
    exec: PathBuf,
}

impl Tree {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        assert!(
            !root.path().as_os_str().as_encoded_bytes().contains(&b':'),
            "TMPDIR contains ':', which XDG_CONFIG_DIRS and PATH cannot hold"
        );
        let at = |path: &str| root.path().join(path);
        let (config, high, low, bin, cwd) =
            (at("config"), at("high"), at("low"), at("bin"), at("cwd"));
        for dir in [
            config.join("autostart"),
            high.join("autostart"),
            low.join("autostart"),
            bin.clone(),
            cwd.join("rel"),
        ] {
            fs::create_dir_all(dir).unwrap();
        }
        for program in [bin.join("tool"), cwd.join("rel/tool")] {
            fs::write(&program, "#!/bin/sh\n").unwrap();
            fs::set_permissions(&program, fs::Permissions::from_mode(0o755)).unwrap();
        }
        let exec = which("true");
        Self {
            root,
            config,
            high,
            low,
            bin,
            cwd,
            exec,
        }
    }

    fn user(&self) -> PathBuf {
        self.config.join("autostart")
    }

    fn high(&self) -> PathBuf {
        self.high.join("autostart")
    }

    fn low(&self) -> PathBuf {
        self.low.join("autostart")
    }

    fn app(&self, extra: &str) -> String {
        format!(
            "[Desktop Entry]\nType=Application\nName=Case\nExec={}\n{extra}",
            self.exec.display()
        )
    }

    fn path_var(&self) -> String {
        // A relative component holding the program: skipped by both.
        format!("{}:rel", self.bin.display())
    }

    /// What systemd starts, case by case and session by session.
    fn systemd(&self) -> BTreeMap<(String, usize), bool> {
        let systemd = PathBuf::from(
            std::env::var_os("XDG_KIT_SYSTEMD")
                .expect("XDG_KIT_SYSTEMD is not set: run the tests in the dev shell (nix develop)"),
        );
        let out = self.root.path().join("units");
        let late = out.join("late");
        for dir in ["normal", "early", "late"] {
            fs::create_dir_all(out.join(dir)).unwrap();
        }
        let status = Command::new(
            systemd.join("lib/systemd/user-generators/systemd-xdg-autostart-generator"),
        )
        .args([out.join("normal"), out.join("early"), late.clone()])
        .env_clear()
        .env("HOME", self.root.path())
        .env("XDG_CONFIG_HOME", &self.config)
        .env(
            "XDG_CONFIG_DIRS",
            format!("{}:{}", self.high.display(), self.low.display()),
        )
        .env("PATH", self.path_var())
        .current_dir(&self.cwd)
        .status()
        .unwrap();
        assert!(status.success());
        let mut started = BTreeMap::new();
        for unit in fs::read_dir(&late).unwrap() {
            let unit = unit.unwrap().path();
            let name = unit.file_name().unwrap().to_str().unwrap().to_owned();
            let Some(case) = name
                .strip_prefix("app-")
                .and_then(|rest| rest.strip_suffix("@autostart.service"))
            else {
                continue;
            };
            let text = fs::read_to_string(&unit).unwrap();
            let condition = text
                .lines()
                .find_map(|line| line.strip_prefix("ExecCondition="))
                .map(|line| {
                    let (program, args) = line.split_once(' ').unwrap();
                    let args: Vec<&str> = args.split('"').skip(1).step_by(2).collect();
                    (PathBuf::from(program), args)
                });
            for (session, desktops) in SESSIONS.iter().enumerate() {
                let passes = condition.as_ref().is_none_or(|(program, args)| {
                    Command::new(program)
                        .args(args)
                        .env_clear()
                        .env("XDG_CURRENT_DESKTOP", desktops.join(":"))
                        .status()
                        .unwrap()
                        .success()
                });
                started.insert((case.to_owned(), session), passes);
            }
        }
        started
    }

    /// What this crate starts.
    fn ours(&self) -> BTreeMap<(String, usize), bool> {
        let entries =
            AutostartEntries::from_dirs(self.user(), [self.high(), self.low()], &Limits::default())
                .unwrap();
        let mut started = BTreeMap::new();
        for entry in entries.iter() {
            let case = entry
                .name()
                .to_str()
                .unwrap()
                .strip_suffix(".desktop")
                .unwrap();
            for (session, desktops) in SESSIONS.iter().enumerate() {
                let context =
                    Context::new(Desktops::new(desktops.iter().copied()), self.path_var());
                let plan = entry.prepare(&context, &LaunchOptions::default());
                started.insert((case.to_owned(), session), plan.is_ok());
            }
        }
        started
    }
}

fn which(name: &str) -> PathBuf {
    std::env::split_paths(&std::env::var_os("PATH").expect("a PATH"))
        .map(|dir| dir.join(name))
        .find(|path| path.is_file())
        .unwrap_or_else(|| panic!("{name} on PATH"))
}

fn put(dir: &Path, name: &str, body: &str) -> PathBuf {
    let path = dir.join(name);
    fs::write(&path, body).unwrap();
    path
}

/// Writes a case per behavior; returns whether this process reads the
/// unreadable fixture anyway (root), which voids that case.
fn populate(tree: &Tree) -> bool {
    let (user, high, low) = (tree.user(), tree.high(), tree.low());
    let plain = tree.app("");
    // Precedence and Hidden.
    put(&high, "plain.desktop", &plain);
    put(&low, "over.desktop", &plain);
    put(&user, "over.desktop", &tree.app("Hidden=true\n"));
    put(&high, "lowhidden.desktop", &plain);
    put(&low, "lowhidden.desktop", &tree.app("Hidden=true\n"));
    put(&high, "hiddenyes.desktop", &tree.app("Hidden=yes\n"));
    put(&high, "hiddenbad.desktop", &tree.app("Hidden=maybe\n"));
    // Desktops.
    put(&high, "onlygnome.desktop", &tree.app("OnlyShowIn=GNOME;\n"));
    put(&high, "notkde.desktop", &tree.app("NotShowIn=KDE;\n"));
    put(
        &high,
        "both.desktop",
        &tree.app("OnlyShowIn=GNOME;\nNotShowIn=X-Cinnamon;\n"),
    );
    put(&high, "emptyonly.desktop", &tree.app("OnlyShowIn=\n"));
    put(&high, "nodisplay.desktop", &tree.app("NoDisplay=true\n"));
    // TryExec.
    put(&high, "tryfound.desktop", &tree.app("TryExec=tool\n"));
    put(&high, "trymissing.desktop", &tree.app("TryExec=absent\n"));
    put(
        &high,
        "tryabs.desktop",
        &tree.app(&format!("TryExec={}\n", tree.bin.join("tool").display())),
    );
    put(&high, "cwdtry.desktop", &tree.app("TryExec=rel/tool\n"));
    put(&high, "tryempty.desktop", &tree.app("TryExec=\n"));
    // Files that are no usable entry.
    put(
        &high,
        "untyped.desktop",
        &format!("[Desktop Entry]\nName=U\nExec={}\n", tree.exec.display()),
    );
    put(
        &high,
        "link.desktop",
        "[Desktop Entry]\nType=Link\nName=L\nURL=https://example.org/\n",
    );
    put(
        &high,
        "noexec.desktop",
        "[Desktop Entry]\nType=Application\nName=N\n",
    );
    put(
        &high,
        "nogroup.desktop",
        "[Something Else]\nType=Application\nExec=x\n",
    );
    put(
        &high,
        "kdegroup.desktop",
        &format!(
            "[KDE Desktop Entry]\nType=Application\nName=K\nExec={}\n",
            tree.exec.display()
        ),
    );
    put(
        &high,
        "legacy.desktop",
        &tree.app("Encoding=Legacy-Mixed\n"),
    );
    put(
        &high,
        "badline.desktop",
        &tree.app("this line has no equals sign\n"),
    );
    put(
        &high,
        "ghostexec.desktop",
        "[Desktop Entry]\nType=Application\nName=G\nExec=no-such-program-xkt\n",
    );
    // Names and non-files.
    put(&high, "plainname", &plain);
    put(&high, ".dotted.desktop", &plain);
    put(&high, "dirover.desktop", &plain);
    fs::create_dir(user.join("dirover.desktop")).unwrap();
    put(&high, "dangling.desktop", &plain);
    symlink(
        tree.root.path().join("nowhere"),
        user.join("dangling.desktop"),
    )
    .unwrap();
    // Extensions neither side interprets alike.
    put(
        &high,
        "gnomeoff.desktop",
        &tree.app("X-GNOME-Autostart-enabled=false\n"),
    );
    put(&high, "sdskip.desktop", &tree.app("X-systemd-skip=true\n"));
    put(
        &high,
        "phase.desktop",
        &tree.app("OnlyShowIn=GNOME;\nX-GNOME-Autostart-Phase=Initialization\n"),
    );
    damaged_overrides(tree)
}
/// The overrides that are damaged: one undecodable, one unreadable. Both
/// mask the system copy on both sides. Returns whether this process reads
/// the unreadable one anyway (root), which voids that case.
fn damaged_overrides(tree: &Tree) -> bool {
    let (user, high) = (tree.user(), tree.high());
    let plain = tree.app("");
    // A Hidden line that is not UTF-8, in a user override.
    put(&high, "undecodable.desktop", &plain);
    fs::write(
        user.join("undecodable.desktop"),
        [tree.app("").as_bytes(), b"Hidden=tr\xffue\n"].concat(),
    )
    .unwrap();
    // An override that cannot be read still masks, on both sides.
    put(&high, "locked.desktop", &plain);
    let locked = put(&user, "locked.desktop", &plain);
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
    fs::read(&locked).is_ok()
}

#[test]
fn which_entries_start_agrees_with_systemd() {
    let tree = Tree::new();
    let root_reads = populate(&tree);

    let (ours, theirs) = (tree.ours(), tree.systemd());
    let mut cases: Vec<&String> = ours
        .keys()
        .chain(theirs.keys())
        .map(|(case, _)| case)
        .collect();
    cases.sort();
    cases.dedup();
    let listed = |case: &str, desktops: &[&str]| {
        DIFFERENCES
            .iter()
            .find(|(name, at, _, _)| *name == case && (*at == "*" || desktops.contains(at)))
    };
    let mut unexpected = Vec::new();
    let mut stale = Vec::new();
    for case in cases {
        if root_reads && case == "locked" {
            continue;
        }
        for (session, desktops) in SESSIONS.iter().enumerate() {
            let key = (case.clone(), session);
            let ours = ours.get(&key).copied().unwrap_or(false);
            let theirs = theirs.get(&key).copied().unwrap_or(false);
            match listed(case, desktops) {
                Some((_, _, expected, _)) => {
                    if ours == theirs || ours != *expected {
                        stale.push(format!(
                            "{case} in {desktops:?}: ours {ours}, systemd {theirs}"
                        ));
                    }
                }
                None if ours != theirs => {
                    unexpected.push(format!(
                        "{case} in {desktops:?}: ours {ours}, systemd {theirs}"
                    ));
                }
                None => {}
            }
        }
    }
    assert!(
        unexpected.is_empty() && stale.is_empty(),
        "unlisted differences:\n{}\n\nlisted differences that no longer happen:\n{}",
        unexpected.join("\n"),
        stale.join("\n")
    );
    // The fixture is not vacuous: both sides start the plain cases.
    assert_eq!(ours.get(&("plain".to_owned(), 0)), Some(&true));
    assert_eq!(theirs.get(&("plain".to_owned(), 0)), Some(&true));
}

#[test]
fn an_unlistable_directory_differs_deliberately() {
    // systemd ignores a directory it cannot open and starts what is
    // behind it; this crate does not start what the unlisted directory
    // might have hidden.
    let tree = Tree::new();
    put(&tree.low(), "behind.desktop", &tree.app(""));
    put(&tree.user(), "mine.desktop", &tree.app(""));
    fs::set_permissions(tree.high(), fs::Permissions::from_mode(0o000)).unwrap();
    let listable = fs::read_dir(tree.high()).is_ok();
    let (ours, theirs) = (tree.ours(), tree.systemd());
    fs::set_permissions(tree.high(), fs::Permissions::from_mode(0o755)).unwrap();
    if listable {
        return;
    }
    let key = |case: &str| (case.to_owned(), 0);
    assert_eq!(ours.get(&key("mine")), Some(&true));
    assert_eq!(theirs.get(&key("mine")), Some(&true));
    assert_eq!(ours.get(&key("behind")), Some(&false));
    assert_eq!(theirs.get(&key("behind")), Some(&true));
}
