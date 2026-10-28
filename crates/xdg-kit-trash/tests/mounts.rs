//! Behavior across real mounts: trash selection by mount, a bind mount of
//! the home trash's file system, restoring across mounts and to an
//! unmounted volume, and erasing and sizing a payload with a mount inside.
//!
//! Gated: it runs only with `XDG_KIT_TRASH_MOUNT_TESTS=1`, and then needs
//! util-linux's `unshare` and unprivileged user namespaces. It re-runs
//! itself in a new user and mount namespace (`unshare --user
//! --map-root-user --mount`), where it may mount tmpfs and bind mounts that
//! no other process sees and that vanish with it. All fixtures are in a
//! temporary directory.

#[cfg(target_os = "linux")]
use std::fs;
#[cfg(target_os = "linux")]
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

#[cfg(target_os = "linux")]
use xdg_kit_trash::{
    AttemptFailure, CacheWrite, CandidateState, Context, EraseFailure, EraseOptions,
    EraseProblemKind, Exclusions, LocalZone, Origin, Policy, RestoreFailure, RestoreOptions,
    SizeOptions, SizeProblemKind, SourceProblem, Trash, TrashError, TrashKind,
};

const GATE: &str = "XDG_KIT_TRASH_MOUNT_TESTS";
const CHILD: &str = "XDG_KIT_TRASH_MOUNT_CHILD";

fn main() -> ExitCode {
    if !cfg!(target_os = "linux") {
        println!("mount tests: skipped (Linux only)");
        return ExitCode::SUCCESS;
    }
    if std::env::var_os(CHILD).is_some() {
        #[cfg(target_os = "linux")]
        run();
        println!("mount tests: ok");
        return ExitCode::SUCCESS;
    }
    if std::env::var_os(GATE).as_deref() != Some("1".as_ref()) {
        println!("mount tests: skipped (set {GATE}=1 to run them)");
        return ExitCode::SUCCESS;
    }
    let exe = std::env::current_exe().unwrap();
    let status = Command::new("unshare")
        .args([
            "--user",
            "--map-root-user",
            "--mount",
            "--propagation",
            "private",
        ])
        .arg(exe)
        .env(CHILD, "1")
        .status()
        .expect("running unshare (util-linux)");
    if status.success() {
        ExitCode::SUCCESS
    } else {
        eprintln!("mount tests failed: {status}");
        ExitCode::FAILURE
    }
}

#[cfg(target_os = "linux")]
fn tmpfs(target: &Path) {
    fs::create_dir_all(target).unwrap();
    rustix::mount::mount(
        "tmpfs",
        target,
        "tmpfs",
        rustix::mount::MountFlags::empty(),
        None,
    )
    .unwrap();
}

#[cfg(target_os = "linux")]
fn unmount(target: &Path) {
    rustix::mount::unmount(target, rustix::mount::UnmountFlags::DETACH).unwrap();
}

#[cfg(target_os = "linux")]
struct Fx {
    _dir: tempfile::TempDir,
    root: PathBuf,
    home: PathBuf,
    context: Context,
}

#[cfg(target_os = "linux")]
impl Fx {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(dir.path()).unwrap();
        let home = root.join("data");
        let context = Context::new(
            &home,
            rustix::process::getuid().as_raw(),
            &root,
            LocalZone::utc(),
        )
        .unwrap();
        Self {
            _dir: dir,
            root,
            home,
            context,
        }
    }

    fn home_trash(&self) -> Trash {
        self.context.open(&self.context.home_trash()).unwrap()
    }
}

#[cfg(target_os = "linux")]
fn run() {
    let tests: [(&str, fn()); 12] = [
        ("selection by mount", selection_by_mount),
        ("a bind mount is another mount", bind_mount),
        ("restoring across mounts", restore_across_mounts),
        ("restoring to an unmounted volume", restore_to_unmounted),
        ("erasing stops at a mount", erase_stops_at_mounts),
        ("sizing stops at a mount", size_stops_at_mounts),
        (
            "a payload that is a mount is not entered",
            payload_is_a_mount,
        ),
        (
            "trash roots are told by place, not name",
            trash_roots_by_place,
        ),
        (
            "files named like this user's trash roots",
            files_named_like_own_roots,
        ),
        ("plans across mounts create nothing", plan_across_mounts),
        ("excluded volumes, nested volumes", excluded_volumes),
        ("another mount is not a fallback", cross_device_is_final),
    ];
    for (name, test) in tests {
        println!("mount test: {name}");
        test();
    }
}

#[cfg(target_os = "linux")]
fn selection_by_mount() {
    let fx = Fx::new();
    let vol = fx.root.join("vol");
    tmpfs(&vol);
    fs::create_dir(vol.join("sub")).unwrap();
    fs::write(vol.join("sub/doc"), "d").unwrap();
    let trashed = fx.context.trash(vol.join("sub/doc")).unwrap();
    assert_eq!(trashed.trash.kind(), TrashKind::Private);
    assert_eq!(trashed.trash.base(), vol, "the mount's top directory");
    assert!(matches!(
        trashed.fallbacks[0].failure,
        AttemptFailure::OtherMount
    ));
    assert!(
        !fx.home.join("Trash").exists(),
        "the home trash was not used"
    );
    let info = fs::read_to_string(vol.join(".Trash-0/info/doc.trashinfo")).unwrap();
    assert!(info.contains("\nPath=sub/doc\n"), "{info}");
    // With an administrator's shared directory, that is used.
    fs::create_dir(vol.join(".Trash")).unwrap();
    fs::set_permissions(
        vol.join(".Trash"),
        std::os::unix::fs::PermissionsExt::from_mode(0o1777),
    )
    .unwrap();
    fs::write(vol.join("sub/doc2"), "d").unwrap();
    let trashed = fx.context.trash(vol.join("sub/doc2")).unwrap();
    assert_eq!(trashed.trash.kind(), TrashKind::Shared);
    // A file on the home trash's mount goes home.
    fs::write(fx.root.join("local"), "l").unwrap();
    assert_eq!(
        fx.context
            .trash(fx.root.join("local"))
            .unwrap()
            .trash
            .kind(),
        TrashKind::Home
    );
    // Discovery with the mount lists both layouts.
    let discovery = fx.context.discover([&vol]);
    assert!(discovery.is_complete());
    let kinds: Vec<TrashKind> = discovery
        .trashes
        .iter()
        .map(|trash| trash.dir().kind())
        .collect();
    assert_eq!(
        kinds,
        [TrashKind::Home, TrashKind::Shared, TrashKind::Private]
    );
    // The mount list knows it.
    let mounts = xdg_kit_trash::mounts(&xdg_kit_trash::Limits::default()).unwrap();
    assert!(
        mounts
            .iter()
            .any(|mount| mount.mount_point == vol && mount.fs_type == "tmpfs")
    );
    unmount(&vol);
}

#[cfg(target_os = "linux")]
fn bind_mount() {
    // A directory of the home trash's own file system, bind-mounted
    // elsewhere: same device, another mount; a rename between them fails.
    let fx = Fx::new();
    let source = fx.root.join("source");
    let bind = fx.root.join("bind");
    fs::create_dir_all(&source).unwrap();
    fs::create_dir_all(&bind).unwrap();
    rustix::mount::mount_bind(&source, &bind).unwrap();
    fs::write(bind.join("doc"), "d").unwrap();
    let trashed = fx.context.trash(bind.join("doc")).unwrap();
    assert_eq!(trashed.trash.kind(), TrashKind::Private, "{trashed:?}");
    assert_eq!(trashed.trash.base(), bind);
    assert!(source.join(".Trash-0/files/doc").exists());
    unmount(&bind);
}

#[cfg(target_os = "linux")]
fn restore_across_mounts() {
    let fx = Fx::new();
    fs::write(fx.root.join("doc"), "d").unwrap();
    let _ = fx.context.trash(fx.root.join("doc")).unwrap();
    let vol = fx.root.join("vol");
    tmpfs(&vol);
    let trash = fx.home_trash();
    let entry = trash.entries().next().unwrap().unwrap();
    let options = RestoreOptions {
        destination: Some(vol.join("doc")),
        ..RestoreOptions::default()
    };
    let err = trash.restore(&entry, &options).unwrap_err();
    assert!(
        matches!(err.failure, RestoreFailure::CrossDevice(_)),
        "{err}"
    );
    assert!(
        fx.home.join("Trash/files/doc").exists()
            && fx.home.join("Trash/info/doc.trashinfo").exists()
    );
    assert!(!vol.join("doc").exists());
    unmount(&vol);
}

#[cfg(target_os = "linux")]
fn restore_to_unmounted() {
    let fx = Fx::new();
    let vol = fx.root.join("vol");
    tmpfs(&vol);
    fs::create_dir(vol.join("sub")).unwrap();
    fs::write(vol.join("sub/doc"), "d").unwrap();
    let trashed = fx.context.trash(vol.join("sub/doc")).unwrap();
    let trash = fx.context.open(&trashed.trash).unwrap();
    let entry = trash.entries().next().unwrap().unwrap();
    assert_eq!(entry.origin(), &Origin::Known(vol.join("sub/doc")));
    // The volume goes away; the open trash still works, its origin's
    // directory does not exist.
    unmount(&vol);
    let err = trash
        .restore(&entry, &RestoreOptions::default())
        .unwrap_err();
    assert!(
        matches!(&err.failure, RestoreFailure::MissingParent(path) if path == &vol.join("sub")),
        "{err}"
    );
    // And discovery of the unmounted top directory finds no trash there.
    assert_eq!(fx.context.discover([&vol]).trashes.len(), 0);
}

#[cfg(target_os = "linux")]
fn mounted_inside_payload(fx: &Fx) -> (Trash, PathBuf) {
    let dir = fx.root.join("tree");
    fs::create_dir_all(dir.join("inner")).unwrap();
    fs::write(dir.join("file"), vec![1; 10_000]).unwrap();
    let _ = fx.context.trash(&dir).unwrap();
    let inner = fx.home.join("Trash/files/tree/inner");
    tmpfs(&inner);
    fs::write(inner.join("on-the-mount"), "keep").unwrap();
    (fx.home_trash(), inner)
}

#[cfg(target_os = "linux")]
fn erase_stops_at_mounts() {
    let fx = Fx::new();
    let (trash, inner) = mounted_inside_payload(&fx);
    let entry = trash.entries().next().unwrap().unwrap();
    let err = trash
        .erase(&entry, &EraseOptions::default(), &|| false)
        .unwrap_err();
    assert!(matches!(err.failure, EraseFailure::Incomplete));
    assert!(
        err.problems
            .iter()
            .any(|problem| problem.path == Path::new("inner")
                && matches!(problem.kind, EraseProblemKind::MountBoundary)),
        "{:?}",
        err.problems
    );
    assert_eq!(
        fs::read_to_string(inner.join("on-the-mount")).unwrap(),
        "keep"
    );
    assert!(
        !fx.home.join("Trash/files/tree/file").exists(),
        "the rest went"
    );
    assert!(
        fx.home.join("Trash/info/tree.trashinfo").exists(),
        "kept while the payload is"
    );
    unmount(&inner);
}

#[cfg(target_os = "linux")]
fn size_stops_at_mounts() {
    let fx = Fx::new();
    let (trash, inner) = mounted_inside_payload(&fx);
    let report = trash.size(&SizeOptions::default(), &|| false).unwrap();
    assert!(!report.is_complete());
    assert!(
        report
            .problems
            .iter()
            .any(|problem| matches!(problem.kind, SizeProblemKind::MountBoundary))
    );
    assert!(matches!(report.cache_write, CacheWrite::Written(_)));
    assert_eq!(
        fs::read_to_string(fx.home.join("Trash/directorysizes")).unwrap(),
        "",
        "an incomplete size is not cached"
    );
    unmount(&inner);
}

#[cfg(target_os = "linux")]
fn payload_is_a_mount() {
    // An outside directory bind-mounted onto the payload's own name: the
    // payload root is on another mount than `files`.
    let fx = Fx::new();
    fs::create_dir_all(fx.root.join("p")).unwrap();
    let _ = fx.context.trash(fx.root.join("p")).unwrap();
    let outside = fx.root.join("outside");
    fs::create_dir(&outside).unwrap();
    fs::write(outside.join("precious"), "keep").unwrap();
    let payload = fx.home.join("Trash/files/p");
    rustix::mount::mount_bind(&outside, &payload).unwrap();
    let trash = fx.home_trash();
    let entry = trash.entries().next().unwrap().unwrap();
    let report = trash.size(&SizeOptions::default(), &|| false).unwrap();
    assert!(
        report
            .problems
            .iter()
            .any(|problem| matches!(problem.kind, SizeProblemKind::MountBoundary)),
        "{:?}",
        report.problems
    );
    let err = trash
        .erase(&entry, &EraseOptions::default(), &|| false)
        .unwrap_err();
    assert!(matches!(err.failure, EraseFailure::Incomplete), "{err}");
    assert!(
        err.problems
            .iter()
            .any(|problem| problem.path == Path::new("")
                && matches!(problem.kind, EraseProblemKind::MountBoundary)),
        "{:?}",
        err.problems
    );
    assert_eq!(err.removed, 0);
    assert_eq!(
        fs::read_to_string(outside.join("precious")).unwrap(),
        "keep"
    );
    assert!(fx.home.join("Trash/info/p.trashinfo").exists());
    unmount(&payload);
}

#[cfg(target_os = "linux")]
fn trash_roots_by_place() {
    let fx = Fx::new();
    let vol = fx.root.join("vol");
    tmpfs(&vol);
    // Another user's trash at the mount's top: never trashed from.
    fs::create_dir_all(vol.join(".Trash-4242/files")).unwrap();
    fs::write(vol.join(".Trash-4242/files/x"), "x").unwrap();
    assert!(matches!(
        fx.context.trash(vol.join(".Trash-4242/files/x")),
        Err(TrashError::InvalidSource {
            problem: SourceProblem::InTrash,
            ..
        })
    ));
    // The same name deeper down is an ordinary directory.
    fs::create_dir_all(vol.join("sub/.Trash-4242/files")).unwrap();
    fs::write(vol.join("sub/.Trash-4242/files/x"), "x").unwrap();
    let trashed = fx
        .context
        .trash(vol.join("sub/.Trash-4242/files/x"))
        .unwrap();
    assert_eq!(trashed.trash.base(), vol);
    unmount(&vol);
}

#[cfg(target_os = "linux")]
fn files_named_like_own_roots() {
    // The home trash on the volume itself; regular files and symbolic links
    // at the volume's top named like its trash roots (this user's
    // included) hold no trash, and go to the home trash.
    let fx = Fx::new();
    let vol = fx.root.join("vol");
    tmpfs(&vol);
    let uid = rustix::process::getuid().as_raw();
    let context = Context::new(vol.join("data"), uid, &vol, LocalZone::utc()).unwrap();
    fs::create_dir(vol.join("target")).unwrap();
    for name in [".Trash".to_string(), format!(".Trash-{uid}")] {
        for link in [false, true] {
            let path = vol.join(&name);
            if link {
                std::os::unix::fs::symlink(vol.join("target"), &path).unwrap();
            } else {
                fs::write(&path, "").unwrap();
            }
            let trashed = context
                .trash(&path)
                .unwrap_or_else(|err| panic!("{name} (link {link}): {err}"));
            assert_eq!(trashed.trash.kind(), TrashKind::Home);
            assert!(!path.exists() && fs::symlink_metadata(&path).is_err());
        }
    }
    assert!(vol.join("target").is_dir(), "a link's target stays");
    // A real directory there is still refused.
    fs::create_dir_all(vol.join(format!(".Trash-{uid}/files"))).unwrap();
    assert!(matches!(
        context.trash(vol.join(format!(".Trash-{uid}"))),
        Err(TrashError::InvalidSource {
            problem: SourceProblem::HoldsTrash,
            ..
        })
    ));
    unmount(&vol);
}

#[cfg(target_os = "linux")]
fn plan_across_mounts() {
    let fx = Fx::new();
    let vol = fx.root.join("vol");
    tmpfs(&vol);
    fs::create_dir(vol.join("sub")).unwrap();
    fs::write(vol.join("sub/doc"), "d").unwrap();
    let listing = |dir: &Path| {
        let mut names: Vec<_> = fs::read_dir(dir)
            .unwrap()
            .map(|item| item.unwrap().file_name())
            .collect();
        names.sort();
        names
    };
    let before = listing(&vol);
    let plan = fx.context.plan(vol.join("sub/doc")).unwrap();
    assert_eq!(listing(&vol), before, "nothing made on the volume");
    assert!(!fx.home.exists(), "nor at home");
    assert!(matches!(
        plan.skipped[0].failure,
        AttemptFailure::OtherMount
    ));
    let destination = plan.destination().unwrap();
    assert_eq!(destination.trash.kind(), TrashKind::Private);
    assert!(
        matches!(&destination.state, CandidateState::WouldCreate(path) if path == &vol.join(".Trash-0"))
    );
    let trashed = fx.context.trash(vol.join("sub/doc")).unwrap();
    assert_eq!(trashed.trash, destination.trash);
    // A file on the home trash's mount: the home trash only.
    fs::write(fx.root.join("local"), "l").unwrap();
    let plan = fx.context.plan(fx.root.join("local")).unwrap();
    let kinds: Vec<_> = plan
        .candidates
        .iter()
        .map(|candidate| candidate.trash.kind())
        .collect();
    assert_eq!(kinds, [TrashKind::Home]);
    assert!(plan.skipped.is_empty());
    unmount(&vol);
}

#[cfg(target_os = "linux")]
fn excluded_volumes() {
    // A volume excluded from a real mount list, with a volume nested
    // inside it that is not.
    let fx = Fx::new();
    let vol = fx.root.join("vol");
    tmpfs(&vol);
    let inner = vol.join("inner");
    tmpfs(&inner);
    fs::write(vol.join("f"), "f").unwrap();
    fs::write(inner.join("g"), "g").unwrap();
    let mounts = xdg_kit_trash::mounts(&xdg_kit_trash::Limits::default()).unwrap();
    let exclusions = Exclusions::from_mounts(&mounts, |mount| mount.mount_point == vol);
    let mut policy = Policy::default();
    policy.exclusions = exclusions;
    let context = fx.context.clone().with_policy(policy);
    for result in [
        context.trash(vol.join("f")).err(),
        context.plan(vol.join("f")).err(),
    ] {
        assert!(
            matches!(result, Some(TrashError::Excluded(_))),
            "{result:?}"
        );
    }
    assert!(vol.join("f").exists());
    assert!(
        !vol.join(".Trash-0").exists(),
        "nothing made on the excluded volume"
    );
    // The nested volume is its own: trashed into its own trash.
    let trashed = context.trash(inner.join("g")).unwrap();
    assert_eq!(trashed.trash.base(), inner);
    assert!(inner.join(".Trash-0/files/g").exists());
    // A file of the home trash's mount is unaffected.
    fs::write(fx.root.join("local"), "l").unwrap();
    assert_eq!(
        context.trash(fx.root.join("local")).unwrap().trash.kind(),
        TrashKind::Home
    );
    unmount(&inner);
    unmount(&vol);
}

#[cfg(target_os = "linux")]
fn cross_device_is_final() {
    // The home trash named explicitly for a file on another mount: the
    // rename says another mount, and no fallback is tried (a link cannot
    // cross mounts either).
    let fx = Fx::new();
    let vol = fx.root.join("vol");
    tmpfs(&vol);
    fs::write(vol.join("f"), "f").unwrap();
    let Err(TrashError::NoTrash { attempts, .. }) = fx
        .context
        .trash_into(&fx.context.home_trash(), vol.join("f"))
    else {
        panic!()
    };
    assert!(
        matches!(attempts[0].failure, AttemptFailure::CrossDevice),
        "{attempts:?}"
    );
    assert!(vol.join("f").exists());
    let files: Vec<_> = fs::read_dir(fx.home.join("Trash/files")).unwrap().collect();
    assert!(files.is_empty());
    unmount(&vol);
}
