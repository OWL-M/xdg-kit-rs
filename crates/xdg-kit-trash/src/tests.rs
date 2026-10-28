//! Trashing, listing, restoring, erasing and sizing, on fixtures in
//! temporary directories only: no test touches the developer's trash.
//! Expected values are written out from the spec, not computed with this
//! crate's own functions.

use std::ffi::{OsStr, OsString};
use std::fs;
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _, symlink};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::fault::{self, Stage};
use crate::*;
use crate::{insert, sys};

/// 2024-02-29T13:14:15Z.
const WHEN: u64 = 1_709_212_455;

fn at(seconds: u64) -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(seconds)
}

fn uid() -> u32 {
    rustix::process::getuid().as_raw()
}

fn is_root() -> bool {
    rustix::process::geteuid().is_root()
}

/// A temporary root with `data` (the data home) and `work` (where files
/// to trash live, and the working directory), and a context over them in
/// UTC at [`WHEN`].
struct Fx {
    _dir: tempfile::TempDir,
    root: PathBuf,
    home: PathBuf,
    work: PathBuf,
    context: Context,
}

impl Fx {
    fn new() -> Self {
        fault::clear();
        let dir = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(dir.path()).unwrap();
        let home = root.join("data");
        let work = root.join("work");
        fs::create_dir(&work).unwrap();
        let context = Context::new(&home, uid(), &work, LocalZone::utc())
            .unwrap()
            .with_clock(Clock::Fixed(at(WHEN)));
        Self {
            _dir: dir,
            root,
            home,
            work,
            context,
        }
    }

    fn file(&self, rel: &str, body: &str) -> PathBuf {
        let path = self.work.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, body).unwrap();
        path
    }

    fn trash(&self) -> PathBuf {
        self.home.join("Trash")
    }

    fn open_home(&self) -> Trash {
        self.context.open(&self.context.home_trash()).unwrap()
    }
}

impl Drop for Fx {
    fn drop(&mut self) {
        fault::clear();
    }
}

fn entries(trash: &Trash) -> Vec<Entry> {
    let mut all: Vec<Entry> = trash.entries().map(Result::unwrap).collect();
    all.sort_by(|a, b| a.name().cmp(b.name()));
    all
}

fn names(dir: &Path) -> Vec<OsString> {
    let mut names: Vec<OsString> = fs::read_dir(dir)
        .unwrap()
        .map(|item| item.unwrap().file_name())
        .collect();
    names.sort();
    names
}

fn mode(path: &Path) -> u32 {
    fs::symlink_metadata(path).unwrap().permissions().mode() & 0o7777
}

/// Runs `f` without root's power over permissions: directly when not
/// root; as root, on a thread whose user and group are 65534 (Linux sets
/// IDs per thread); elsewhere not at all, saying so.
fn unprivileged<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> Option<T> {
    if !is_root() {
        return Some(f());
    }
    #[cfg(target_os = "linux")]
    {
        use rustix::process::{Gid, Uid};
        std::thread::spawn(move || {
            let (gid, user) = (Gid::from_raw(65534), Uid::from_raw(65534));
            // In a user namespace that maps no such IDs this fails.
            let dropped = rustix::thread::set_thread_res_gid(gid, gid, gid)
                .and_then(|()| rustix::thread::set_thread_res_uid(user, user, user));
            match dropped {
                Ok(()) => Some(f()),
                Err(err) => {
                    eprintln!("skipped: running as root, and dropping to user 65534 failed: {err}");
                    None
                }
            }
        })
        .join()
        .unwrap()
    }
    #[cfg(not(target_os = "linux"))]
    {
        eprintln!("skipped: running as root, and only Linux sets user IDs per thread");
        None
    }
}

mod info_format {
    use super::*;

    fn parse(text: &[u8]) -> TrashInfo {
        TrashInfo::parse(text).unwrap()
    }

    #[test]
    fn the_first_occurrence_wins() {
        let info = parse(b"[Trash Info]\nPath=a\nDeletionDate=2004-08-31T22:32:08\nPath=b\nDeletionDate=2010-01-01T00:00:00\n");
        assert_eq!(info.path(), &Field::Valid(b"a".to_vec()));
        assert_eq!(
            info.deletion_date(),
            &Field::Valid(DeletionDate::new(2004, 8, 31, 22, 32, 8).unwrap())
        );
    }

    #[test]
    fn a_malformed_first_occurrence_is_not_replaced() {
        let info = parse(b"[Trash Info]\nPath=a%G0\nPath=good\nDeletionDate=yesterday\nDeletionDate=2004-08-31T22:32:08\n");
        assert_eq!(
            info.path(),
            &Field::Invalid(FieldProblem::Escape(EscapeProblem::Percent(1)))
        );
        assert_eq!(info.deletion_date(), &Field::Invalid(FieldProblem::Date));
    }

    #[test]
    fn other_lines_and_groups_are_ignored() {
        let info = parse(b"[Trash Info]\n# comment\nPathX=no\nX-Path=no\n[Other Group]\nUnknown=1\n  Path=indented, no\nPath = spaced\n");
        assert_eq!(info.path(), &Field::Valid(b"spaced".to_vec()));
        assert_eq!(info.deletion_date(), &Field::Missing);
    }

    #[test]
    fn the_header_must_be_exact() {
        for text in [
            &b""[..],
            b"\n[Trash Info]\nPath=a\n",
            b"[Trash Info] \nPath=a\n",
            b"[trash info]\nPath=a\n",
            b"\xEF\xBB\xBF[Trash Info]\nPath=a\n",
            b"[Trash Info]\r\nPath=a\n",
            b"Path=a\n[Trash Info]\n",
        ] {
            assert_eq!(
                TrashInfo::parse(text),
                Err(HeaderProblem),
                "{:?}",
                String::from_utf8_lossy(text)
            );
        }
        assert!(
            TrashInfo::parse(b"[Trash Info]").is_ok(),
            "a header alone, no newline"
        );
    }

    #[test]
    fn escapes_decode_to_bytes() {
        let info = parse(b"[Trash Info]\nPath=caf%e9%20%2Fx\\sy%25\n");
        assert_eq!(info.path(), &Field::Valid(b"caf\xE9 /x y%".to_vec()));
        for (value, problem) in [
            (&b"%4"[..], FieldProblem::Escape(EscapeProblem::Percent(0))),
            (b"ab%", FieldProblem::Escape(EscapeProblem::Percent(2))),
            (b"a%zz", FieldProblem::Escape(EscapeProblem::Percent(1))),
            (b"a\\x", FieldProblem::Escape(EscapeProblem::Backslash(1))),
            (b"a\\", FieldProblem::Escape(EscapeProblem::Backslash(1))),
            (b"", FieldProblem::Empty),
        ] {
            let mut text = b"[Trash Info]\nPath=".to_vec();
            text.extend_from_slice(value);
            assert_eq!(
                parse(&text).path(),
                &Field::Invalid(problem),
                "{:?}",
                String::from_utf8_lossy(value)
            );
        }
    }

    #[test]
    fn decoded_paths_are_checked() {
        let base = Path::new("/base");
        let ok = |raw: &[u8], volume| info::resolve(raw, base, volume);
        assert_eq!(ok(b"a/b", None), Ok(PathBuf::from("/base/a/b")));
        assert_eq!(ok(b"./a//b", None), Ok(PathBuf::from("/base/a/b")));
        assert_eq!(ok(b"/etc/x", None), Ok(PathBuf::from("/etc/x")));
        assert_eq!(ok(b"a/../b", None), Err(PathProblem::ParentComponent));
        assert_eq!(ok(b"..", None), Err(PathProblem::ParentComponent));
        assert_eq!(ok(b"/a/../../etc", None), Err(PathProblem::ParentComponent));
        assert_eq!(ok(b"a\0b", None), Err(PathProblem::Nul));
        assert_eq!(ok(b"/", None), Err(PathProblem::NoFileName));
        assert_eq!(ok(b"a/", None), Err(PathProblem::NoFileName));
        assert_eq!(ok(b"a/.", None), Err(PathProblem::NoFileName));
        let volume = Some(AbsolutePaths::UnderTopdir);
        assert_eq!(ok(b"/base/x", volume), Ok(PathBuf::from("/base/x")));
        assert_eq!(ok(b"/based/x", volume), Err(PathProblem::OutsideTopdir));
        assert_eq!(ok(b"/base", volume), Err(PathProblem::OutsideTopdir));
        assert_eq!(ok(b"/etc/x", volume), Err(PathProblem::OutsideTopdir));
        assert_eq!(
            ok(b"/base/x", Some(AbsolutePaths::Reject)),
            Err(PathProblem::AbsoluteRefused)
        );
        assert_eq!(
            ok(b"/etc/x", Some(AbsolutePaths::Accept)),
            Ok(PathBuf::from("/etc/x"))
        );
        assert_eq!(
            ok(b"rel", Some(AbsolutePaths::Reject)),
            Ok(PathBuf::from("/base/rel"))
        );
    }

    #[test]
    fn an_escaped_parent_component_is_still_one() {
        let info = parse(b"[Trash Info]\nPath=%2E%2E/x\n");
        let raw = info.path().valid().unwrap();
        assert_eq!(
            info::resolve(raw, Path::new("/b"), None),
            Err(PathProblem::ParentComponent)
        );
    }

    #[test]
    fn writing_escapes_every_byte_but_the_unreserved() {
        let all: Vec<u8> = (1..=255).collect();
        let written = info::format(&all, DeletionDate::new(2020, 1, 2, 3, 4, 5).unwrap());
        let text = std::str::from_utf8(&written).unwrap();
        let path = text.lines().nth(1).unwrap().strip_prefix("Path=").unwrap();
        for byte in 1..=255u8 {
            let kept = byte.is_ascii_alphanumeric() || b"-_.!~*'()/".contains(&byte);
            let escaped = format!("%{byte:02X}");
            assert_eq!(path.contains(&escaped), !kept, "byte {byte:#04x}");
        }
        assert!(text.ends_with("\nDeletionDate=2020-01-02T03:04:05\n"));
        let back = parse(&written);
        assert_eq!(back.path(), &Field::Valid(all));
    }
}

mod times {
    use super::*;

    #[test]
    fn deletion_dates_parse_strictly() {
        let date = |text: &str| DeletionDate::parse(text.as_bytes());
        assert_eq!(
            date("2004-08-31T22:32:08"),
            DeletionDate::new(2004, 8, 31, 22, 32, 8)
        );
        assert_eq!(
            date("20040831T22:32:08"),
            DeletionDate::new(2004, 8, 31, 22, 32, 8),
            "the spec's example"
        );
        assert_eq!(
            date("2024-02-29T00:00:00"),
            DeletionDate::new(2024, 2, 29, 0, 0, 0)
        );
        for bad in [
            "2023-02-29T00:00:00",
            "2004-08-31T24:00:00",
            "2004-08-31T23:60:00",
            "2004-08-31T23:59:60",
            "2004-08-31T22:32:08Z",
            "2004-08-31T22:32:08+02:00",
            "2004-08-31T22:32:08.5",
            "2004-08-31 22:32:08",
            "2004-8-31T22:32:08",
            "+004-08-31T22:32:08",
            "2004-08-31T22:32",
            "",
        ] {
            assert_eq!(date(bad), None, "{bad}");
        }
        assert_eq!(
            DeletionDate::new(2004, 8, 31, 22, 32, 8)
                .unwrap()
                .to_string(),
            "2004-08-31T22:32:08"
        );
        assert_eq!(DeletionDate::new(10000, 1, 1, 0, 0, 0), None);
    }

    #[test]
    fn writing_uses_the_zone() {
        let east = LocalZone::fixed(5 * 3600 + 1800).unwrap();
        assert_eq!(
            DeletionDate::at(at(WHEN), &east).unwrap().to_string(),
            "2024-02-29T18:44:15"
        );
        assert_eq!(
            DeletionDate::at(at(WHEN), &LocalZone::utc())
                .unwrap()
                .to_string(),
            "2024-02-29T13:14:15"
        );
    }

    /// A `TZif` (version 1) zone: UTC+1, and UTC+2 from 2024-03-31T01:00Z to
    /// 2024-10-27T01:00Z.
    fn summer_zone() -> LocalZone {
        let mut data = b"TZif".to_vec();
        data.extend_from_slice(&[0; 16]);
        for count in [0u32, 0, 0, 2, 2, 9] {
            data.extend_from_slice(&count.to_be_bytes());
        }
        for time in [1_711_846_800i32, 1_729_990_800] {
            data.extend_from_slice(&time.to_be_bytes());
        }
        data.extend_from_slice(&[1, 0]);
        data.extend_from_slice(&3600i32.to_be_bytes());
        data.extend_from_slice(&[0, 0]);
        data.extend_from_slice(&7200i32.to_be_bytes());
        data.extend_from_slice(&[1, 4]);
        data.extend_from_slice(b"STD\0SUMM\0");
        LocalZone::tzif("Test/Summer", &data).unwrap()
    }

    #[test]
    fn civil_times_are_not_instants() {
        let zone = summer_zone();
        let date = |text: &str| DeletionDate::parse(text.as_bytes()).unwrap();
        assert_eq!(
            date("2024-06-01T14:00:00").instants(&zone),
            Instants::Unique(at(1_717_243_200))
        );
        // 02:30 was skipped when clocks went from 02:00 to 03:00.
        assert_eq!(
            date("2024-03-31T02:30:00").instants(&zone),
            Instants::Skipped {
                before: at(1_711_848_600),
                after: at(1_711_845_000),
            }
        );
        // 02:30 came twice when clocks went from 03:00 back to 02:00.
        assert_eq!(
            date("2024-10-27T02:30:00").instants(&zone),
            Instants::Ambiguous {
                earlier: at(1_729_989_000),
                later: at(1_729_992_600),
            }
        );
        assert_eq!(zone.name(), Some("Test/Summer"));
    }

    #[test]
    fn the_end_of_the_range_is_not_an_instant() {
        let last = DeletionDate::new(9999, 12, 31, 23, 59, 59).unwrap();
        assert_eq!(last.instants(&LocalZone::utc()), Instants::OutOfRange);
        let near = DeletionDate::new(9999, 12, 30, 21, 0, 0).unwrap();
        assert!(matches!(
            near.instants(&LocalZone::utc()),
            Instants::Unique(_)
        ));
        let first = DeletionDate::new(0, 1, 1, 0, 0, 0).unwrap();
        assert!(matches!(
            first.instants(&LocalZone::fixed(14 * 3600).unwrap()),
            Instants::Unique(_)
        ));
    }
}

mod trashing {
    use super::*;

    #[test]
    fn into_the_home_trash() {
        let fx = Fx::new();
        let source = fx.file("notes.txt", "hello");
        let trashed = fx.context.trash(&source).unwrap();
        assert_eq!(trashed.trash.kind(), TrashKind::Home);
        assert_eq!(trashed.name, "notes.txt");
        assert_eq!(trashed.original, source);
        assert!(matches!(trashed.durability, Durability::Synced));
        assert!(!trashed.replaced_meanwhile);
        assert!(!source.exists());
        assert_eq!(
            fs::read_to_string(fx.trash().join("files/notes.txt")).unwrap(),
            "hello"
        );
        assert_eq!(
            fs::read_to_string(fx.trash().join("info/notes.txt.trashinfo")).unwrap(),
            format!(
                "[Trash Info]\nPath={}\nDeletionDate=2024-02-29T13:14:15\n",
                source.display()
            )
        );
        for dir in [
            fx.trash(),
            fx.trash().join("files"),
            fx.trash().join("info"),
        ] {
            assert_eq!(mode(&dir), 0o700, "{}", dir.display());
        }
        assert_eq!(
            mode(&fx.trash().join("info/notes.txt.trashinfo")) & 0o077,
            0
        );
    }

    #[test]
    fn below_the_data_home_the_path_is_relative() {
        let fx = Fx::new();
        fs::create_dir_all(fx.home.join("docs")).unwrap();
        fs::write(fx.home.join("docs/a b"), "x").unwrap();
        let _ = fx.context.trash(fx.home.join("docs/a b")).unwrap();
        let info = fs::read_to_string(fx.trash().join("info/a b.trashinfo")).unwrap();
        assert_eq!(
            info,
            "[Trash Info]\nPath=docs/a%20b\nDeletionDate=2024-02-29T13:14:15\n"
        );
        let entry = &entries(&fx.open_home())[0];
        assert_eq!(entry.origin(), &Origin::Known(fx.home.join("docs/a b")));
    }

    #[test]
    fn relative_paths_are_from_the_context() {
        let fx = Fx::new();
        fx.file("sub/r.txt", "r");
        let trashed = fx.context.trash("sub/r.txt").unwrap();
        assert_eq!(trashed.original, fx.work.join("sub/r.txt"));
        let trashed = fx.context.trash("sub/").unwrap();
        assert_eq!(
            trashed.original,
            fx.work.join("sub"),
            "trailing slashes are ignored"
        );
    }

    #[test]
    fn symlinks_are_trashed_themselves() {
        let fx = Fx::new();
        let target = fx.file("target", "keep");
        symlink(&target, fx.work.join("link")).unwrap();
        symlink("/nonexistent", fx.work.join("dangling")).unwrap();
        let _ = fx.context.trash(fx.work.join("link")).unwrap();
        let _ = fx.context.trash(fx.work.join("dangling")).unwrap();
        assert_eq!(fs::read_to_string(&target).unwrap(), "keep");
        let files = fx.trash().join("files");
        assert_eq!(fs::read_link(files.join("link")).unwrap(), target);
        assert_eq!(
            fs::read_link(files.join("dangling")).unwrap(),
            Path::new("/nonexistent")
        );
        // A symlinked directory above the file is followed; the recorded
        // origin is the real directory.
        fs::create_dir(fx.work.join("real")).unwrap();
        symlink(fx.work.join("real"), fx.work.join("alias")).unwrap();
        fs::write(fx.work.join("real/f"), "f").unwrap();
        let trashed = fx.context.trash(fx.work.join("alias/f")).unwrap();
        assert_eq!(trashed.original, fx.work.join("real/f"));
    }

    #[test]
    fn non_utf8_names_keep_their_bytes() {
        let fx = Fx::new();
        let name = OsStr::from_bytes(b"caf\xE9 \n%");
        let source = fx.work.join(name);
        fs::write(&source, "x").unwrap();
        let trashed = fx.context.trash(&source).unwrap();
        assert_eq!(trashed.name, name);
        let info = fs::read(fx.trash().join("info").join(info_name(name))).unwrap();
        let expected = format!("Path={}/caf%E9%20%0A%25\n", fx.work.display());
        assert!(
            info.windows(expected.len())
                .any(|w| w == expected.as_bytes()),
            "{}",
            String::from_utf8_lossy(&info)
        );
        let trash = fx.open_home();
        let entry = &entries(&trash)[0];
        assert_eq!(entry.origin(), &Origin::Known(source.clone()));
        let _ = trash.restore(entry, &RestoreOptions::default()).unwrap();
        assert_eq!(fs::read(&source).unwrap(), b"x");
    }

    fn info_name(name: &OsStr) -> OsString {
        let mut info = name.to_os_string();
        info.push(".trashinfo");
        info
    }

    #[test]
    fn collisions_never_overwrite() {
        let fx = Fx::new();
        let files = fx.trash().join("files");
        let info = fx.trash().join("info");
        fx.file("a", "first");
        let _ = fx.context.trash(fx.work.join("a")).unwrap();
        // An orphaned payload, an orphaned directory, a dangling link and a
        // trash info file without payload take the next names.
        fs::write(files.join("a.2"), "orphan").unwrap();
        fs::create_dir(files.join("a.3")).unwrap();
        symlink("/nonexistent", files.join("a.4")).unwrap();
        fs::write(info.join("a.5.trashinfo"), "[Trash Info]\nPath=reserved\n").unwrap();
        fx.file("a", "second");
        let trashed = fx.context.trash(fx.work.join("a")).unwrap();
        assert_eq!(trashed.name, "a.6");
        assert_eq!(fs::read_to_string(files.join("a")).unwrap(), "first");
        assert_eq!(fs::read_to_string(files.join("a.2")).unwrap(), "orphan");
        assert!(fs::read_dir(files.join("a.3")).unwrap().next().is_none());
        assert_eq!(
            fs::read_link(files.join("a.4")).unwrap(),
            Path::new("/nonexistent")
        );
        assert_eq!(
            fs::read_to_string(info.join("a.5.trashinfo")).unwrap(),
            "[Trash Info]\nPath=reserved\n"
        );
        assert_eq!(fs::read_to_string(files.join("a.6")).unwrap(), "second");
        assert!(!info.join("a.2.trashinfo").exists() && !info.join("a.3.trashinfo").exists());
    }

    #[test]
    fn a_name_taken_after_the_check_is_released() {
        // The payload name is free when looked at, taken when renamed to:
        // the move fails as existing, the reservation goes, the next name
        // is used.
        let fx = Fx::new();
        fx.file("b", "x");
        let _ = fx.context.trash(fx.work.join("b")).unwrap();
        fs::remove_file(fx.trash().join("info/b.trashinfo")).unwrap();
        // `b` is now an orphaned payload; the reservation of `b` itself
        // is skipped by the look in `files`.
        fx.file("b", "y");
        let trashed = fx.context.trash(fx.work.join("b")).unwrap();
        assert_eq!(trashed.name, "b.2");
        assert_eq!(names(&fx.trash().join("info")), ["b.2.trashinfo"]);
    }

    #[test]
    fn long_names_fit_with_room_for_suffixes() {
        let fx = Fx::new();
        let name = "x".repeat(255);
        fx.file(&name, "1");
        let first = fx.context.trash(fx.work.join(&name)).unwrap();
        fx.file(&name, "2");
        let second = fx.context.trash(fx.work.join(&name)).unwrap();
        let limit = 255 - ".trashinfo".len();
        assert_eq!(first.name.len(), limit);
        assert_eq!(second.name.len(), limit);
        assert_eq!(
            second.name.as_bytes(),
            [&name.as_bytes()[..limit - 2], b".2"].concat()
        );
        for entry in names(&fx.trash().join("info")) {
            assert!(entry.len() <= 255);
        }
        // A multi-byte character is not cut in half.
        let wide = "é".repeat(127);
        fx.file(&wide, "w");
        let trashed = fx.context.trash(fx.work.join(&wide)).unwrap();
        assert!(trashed.name.to_str().is_some());
        assert_eq!(trashed.name.len(), 244);
    }

    #[test]
    fn simultaneous_insertions_get_distinct_names() {
        let fx = Fx::new();
        let count = 16;
        let sources: Vec<PathBuf> = (0..count)
            .map(|i| fx.file(&format!("d{i}/same"), &i.to_string()))
            .collect();
        let context = fx.context.clone();
        let names: Vec<OsString> = std::thread::scope(|scope| {
            let handles: Vec<_> = sources
                .iter()
                .map(|source| {
                    let context = context.clone();
                    scope.spawn(move || context.trash(source).unwrap().name)
                })
                .collect();
            handles
                .into_iter()
                .map(|handle| handle.join().unwrap())
                .collect()
        });
        let mut unique = names.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(unique.len(), count);
        // Each payload is described by its own trash info file.
        for entry in entries(&fx.open_home()) {
            let body = fs::read_to_string(fx.trash().join("files").join(entry.name())).unwrap();
            assert_eq!(
                entry.origin(),
                &Origin::Known(fx.work.join(format!("d{body}/same")))
            );
        }
    }

    #[test]
    fn the_trash_itself_is_refused() {
        let fx = Fx::new();
        fx.file("x", "x");
        let _ = fx.context.trash(fx.work.join("x")).unwrap();
        let problem = |path: PathBuf| match fx.context.trash(path) {
            Err(TrashError::InvalidSource { problem, .. }) => problem,
            other => panic!("{other:?}"),
        };
        assert_eq!(problem(fx.trash()), SourceProblem::HoldsTrash);
        assert_eq!(problem(fx.home.clone()), SourceProblem::HoldsTrash);
        assert_eq!(problem(fx.trash().join("files/x")), SourceProblem::InTrash);
        assert_eq!(problem(fx.trash().join("info")), SourceProblem::InTrash);
        assert!(fx.trash().join("files/x").exists());
    }

    #[test]
    fn sources_that_cannot_be_trashed() {
        let fx = Fx::new();
        for path in ["/", ".", "..", "a/..", ""] {
            assert!(
                matches!(
                    fx.context.trash(path),
                    Err(TrashError::InvalidSource {
                        problem: SourceProblem::NoName,
                        ..
                    })
                ),
                "{path:?}"
            );
        }
        assert!(matches!(
            fx.context.trash("missing"),
            Err(TrashError::SourceMissing { .. })
        ));
        assert!(matches!(
            fx.context.trash("no/dir/x"),
            Err(TrashError::SourceMissing { .. })
        ));
        assert!(!fx.trash().exists(), "nothing was created");
    }

    /// Fails `stages`, trashes, and checks nothing moved and no trash info
    /// file was left.
    fn fails_cleanly(stages: &[(Stage, u32)]) -> Vec<Attempt> {
        let fx = Fx::new();
        let source = fx.file("f", "data");
        for &(stage, skip) in stages {
            fault::fail(stage, skip);
        }
        let Err(TrashError::NoTrash { attempts, .. }) = fx.context.trash(&source) else {
            panic!("expected NoTrash for {stages:?}");
        };
        fault::clear();
        assert_eq!(fs::read_to_string(&source).unwrap(), "data", "{stages:?}");
        assert_eq!(
            names(&fx.trash().join("files")),
            Vec::<OsString>::new(),
            "{stages:?}"
        );
        assert_eq!(
            names(&fx.trash().join("info")),
            Vec::<OsString>::new(),
            "{stages:?}"
        );
        attempts
    }

    #[test]
    fn a_failure_before_the_move_leaves_the_source() {
        for stage in [
            Stage::CreateInfo,
            Stage::WriteInfo,
            Stage::SyncInfo,
            Stage::SyncInfoDir,
            Stage::Move,
        ] {
            let attempts = fails_cleanly(&[(stage, 0)]);
            assert_eq!(attempts.len(), 1, "{stage:?}");
            assert!(attempts[0].leftovers.is_empty());
            let expected = match stage {
                Stage::Move => matches!(attempts[0].failure, AttemptFailure::Move(_)),
                _ => matches!(attempts[0].failure, AttemptFailure::Metadata(_)),
            };
            assert!(expected, "{stage:?}: {:?}", attempts[0].failure);
        }
        let attempts = fails_cleanly(&[(Stage::MoveCrossDevice, 0)]);
        assert!(matches!(attempts[0].failure, AttemptFailure::CrossDevice));
    }

    #[test]
    fn a_reservation_that_cannot_be_removed_is_reported() {
        let fx = Fx::new();
        let source = fx.file("f", "data");
        fault::fail(Stage::Move, 0);
        fault::fail(Stage::RemoveReservation, 0);
        let Err(TrashError::NoTrash { attempts, .. }) = fx.context.trash(&source) else {
            panic!()
        };
        let leftover = attempts[0].leftovers.first().unwrap();
        assert_eq!(leftover.path, fx.trash().join("info/f.trashinfo"));
        assert!(leftover.path.exists());
        assert!(source.exists());
        // Listed as metadata without payload.
        let listed = entries(&fx.open_home());
        assert!(matches!(listed[0].payload(), Payload::Missing));
    }

    #[test]
    fn a_vanished_source_keeps_its_cleanup_outcome() {
        let fx = Fx::new();
        let source = fx.file("f", "data");
        fault::fail(Stage::SourceVanishes, 0);
        fault::fail(Stage::RemoveReservation, 0);
        let Err(TrashError::SourceMissing { attempts, .. }) = fx.context.trash(&source) else {
            panic!()
        };
        let last = attempts.last().unwrap();
        assert!(matches!(last.failure, AttemptFailure::SourceGone));
        let leftover = last.leftovers.first().unwrap();
        assert_eq!(leftover.path, fx.trash().join("info/f.trashinfo"));
        assert!(leftover.path.exists());
        // Without the cleanup failure, nothing is left.
        let fx = Fx::new();
        let source = fx.file("f", "data");
        fault::fail(Stage::SourceVanishes, 0);
        let Err(TrashError::SourceMissing { attempts, .. }) = fx.context.trash(&source) else {
            panic!()
        };
        assert!(attempts.last().unwrap().leftovers.is_empty());
        assert_eq!(names(&fx.trash().join("info")), Vec::<OsString>::new());
    }

    #[test]
    fn existing_trash_directories_are_never_trashed() {
        // A private trash at the top directory of the source's mount, the
        // home trash on the same mount: selection would use only the home
        // trash, but the private one is still a trash.
        let fx = Fx::new();
        let top = fx.root.join("vol");
        fs::create_dir(&top).unwrap();
        fs::write(top.join("a"), "a").unwrap();
        let private = fx.context.private_trash(&top);
        let _ = fx.context.trash_into(&private, top.join("a")).unwrap();
        fault::pretend_volume(&top);
        let problem = |path: PathBuf| match fx.context.trash(path) {
            Err(TrashError::InvalidSource { problem, .. }) => problem,
            other => panic!("{other:?}"),
        };
        assert_eq!(problem(private.path()), SourceProblem::HoldsTrash);
        assert_eq!(
            problem(private.path().join("files/a")),
            SourceProblem::InTrash
        );
        // Another user's, and an administrator's shared directory.
        for dir in [".Trash-4242/files", ".Trash/4242/files"] {
            fs::create_dir_all(top.join(dir)).unwrap();
            fs::write(top.join(dir).join("x"), "x").unwrap();
            assert_eq!(
                problem(top.join(dir).join("x")),
                SourceProblem::InTrash,
                "{dir}"
            );
        }
        assert_eq!(problem(top.join(".Trash")), SourceProblem::HoldsTrash);
        assert_eq!(problem(top.join(".Trash-4242")), SourceProblem::HoldsTrash);
        assert!(private.path().join("files/a").exists());
    }

    #[test]
    fn trash_like_names_alone_mean_nothing() {
        let fx = Fx::new();
        // Regular files, a symbolic link to a directory, and directories
        // that are not right below a mount's top directory.
        for name in [".Trash", ".Trash-123"] {
            fs::write(fx.work.join(name), "").unwrap();
            let _ = fx.context.trash(fx.work.join(name)).unwrap();
        }
        fs::create_dir(fx.work.join("target")).unwrap();
        symlink(fx.work.join("target"), fx.work.join(".Trash-7")).unwrap();
        let _ = fx.context.trash(fx.work.join(".Trash-7")).unwrap();
        assert!(
            fx.work.join("target").is_dir(),
            "the link went, not its target"
        );
        fs::create_dir_all(fx.work.join("a/.Trash-5/files")).unwrap();
        fs::write(fx.work.join("a/.Trash-5/files/x"), "x").unwrap();
        let _ = fx
            .context
            .trash(fx.work.join("a/.Trash-5/files/x"))
            .unwrap();
        fs::create_dir_all(fx.work.join("b/.Trash/9")).unwrap();
        let _ = fx.context.trash(fx.work.join("b/.Trash")).unwrap();
        // Names that only look similar.
        for name in [".Trash-", ".Trash-12a", ".Trashy", "Trash"] {
            fs::write(fx.work.join(name), "").unwrap();
            let _ = fx.context.trash(fx.work.join(name)).unwrap();
        }
        // At a top directory, a regular file or link named like this
        // user's own trash roots holds no trash either: not refused (the
        // private trash cannot be made there, so here it finds no trash).
        let top0 = fx.root.join("own");
        fs::create_dir(&top0).unwrap();
        fault::pretend_volume(&top0);
        let own = format!(".Trash-{}", uid());
        fs::write(top0.join(&own), "").unwrap();
        assert!(matches!(
            fx.context.trash(top0.join(&own)),
            Err(TrashError::NoTrash { .. })
        ));
        fs::remove_file(top0.join(&own)).unwrap();
        fs::write(top0.join(".Trash"), "").unwrap();
        let _ = fx.context.trash(top0.join(".Trash")).unwrap();
        symlink(fx.work.join("target"), top0.join(".Trash")).unwrap();
        let _ = fx.context.trash(top0.join(".Trash")).unwrap();
        assert!(fx.work.join("target").is_dir());
        fault::clear();
        // At a top directory, a regular file or link named so is no trash
        // root either.
        let top = fx.root.join("vol");
        fs::create_dir(&top).unwrap();
        fs::write(top.join(".Trash-99"), "").unwrap();
        fault::pretend_volume(&top);
        let _ = fx.context.trash(top.join(".Trash-99")).unwrap();
    }

    #[test]
    fn the_top_directory_is_found_through_unreadable_ancestors() {
        // An execute-only ancestor can be passed but not read: its mount is
        // still told, so the walk goes on to the real top of the mount.
        unprivileged(|| {
            let fx = Fx::new();
            let inner = fx.work.join("x/y");
            fs::create_dir_all(&inner).unwrap();
            fs::set_permissions(fx.work.join("x"), fs::Permissions::from_mode(0o111)).unwrap();
            let fd = sys::open_dir(&inner).unwrap();
            let place = sys::Place::of(std::os::fd::AsFd::as_fd(&fd)).unwrap();
            let found = insert::topdir(&inner, place);
            fs::set_permissions(fx.work.join("x"), fs::Permissions::from_mode(0o755)).unwrap();
            let found = found.unwrap();
            // The reference: the deepest mount point above the directory,
            // from the mount list.
            #[cfg(target_os = "linux")]
            {
                let mounts = mounts(&Limits::default()).unwrap();
                let deepest = mounts
                    .iter()
                    .map(|mount| &mount.mount_point)
                    .filter(|point| inner.starts_with(point))
                    .max_by_key(|point| point.components().count())
                    .unwrap();
                assert_eq!(&found, deepest);
            }
            assert!(!found.starts_with(fx.work.join("x")));
        });
    }

    #[test]
    fn a_sync_failure_after_the_move_is_not_a_failure() {
        let fx = Fx::new();
        let source = fx.file("f", "data");
        fault::fail(Stage::SyncAfterMove, 0);
        let trashed = fx.context.trash(&source).unwrap();
        assert!(matches!(trashed.durability, Durability::NotSynced(_)));
        assert!(!source.exists());
        assert!(fx.trash().join("files/f").exists());
    }

    #[test]
    fn the_clock_is_injected() {
        let fx = Fx::new();
        let context = Context::new(
            fx.home.clone(),
            uid(),
            fx.work.clone(),
            LocalZone::fixed(-3600).unwrap(),
        )
        .unwrap()
        .with_clock(Clock::Fixed(at(0)));
        fx.file("t", "t");
        let _ = context.trash(fx.work.join("t")).unwrap();
        let info = fs::read_to_string(fx.trash().join("info/t.trashinfo")).unwrap();
        assert!(
            info.ends_with("DeletionDate=1969-12-31T23:00:00\n"),
            "{info}"
        );
    }
}

mod volumes {
    use super::*;

    /// A top directory inside the fixture, with an admin-made `.Trash`.
    fn topdir(fx: &Fx, shared_mode: Option<u32>) -> PathBuf {
        let top = fx.root.join("vol");
        fs::create_dir(&top).unwrap();
        if let Some(mode) = shared_mode {
            fs::create_dir(top.join(".Trash")).unwrap();
            fs::set_permissions(top.join(".Trash"), fs::Permissions::from_mode(mode)).unwrap();
        }
        top
    }

    fn private(top: &Path) -> PathBuf {
        top.join(format!(".Trash-{}", uid()))
    }

    fn shared(top: &Path) -> PathBuf {
        top.join(".Trash").join(uid().to_string())
    }

    #[test]
    fn the_shared_layout_is_preferred() {
        let fx = Fx::new();
        let top = topdir(&fx, Some(0o1777));
        fs::write(top.join("doc"), "d").unwrap();
        fault::pretend_volume(&top);
        let trashed = fx.context.trash(top.join("doc")).unwrap();
        assert_eq!(trashed.trash.kind(), TrashKind::Shared);
        assert_eq!(trashed.trash.path(), shared(&top));
        assert!(matches!(
            trashed.fallbacks[0].failure,
            AttemptFailure::OtherMount
        ));
        assert_eq!(mode(&shared(&top)), 0o700);
        assert_eq!(
            fs::read_to_string(shared(&top).join("info/doc.trashinfo")).unwrap(),
            "[Trash Info]\nPath=doc\nDeletionDate=2024-02-29T13:14:15\n",
            "relative to the top directory"
        );
        assert!(!fx.trash().exists());
        let trash = fx.context.open(&fx.context.shared_trash(&top)).unwrap();
        assert_eq!(entries(&trash)[0].origin(), &Origin::Known(top.join("doc")));
    }

    #[test]
    fn without_a_shared_trash_the_private_one_is_made() {
        let fx = Fx::new();
        let top = topdir(&fx, None);
        fs::write(top.join("doc"), "d").unwrap();
        fault::pretend_volume(&top);
        let trashed = fx.context.trash(top.join("doc")).unwrap();
        assert_eq!(trashed.trash.kind(), TrashKind::Private);
        assert_eq!(mode(&private(&top)), 0o700);
        assert!(matches!(
            &trashed.fallbacks[1].failure,
            AttemptFailure::Open(OpenError {
                kind: OpenErrorKind::Absent,
                ..
            })
        ));
        assert!(
            !top.join(".Trash").exists(),
            ".Trash is the administrator's to make"
        );
    }

    #[test]
    fn a_shared_trash_failing_its_checks_is_not_used() {
        for (setup, check) in [
            (0o0777, CheckFailure::NotSticky(0o0777)),
            (0o1777, CheckFailure::Symlink),
        ] {
            let fx = Fx::new();
            let top = topdir(&fx, Some(setup));
            if check == CheckFailure::Symlink {
                fs::remove_dir(top.join(".Trash")).unwrap();
                fs::create_dir(fx.root.join("elsewhere")).unwrap();
                symlink(fx.root.join("elsewhere"), top.join(".Trash")).unwrap();
            }
            fs::write(top.join("doc"), "d").unwrap();
            fault::pretend_volume(&top);
            let trashed = fx.context.trash(top.join("doc")).unwrap();
            assert_eq!(trashed.trash.kind(), TrashKind::Private, "{check:?}");
            let AttemptFailure::Open(err) = &trashed.fallbacks[1].failure else {
                panic!("{:?}", trashed.fallbacks)
            };
            assert!(
                matches!(err.kind, OpenErrorKind::Unsafe(found) if found == check),
                "{err:?}"
            );
            assert_eq!(err.path, top.join(".Trash"));
            assert!(!fx.root.join("elsewhere").join(uid().to_string()).exists());
        }
    }

    #[test]
    fn the_sticky_check_can_be_waived_by_policy() {
        let fx = Fx::new();
        let top = topdir(&fx, Some(0o0777));
        let policy = Policy {
            unchecked_sticky: vec![top.clone()],
            ..Policy::default()
        };
        let context = fx.context.clone().with_policy(policy);
        fs::write(top.join("doc"), "d").unwrap();
        fault::pretend_volume(&top);
        assert_eq!(
            context.trash(top.join("doc")).unwrap().trash.kind(),
            TrashKind::Shared
        );
    }

    #[test]
    fn broken_shared_directories_fall_back_before_the_move() {
        // `$uid` a file; `$uid/files` a symlink; `$uid/info` owned by
        // someone else (as the context sees it): each fails the shared
        // trash before anything moves, and the private one takes the file.
        #[allow(clippy::type_complexity, reason = "a table of named setups")]
        let cases: [(&str, fn(&Path)); 3] = [
            ("uid is a file", |dir| fs::write(dir, "").unwrap()),
            ("files is a symlink", |dir| {
                fs::create_dir(dir).unwrap();
                fs::set_permissions(dir, fs::Permissions::from_mode(0o700)).unwrap();
                symlink("/tmp", dir.join("files")).unwrap();
            }),
            ("info is group-writable", |dir| {
                fs::create_dir(dir).unwrap();
                fs::set_permissions(dir, fs::Permissions::from_mode(0o700)).unwrap();
                fs::create_dir(dir.join("info")).unwrap();
                fs::set_permissions(dir.join("info"), fs::Permissions::from_mode(0o770)).unwrap();
            }),
        ];
        for (what, setup) in cases {
            let fx = Fx::new();
            let top = topdir(&fx, Some(0o1777));
            setup(&shared(&top));
            fs::write(top.join("doc"), "d").unwrap();
            fault::pretend_volume(&top);
            let trashed = fx.context.trash(top.join("doc")).unwrap();
            assert_eq!(trashed.trash.kind(), TrashKind::Private, "{what}");
            assert!(
                matches!(trashed.fallbacks[1].failure, AttemptFailure::Open(_)),
                "{what}"
            );
        }
    }

    #[test]
    fn a_failed_reservation_falls_back() {
        let fx = Fx::new();
        let top = topdir(&fx, Some(0o1777));
        fs::write(top.join("doc"), "d").unwrap();
        fault::pretend_volume(&top);
        // The first write fails (shared); the private trash's succeeds.
        fault::fail(Stage::WriteInfo, 0);
        let trashed = fx.context.trash(top.join("doc")).unwrap();
        assert!(matches!(
            trashed.fallbacks[1].failure,
            AttemptFailure::Metadata(_)
        ));
        assert_eq!(trashed.trash.kind(), TrashKind::Private);
        assert!(
            names(&shared(&top).join("info")).is_empty(),
            "the reservation was removed"
        );
    }

    #[test]
    fn policies_can_refuse_volume_trashing() {
        let fx = Fx::new();
        let top = topdir(&fx, None);
        fs::write(top.join("doc"), "d").unwrap();
        fault::pretend_volume(&top);
        let policy = Policy {
            private_volume_trash: false,
            ..Policy::default()
        };
        let Err(TrashError::NoTrash { attempts, .. }) = fx
            .context
            .clone()
            .with_policy(policy)
            .trash(top.join("doc"))
        else {
            panic!()
        };
        assert_eq!(attempts.len(), 2);
        assert!(!private(&top).exists());
        let policy = Policy {
            volume_trash: false,
            ..Policy::default()
        };
        let Err(TrashError::NoTrash { attempts, .. }) = fx
            .context
            .clone()
            .with_policy(policy)
            .trash(top.join("doc"))
        else {
            panic!()
        };
        assert!(matches!(attempts[1].failure, AttemptFailure::Disabled));
        assert!(top.join("doc").exists(), "never erased, never copied");
    }

    #[test]
    fn an_explicit_volume_trash_takes_only_its_own_files() {
        let fx = Fx::new();
        let top = topdir(&fx, None);
        let outside = fx.file("out", "o");
        let Err(TrashError::NoTrash { attempts, .. }) = fx
            .context
            .trash_into(&fx.context.private_trash(&top), &outside)
        else {
            panic!()
        };
        assert!(matches!(attempts[0].failure, AttemptFailure::OutsideBase));
        assert!(outside.exists());
    }

    #[test]
    fn discovery_lists_both_layouts_and_reports_problems() {
        let fx = Fx::new();
        let top = topdir(&fx, Some(0o1777));
        for name in ["one", "two"] {
            fs::write(top.join(name), name).unwrap();
        }
        let _ = fx
            .context
            .trash_into(&fx.context.shared_trash(&top), top.join("one"))
            .unwrap();
        let _ = fx
            .context
            .trash_into(&fx.context.private_trash(&top), top.join("two"))
            .unwrap();
        let bad = fx.root.join("bad");
        fs::create_dir_all(bad.join(".Trash").join(uid().to_string())).unwrap();
        let discovery = fx.context.discover([&top, &bad]);
        let kinds: Vec<TrashKind> = discovery
            .trashes
            .iter()
            .map(|trash| trash.dir().kind())
            .collect();
        assert_eq!(
            kinds,
            [TrashKind::Shared, TrashKind::Private],
            "no home trash yet"
        );
        assert_eq!(discovery.problems.len(), 1);
        assert_eq!(discovery.problems[0].path, bad.join(".Trash"));
        assert!(!discovery.is_complete());
        let listed: usize = discovery
            .trashes
            .iter()
            .map(|trash| trash.entries().count())
            .sum();
        assert_eq!(listed, 2);
    }
}

mod checks {
    use super::*;

    fn open_error(fx: &Fx, context: &Context) -> OpenError {
        context.open(&fx.context.home_trash()).unwrap_err()
    }

    #[test]
    fn ownership_is_checked_with_the_context_user() {
        // Meaningful as root too: the user checked is the context's.
        let fx = Fx::new();
        fx.file("f", "f");
        let _ = fx.context.trash(fx.work.join("f")).unwrap();
        let other =
            Context::new(&fx.home, uid().wrapping_add(1), &fx.work, LocalZone::utc()).unwrap();
        let err = other.open(&other.home_trash()).unwrap_err();
        assert!(matches!(
            err.kind,
            OpenErrorKind::Unsafe(CheckFailure::Owner { found, expected }) if found == uid() && expected == uid().wrapping_add(1)
        ));
        fx.file("g", "g");
        assert!(matches!(
            other.trash(fx.work.join("g")),
            Err(TrashError::NoTrash { .. })
        ));
        assert!(fx.work.join("g").exists());
    }

    #[test]
    fn writable_and_linked_directories_are_refused() {
        let fx = Fx::new();
        fx.file("f", "f");
        let _ = fx.context.trash(fx.work.join("f")).unwrap();
        let info = fx.trash().join("info");
        fs::set_permissions(&info, fs::Permissions::from_mode(0o733)).unwrap();
        let err = open_error(&fx, &fx.context);
        assert_eq!(err.path, info);
        assert!(matches!(
            err.kind,
            OpenErrorKind::Unsafe(CheckFailure::Writable(0o733))
        ));
        fs::set_permissions(&info, fs::Permissions::from_mode(0o700)).unwrap();
        let files = fx.trash().join("files");
        fs::rename(&files, fx.root.join("moved")).unwrap();
        symlink(fx.root.join("moved"), &files).unwrap();
        assert!(matches!(
            open_error(&fx, &fx.context).kind,
            OpenErrorKind::Unsafe(CheckFailure::Symlink)
        ));
        fs::remove_file(&files).unwrap();
        fs::write(&files, "").unwrap();
        assert!(matches!(
            open_error(&fx, &fx.context).kind,
            OpenErrorKind::Unsafe(CheckFailure::NotDirectory(FileKind::File))
        ));
    }

    #[test]
    fn a_trash_that_exists_but_cannot_be_read_is_reported() {
        unprivileged(|| {
            let fx = Fx::new();
            fx.file("f", "f");
            let _ = fx.context.trash(fx.work.join("f")).unwrap();
            fs::set_permissions(fx.trash(), fs::Permissions::from_mode(0o000)).unwrap();
            let discovery = fx.context.discover(std::iter::empty::<&Path>());
            fs::set_permissions(fx.trash(), fs::Permissions::from_mode(0o700)).unwrap();
            assert!(discovery.trashes.is_empty());
            assert_eq!(discovery.problems.len(), 1, "not shown as an empty trash");
            assert!(
                matches!(&discovery.problems[0].kind, OpenErrorKind::Io(err) if err.kind() == std::io::ErrorKind::PermissionDenied)
            );
        });
    }

    #[test]
    fn a_trash_without_its_subdirectories_is_reported() {
        let fx = Fx::new();
        fs::create_dir_all(fx.trash().join("info")).unwrap();
        for dir in [fx.trash(), fx.trash().join("info")] {
            fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
        }
        let discovery = fx.context.discover(std::iter::empty::<&Path>());
        assert!(discovery.trashes.is_empty());
        assert_eq!(discovery.problems.len(), 1);
        assert_eq!(discovery.problems[0].path, fx.trash().join("files"));
        // No trash directory at all is not a problem.
        let empty = Fx::new();
        assert!(
            empty
                .context
                .discover(std::iter::empty::<&Path>())
                .is_complete()
        );
    }

    #[test]
    fn the_context_anchors_and_validates() {
        assert!(matches!(
            Context::new("d", 1, "relative", LocalZone::utc()),
            Err(ContextError::RelativeCwd(_))
        ));
        let context = Context::new("d", 1, "/w", LocalZone::utc()).unwrap();
        assert_eq!(context.data_home(), Path::new("/w/d"));
        let policy = Policy {
            unchecked_sticky: vec!["mnt".into()],
            ..Policy::default()
        };
        assert_eq!(
            context.with_policy(policy).policy().unchecked_sticky,
            [PathBuf::from("/w/mnt")]
        );
    }
}

mod listing {
    use super::*;

    #[test]
    fn every_payload_and_every_orphan_is_listed() {
        let fx = Fx::new();
        fx.file("whole", "w");
        let _ = fx.context.trash(fx.work.join("whole")).unwrap();
        let (files, info) = (fx.trash().join("files"), fx.trash().join("info"));
        fs::write(files.join("no-info"), "").unwrap();
        fs::write(
            info.join("no-payload.trashinfo"),
            "[Trash Info]\nPath=/x/y\n",
        )
        .unwrap();
        fs::write(files.join("bad-header"), "").unwrap();
        fs::write(
            info.join("bad-header.trashinfo"),
            "[Trash Entry]\nPath=/x\n",
        )
        .unwrap();
        fs::write(files.join("big"), "").unwrap();
        fs::write(info.join("big.trashinfo"), vec![b'x'; 70_000]).unwrap();
        fs::write(files.join("dir-info"), "").unwrap();
        fs::create_dir(info.join("dir-info.trashinfo")).unwrap();
        fs::write(files.join("link-info"), "").unwrap();
        fs::write(
            fx.root.join("target.trashinfo"),
            "[Trash Info]\nPath=/etc/passwd\n",
        )
        .unwrap();
        symlink(
            fx.root.join("target.trashinfo"),
            info.join("link-info.trashinfo"),
        )
        .unwrap();
        fs::write(files.join("dotdot"), "").unwrap();
        fs::write(info.join("dotdot.trashinfo"), "[Trash Info]\nPath=a/../b\n").unwrap();
        fs::write(info.join("unrelated.txt"), "").unwrap();
        fs::write(info.join(".trashinfo"), "").unwrap();
        let listed = entries(&fx.open_home());
        let by_name = |name: &str| {
            listed
                .iter()
                .find(|entry| entry.name() == name)
                .unwrap_or_else(|| panic!("{name}"))
        };
        assert_eq!(
            listed.len(),
            8,
            "{:?}",
            listed.iter().map(Entry::name).collect::<Vec<_>>()
        );
        assert!(by_name("whole").is_intact());
        assert_eq!(
            by_name("no-info").origin(),
            &Origin::Unknown(UnknownOrigin::MetadataMissing)
        );
        assert!(matches!(by_name("no-info").metadata(), Metadata::Missing));
        assert!(matches!(by_name("no-payload").payload(), Payload::Missing));
        assert_eq!(
            by_name("no-payload").origin(),
            &Origin::Known("/x/y".into())
        );
        assert_eq!(
            by_name("bad-header").origin(),
            &Origin::Unknown(UnknownOrigin::Header)
        );
        assert!(matches!(
            by_name("big").metadata(),
            Metadata::TooLarge(70_000)
        ));
        assert!(matches!(
            by_name("dir-info").metadata(),
            Metadata::NotRegular(FileKind::Directory)
        ));
        assert!(
            matches!(
                by_name("link-info").metadata(),
                Metadata::NotRegular(FileKind::Symlink)
            ),
            "not followed"
        );
        assert_eq!(
            by_name("link-info").origin(),
            &Origin::Unknown(UnknownOrigin::MetadataUnusable)
        );
        assert_eq!(
            by_name("dotdot").origin(),
            &Origin::Unknown(UnknownOrigin::PathRefused(PathProblem::ParentComponent))
        );
        assert_eq!(
            by_name("whole").deletion_date(),
            DeletionDate::new(2024, 2, 29, 13, 14, 15)
        );
    }

    #[test]
    fn unreadable_metadata_is_reported() {
        unprivileged(|| {
            let fx = Fx::new();
            fx.file("f", "f");
            let _ = fx.context.trash(fx.work.join("f")).unwrap();
            let info = fx.trash().join("info/f.trashinfo");
            fs::set_permissions(&info, fs::Permissions::from_mode(0o000)).unwrap();
            let trash = fx.open_home();
            let listed = entries(&trash);
            assert!(
                matches!(listed[0].metadata(), Metadata::Unreadable(err) if err.kind() == std::io::ErrorKind::PermissionDenied)
            );
            assert_eq!(
                listed[0].origin(),
                &Origin::Unknown(UnknownOrigin::MetadataUnusable)
            );
            // Its payload can still be erased, and the file it cannot read
            // is still removed with it (its identity was seen).
            let erased = trash
                .erase(&listed[0], &EraseOptions::default(), &|| false)
                .unwrap();
            assert!(matches!(erased.metadata, MetadataRemoval::Removed));
        });
    }

    #[test]
    fn entry_names_stay_inside_the_trash() {
        let fx = Fx::new();
        fx.file("f", "f");
        let _ = fx.context.trash(fx.work.join("f")).unwrap();
        let victim = fx.trash().join("victim");
        fs::write(&victim, "keep").unwrap();
        fs::write(
            fx.trash().join("info/..trashinfo"),
            "[Trash Info]\nPath=/x\n",
        )
        .unwrap();
        fs::write(
            fx.trash().join("info/...trashinfo"),
            "[Trash Info]\nPath=/x\n",
        )
        .unwrap();
        let trash = fx.open_home();
        for bad in ["../victim", "..", ".", "", "a/b", "/etc/passwd", "a\0b"] {
            assert!(
                matches!(trash.entry(OsStr::new(bad)), Err(InvalidName(name)) if name == bad),
                "{bad:?}"
            );
        }
        assert_eq!(fs::read_to_string(&victim).unwrap(), "keep");
        let entry = trash.entry(OsStr::new("f")).unwrap().unwrap();
        assert!(entry.is_intact());
        assert!(trash.entry(OsStr::new("nothing")).unwrap().is_none());
        // The odd info files name no entry.
        assert_eq!(entries(&trash).len(), 1);
    }

    #[test]
    fn a_cut_short_listing_says_so() {
        let fx = Fx::new();
        for i in 0..5 {
            fx.file(&format!("f{i}"), "");
            let _ = fx.context.trash(fx.work.join(format!("f{i}"))).unwrap();
        }
        let limits = Limits {
            max_listed: 3,
            ..Limits::default()
        };
        let context = fx.context.clone().with_limits(limits);
        let trash = context.open(&context.home_trash()).unwrap();
        let items: Vec<_> = trash.entries().collect();
        assert_eq!(items.len(), 4);
        assert!(items[..3].iter().all(Result::is_ok));
        assert!(matches!(items[3], Err(ListError::Limit)));
    }

    #[test]
    fn a_per_volume_absolute_path_follows_the_policy() {
        let fx = Fx::new();
        let top = fx.root.join("vol");
        fs::create_dir(&top).unwrap();
        fs::write(top.join("a"), "").unwrap();
        let dir = fx.context.private_trash(&top);
        let _ = fx.context.trash_into(&dir, top.join("a")).unwrap();
        let info = dir.path().join("info/a.trashinfo");
        let origin = |policy: AbsolutePaths, path: &str| {
            fs::write(&info, format!("[Trash Info]\nPath={path}\n")).unwrap();
            let chosen = Policy {
                absolute_paths: policy,
                ..Policy::default()
            };
            let context = fx.context.clone().with_policy(chosen);
            entries(&context.open(&dir).unwrap())[0].origin().clone()
        };
        let inside = format!("{}/a", top.display());
        assert_eq!(
            origin(AbsolutePaths::UnderTopdir, &inside),
            Origin::Known(top.join("a"))
        );
        assert_eq!(
            origin(AbsolutePaths::UnderTopdir, "/etc/passwd"),
            Origin::Unknown(UnknownOrigin::PathRefused(PathProblem::OutsideTopdir))
        );
        assert_eq!(
            origin(AbsolutePaths::Reject, &inside),
            Origin::Unknown(UnknownOrigin::PathRefused(PathProblem::AbsoluteRefused))
        );
        assert_eq!(
            origin(AbsolutePaths::Accept, "/etc/passwd"),
            Origin::Known("/etc/passwd".into())
        );
        assert_eq!(
            origin(AbsolutePaths::Reject, "a"),
            Origin::Known(top.join("a"))
        );
    }
}

mod restoring {
    use super::*;

    fn trashed(fx: &Fx, rel: &str) -> (Trash, Entry) {
        let source = fx.file(rel, rel);
        let _ = fx.context.trash(&source).unwrap();
        let trash = fx.open_home();
        let entry = entries(&trash)
            .into_iter()
            .find(|entry| entry.origin() == &Origin::Known(source.clone()))
            .unwrap();
        (trash, entry)
    }

    #[test]
    fn back_to_the_origin() {
        let fx = Fx::new();
        let (trash, entry) = trashed(&fx, "a/b");
        let restored = trash.restore(&entry, &RestoreOptions::default()).unwrap();
        assert_eq!(restored.destination, fx.work.join("a/b"));
        assert!(matches!(restored.metadata, MetadataRemoval::Removed));
        assert_eq!(restored.created, Vec::<PathBuf>::new());
        assert_eq!(fs::read_to_string(fx.work.join("a/b")).unwrap(), "a/b");
        assert_eq!(names(&fx.trash().join("files")), Vec::<OsString>::new());
        assert_eq!(names(&fx.trash().join("info")), Vec::<OsString>::new());
    }

    #[test]
    fn conflicts_replace_nothing() {
        let fx = Fx::new();
        let (trash, entry) = trashed(&fx, "c");
        for occupy in [
            |path: &Path| fs::write(path, "new").unwrap(),
            |path: &Path| symlink("/nonexistent", path).unwrap(),
            |path: &Path| fs::create_dir(path).unwrap(),
        ] {
            occupy(&fx.work.join("c"));
            let err = trash
                .restore(&entry, &RestoreOptions::default())
                .unwrap_err();
            assert!(
                matches!(&err.failure, RestoreFailure::Conflict(path) if path == &fx.work.join("c")),
                "{err}"
            );
            assert!(
                fx.trash().join("files/c").exists() && fx.trash().join("info/c.trashinfo").exists()
            );
            let _ =
                fs::remove_file(fx.work.join("c")).or_else(|_| fs::remove_dir(fx.work.join("c")));
        }
    }

    #[test]
    fn missing_parents_are_created_only_when_asked() {
        let fx = Fx::new();
        let (trash, entry) = trashed(&fx, "p/q/r");
        fs::remove_dir_all(fx.work.join("p")).unwrap();
        let err = trash
            .restore(&entry, &RestoreOptions::default())
            .unwrap_err();
        assert!(
            matches!(&err.failure, RestoreFailure::MissingParent(path) if path == &fx.work.join("p/q"))
        );
        assert!(!fx.work.join("p").exists());
        // Created, then removed again when the move fails.
        fault::fail(Stage::Restore, 0);
        let options = RestoreOptions {
            create_parents: true,
            ..RestoreOptions::default()
        };
        let err = trash.restore(&entry, &options).unwrap_err();
        assert!(matches!(err.failure, RestoreFailure::Move(_)));
        assert_eq!(err.left_behind, Vec::<PathBuf>::new());
        assert!(!fx.work.join("p").exists());
        fault::clear();
        let restored = trash.restore(&entry, &options).unwrap();
        assert_eq!(restored.created, [fx.work.join("p"), fx.work.join("p/q")]);
        assert_eq!(fs::read_to_string(fx.work.join("p/q/r")).unwrap(), "p/q/r");
    }

    #[test]
    fn created_parents_are_accounted_for_at_once() {
        let fx = Fx::new();
        let (trash, entry) = trashed(&fx, "u/v/w");
        fs::remove_dir_all(fx.work.join("u")).unwrap();
        let options = RestoreOptions {
            create_parents: true,
            ..RestoreOptions::default()
        };
        // Made, then not openable: removed again, by its identity.
        fault::fail(Stage::OpenCreated, 0);
        let err = trash.restore(&entry, &options).unwrap_err();
        assert!(
            matches!(err.failure, RestoreFailure::Parent { .. }),
            "{err}"
        );
        assert_eq!(err.left_behind, Vec::<PathBuf>::new());
        assert!(!fx.work.join("u").exists());
        // Made, then not identifiable: never removed, reported.
        fault::fail(Stage::IdentifyCreated, 0);
        fault::fail(Stage::OpenCreated, 0);
        let err = trash.restore(&entry, &options).unwrap_err();
        assert_eq!(err.left_behind, [fx.work.join("u")]);
        assert!(fx.work.join("u").is_dir());
        assert!(fx.trash().join("files/w").exists());
    }

    #[test]
    fn destinations_are_checked_as_written() {
        let fx = Fx::new();
        let (trash, entry) = trashed(&fx, "r");
        for suffix in ["a/.", "a/", "a/..", "a//"] {
            let destination = PathBuf::from(format!("{}/{suffix}", fx.work.display()));
            let options = RestoreOptions {
                destination: Some(destination.clone()),
                create_parents: true,
            };
            let err = trash.restore(&entry, &options).unwrap_err();
            assert!(
                matches!(&err.failure, RestoreFailure::InvalidDestination(path) if path == &destination),
                "{suffix}: {err}"
            );
            assert!(!fx.work.join("a").exists(), "{suffix}: nothing was made");
        }
        assert!(fx.trash().join("files/r").exists());
        assert!(fx.trash().join("info/r.trashinfo").exists());
    }

    #[test]
    fn existing_parents_are_never_removed() {
        let fx = Fx::new();
        let (trash, entry) = trashed(&fx, "m/n");
        fs::write(fx.work.join("m/n"), "other").unwrap();
        let options = RestoreOptions {
            create_parents: true,
            ..RestoreOptions::default()
        };
        let err = trash.restore(&entry, &options).unwrap_err();
        assert!(matches!(err.failure, RestoreFailure::Conflict(_)));
        assert_eq!(
            fs::read_to_string(fx.work.join("m/n")).unwrap(),
            "other",
            "not created by the restore: kept"
        );
    }

    #[test]
    fn changed_entries_are_refused() {
        let fx = Fx::new();
        let (trash, entry) = trashed(&fx, "e");
        // The trash info file rewritten in place, same inode.
        let info = fx.trash().join("info/e.trashinfo");
        let inode = fs::metadata(&info).unwrap().ino();
        let mut text = fs::read(&info).unwrap();
        text.extend_from_slice(b"X-Extra=1\n");
        fs::write(&info, &text).unwrap();
        assert_eq!(fs::metadata(&info).unwrap().ino(), inode);
        let err = trash
            .restore(&entry, &RestoreOptions::default())
            .unwrap_err();
        assert!(matches!(
            err.failure,
            RestoreFailure::Changed(Changed::Metadata)
        ));
        // The payload replaced by another file.
        let (trash, entry) = (fx.open_home(), entries(&fx.open_home()).remove(0));
        // (The new file is made while the old one exists: a freed inode
        // number can be reused at once, and identity cannot tell then.)
        let payload = fx.trash().join("files/e");
        fs::write(fx.root.join("swap"), "swapped").unwrap();
        fs::rename(fx.root.join("swap"), &payload).unwrap();
        let err = trash
            .restore(&entry, &RestoreOptions::default())
            .unwrap_err();
        assert!(matches!(
            err.failure,
            RestoreFailure::Changed(Changed::Payload)
        ));
        assert!(!fx.work.join("e").exists());
        // From another trash.
        let other = Fx::new();
        let (other_trash, _) = trashed(&other, "o");
        let err = other_trash
            .restore(&entry, &RestoreOptions::default())
            .unwrap_err();
        assert!(matches!(
            err.failure,
            RestoreFailure::Changed(Changed::Trash)
        ));
    }

    #[test]
    fn an_unknown_origin_needs_a_destination() {
        let fx = Fx::new();
        trashed(&fx, "seed");
        fs::write(fx.trash().join("files/lost"), "lost").unwrap();
        let trash = fx.open_home();
        let entry = entries(&trash)
            .into_iter()
            .find(|entry| entry.name() == "lost")
            .unwrap();
        let err = trash
            .restore(&entry, &RestoreOptions::default())
            .unwrap_err();
        assert!(matches!(
            err.failure,
            RestoreFailure::OriginUnknown(UnknownOrigin::MetadataMissing)
        ));
        for bad in ["relative", "/"] {
            let options = RestoreOptions {
                destination: Some(bad.into()),
                ..RestoreOptions::default()
            };
            assert!(matches!(
                trash.restore(&entry, &options).unwrap_err().failure,
                RestoreFailure::InvalidDestination(_)
            ));
        }
        let options = RestoreOptions {
            destination: Some(fx.work.join("found")),
            ..RestoreOptions::default()
        };
        let restored = trash.restore(&entry, &options).unwrap();
        assert!(matches!(restored.metadata, MetadataRemoval::Absent));
        assert_eq!(fs::read_to_string(fx.work.join("found")).unwrap(), "lost");
    }

    #[test]
    fn metadata_that_cannot_be_removed_is_reported() {
        let fx = Fx::new();
        let (trash, entry) = trashed(&fx, "k");
        fault::fail(Stage::RemoveInfo, 0);
        let restored = trash.restore(&entry, &RestoreOptions::default()).unwrap();
        assert!(matches!(restored.metadata, MetadataRemoval::Kept(_)));
        assert!(fx.work.join("k").exists());
        let listed = entries(&trash);
        assert!(
            matches!(listed[0].payload(), Payload::Missing),
            "now metadata without payload"
        );
    }

    #[test]
    fn a_trash_moved_after_opening_is_still_the_one_used() {
        let fx = Fx::new();
        let (trash, entry) = trashed(&fx, "h");
        fs::rename(fx.trash(), fx.home.join("Moved")).unwrap();
        fs::create_dir_all(fx.trash().join("files")).unwrap();
        let _ = trash.restore(&entry, &RestoreOptions::default()).unwrap();
        assert_eq!(fs::read_to_string(fx.work.join("h")).unwrap(), "h");
        assert_eq!(names(&fx.home.join("Moved/info")), Vec::<OsString>::new());
    }
}

mod erasing {
    use super::*;

    fn tree(fx: &Fx) -> (Trash, Entry) {
        let dir = fx.work.join("tree");
        fs::create_dir_all(dir.join("a/b/c")).unwrap();
        for (path, body) in [("x", "1"), ("a/y", "2"), ("a/b/z", "3"), ("a/b/c/w", "4")] {
            fs::write(dir.join(path), body).unwrap();
        }
        let outside = fx.file("outside", "keep");
        symlink(&outside, dir.join("a/link")).unwrap();
        let _ = fx.context.trash(&dir).unwrap();
        let trash = fx.open_home();
        let entry = entries(&trash).remove(0);
        (trash, entry)
    }

    #[test]
    fn a_tree_is_erased_whole_and_links_are_not_followed() {
        let fx = Fx::new();
        let (trash, entry) = tree(&fx);
        let erased = trash
            .erase(&entry, &EraseOptions::default(), &|| false)
            .unwrap();
        assert_eq!(erased.removed, 9, "5 files and links, 4 directories");
        assert!(matches!(erased.metadata, MetadataRemoval::Removed));
        assert_eq!(fs::read_to_string(fx.work.join("outside")).unwrap(), "keep");
        assert_eq!(names(&fx.trash().join("files")), Vec::<OsString>::new());
        assert_eq!(names(&fx.trash().join("info")), Vec::<OsString>::new());
    }

    #[test]
    fn a_failure_inside_keeps_the_metadata() {
        let fx = Fx::new();
        let (trash, entry) = tree(&fx);
        fault::fail(Stage::EraseItem, 2);
        let err = trash
            .erase(&entry, &EraseOptions::default(), &|| false)
            .unwrap_err();
        fault::clear();
        assert!(matches!(err.failure, EraseFailure::Incomplete));
        assert_eq!(err.problems.len(), 1, "{:?}", err.problems);
        assert!(fx.trash().join("info/tree.trashinfo").exists());
        assert!(fx.trash().join("files/tree").exists());
        // Listed again, it erases.
        let entry = entries(&trash).remove(0);
        trash
            .erase(&entry, &EraseOptions::default(), &|| false)
            .unwrap();
        assert_eq!(names(&fx.trash().join("info")), Vec::<OsString>::new());
    }

    #[test]
    fn the_last_removal_failing_keeps_the_metadata() {
        let fx = Fx::new();
        let (trash, entry) = tree(&fx);
        // Nine items; the tenth removal would be... the ninth is the
        // payload directory itself.
        fault::fail(Stage::EraseItem, 8);
        let err = trash
            .erase(&entry, &EraseOptions::default(), &|| false)
            .unwrap_err();
        assert!(matches!(err.failure, EraseFailure::Incomplete));
        assert_eq!(err.removed, 8);
        assert_eq!(err.problems[0].path, Path::new(""));
        assert!(fx.trash().join("info/tree.trashinfo").exists());
    }

    #[test]
    fn an_unconfirmed_removal_keeps_the_metadata() {
        let fx = Fx::new();
        let (trash, entry) = tree(&fx);
        fault::fail(Stage::Confirm, 0);
        let err = trash
            .erase(&entry, &EraseOptions::default(), &|| false)
            .unwrap_err();
        assert!(matches!(err.failure, EraseFailure::Unconfirmed(_)));
        assert!(!fx.trash().join("files/tree").exists());
        assert!(
            fx.trash().join("info/tree.trashinfo").exists(),
            "gone is not confirmed"
        );
    }

    #[test]
    fn cancelling_stops_with_counts() {
        let fx = Fx::new();
        let (trash, entry) = tree(&fx);
        let calls = std::cell::Cell::new(0);
        let cancel = || {
            calls.set(calls.get() + 1);
            calls.get() > 4
        };
        let err = trash
            .erase(&entry, &EraseOptions::default(), &cancel)
            .unwrap_err();
        assert!(matches!(err.failure, EraseFailure::Cancelled));
        assert!(err.removed < 9);
        assert!(fx.trash().join("info/tree.trashinfo").exists());
        assert!(fx.trash().join("files/tree").exists());
    }

    #[test]
    fn vanishing_descendants_are_counted_not_failed() {
        let fx = Fx::new();
        let dir = fx.work.join("many");
        fs::create_dir(&dir).unwrap();
        for i in 0..50 {
            fs::write(dir.join(format!("f{i}")), "").unwrap();
        }
        let _ = fx.context.trash(&dir).unwrap();
        let trash = fx.open_home();
        let entry = entries(&trash).remove(0);
        let root = fx.trash().join("files/many");
        let calls = std::cell::Cell::new(0);
        // Asked once before starting, once before the first read of the
        // directory; at the third ask a first batch of its names has been
        // read, and another process removes them all. Names already read
        // vanish; ones not yet read are never seen.
        let cancel = || {
            calls.set(calls.get() + 1);
            if calls.get() == 3 {
                for i in 0..50 {
                    let _ = fs::remove_file(root.join(format!("f{i}")));
                }
            }
            false
        };
        let erased = trash
            .erase(&entry, &EraseOptions::default(), &cancel)
            .unwrap();
        assert!(erased.removed + erased.vanished <= 51);
        assert!(erased.vanished >= 1, "{erased:?}");
        assert!(!root.exists());
        assert!(matches!(erased.metadata, MetadataRemoval::Removed));
    }

    #[test]
    fn a_vanished_payload_is_a_change() {
        let fx = Fx::new();
        let (trash, entry) = tree(&fx);
        fs::remove_dir_all(fx.trash().join("files/tree")).unwrap();
        let err = trash
            .erase(&entry, &EraseOptions::default(), &|| false)
            .unwrap_err();
        assert!(matches!(
            err.failure,
            EraseFailure::Changed(Changed::Payload)
        ));
        assert!(fx.trash().join("info/tree.trashinfo").exists());
    }

    #[test]
    fn read_only_directories_follow_the_permission_policy() {
        unprivileged(|| {
            let fx = Fx::new();
            let (trash, entry) = tree(&fx);
            let inner = fx.trash().join("files/tree/a/b");
            fs::set_permissions(&inner, fs::Permissions::from_mode(0o500)).unwrap();
            let err = trash
                .erase(&entry, &EraseOptions::default(), &|| false)
                .unwrap_err();
            assert!(matches!(err.failure, EraseFailure::Incomplete));
            assert_eq!(err.permissions_changed, 0);
            assert_eq!(mode(&inner), 0o500, "left as it was");
            assert!(fx.trash().join("info/tree.trashinfo").exists());
            let entry = entries(&trash).remove(0);
            let options = EraseOptions {
                permissions: Permissions::GrantOwner,
            };
            let erased = trash.erase(&entry, &options, &|| false).unwrap();
            assert_eq!(erased.permissions_changed, 1);
        });
    }

    #[test]
    fn an_entry_without_payload_is_not_erased() {
        let fx = Fx::new();
        fs::create_dir_all(fx.trash().join("files")).unwrap();
        fs::create_dir_all(fx.trash().join("info")).unwrap();
        for dir in [
            fx.trash(),
            fx.trash().join("files"),
            fx.trash().join("info"),
        ] {
            fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
        }
        fs::write(
            fx.trash().join("info/r.trashinfo"),
            "[Trash Info]\nPath=/r\n",
        )
        .unwrap();
        let trash = fx.open_home();
        let entry = entries(&trash).remove(0);
        let err = trash
            .erase(&entry, &EraseOptions::default(), &|| false)
            .unwrap_err();
        assert!(matches!(err.failure, EraseFailure::PayloadMissing));
    }

    #[test]
    fn depth_and_entry_limits_stop_erasing() {
        let fx = Fx::new();
        let (_, _) = tree(&fx);
        let limits = Limits {
            max_depth: 2,
            ..Limits::default()
        };
        let context = fx.context.clone().with_limits(limits);
        let trash = context.open(&context.home_trash()).unwrap();
        let entry = entries(&trash).remove(0);
        let err = trash
            .erase(&entry, &EraseOptions::default(), &|| false)
            .unwrap_err();
        assert!(
            err.problems
                .iter()
                .any(|problem| matches!(problem.kind, EraseProblemKind::TooDeep))
        );
        assert!(fx.trash().join("info/tree.trashinfo").exists());
        let limits = Limits {
            max_tree_entries: 3,
            ..Limits::default()
        };
        let context = fx.context.clone().with_limits(limits);
        let trash = context.open(&context.home_trash()).unwrap();
        let entry = entries(&trash).remove(0);
        let err = trash
            .erase(&entry, &EraseOptions::default(), &|| false)
            .unwrap_err();
        assert!(
            err.problems
                .iter()
                .any(|problem| matches!(problem.kind, EraseProblemKind::TooMany))
        );
        assert!(fx.trash().join("info/tree.trashinfo").exists());
    }
}

mod maintenance {
    use super::*;

    fn reservation(fx: &Fx) -> Trash {
        fx.file("seed", "");
        let _ = fx.context.trash(fx.work.join("seed")).unwrap();
        fs::write(
            fx.trash().join("info/pending.trashinfo"),
            "[Trash Info]\nPath=/p\n",
        )
        .unwrap();
        fx.open_home()
    }

    #[test]
    fn a_reservation_survives_everything_but_an_explicit_discard() {
        let fx = Fx::new();
        let trash = reservation(&fx);
        let info = fx.trash().join("info/pending.trashinfo");
        // Old, as a paused writer's would be.
        let old = fs::File::options().write(true).open(&info).unwrap();
        old.set_modified(at(0)).unwrap();
        let _ = trash.size(&SizeOptions::default(), &|| false).unwrap();
        let seed = entries(&trash)
            .into_iter()
            .find(|entry| entry.name() == "seed")
            .unwrap();
        trash
            .erase(&seed, &EraseOptions::default(), &|| false)
            .unwrap();
        assert!(info.exists(), "listing, sizing and erasing others leave it");
        let pending = entries(&trash).remove(0);
        assert!(matches!(pending.payload(), Payload::Missing));
        trash
            .discard_metadata(&pending, Quiescent::asserted())
            .unwrap();
        assert!(!info.exists());
    }

    #[test]
    fn an_uninspectable_payload_is_not_taken_as_absent() {
        unprivileged(|| {
            let fx = Fx::new();
            let trash = reservation(&fx);
            let pending = entries(&trash)
                .into_iter()
                .find(|entry| entry.name() == "pending")
                .unwrap();
            // The paused writer finishes; then `files` cannot be searched.
            fs::write(fx.trash().join("files/pending"), "late").unwrap();
            fs::set_permissions(fx.trash().join("files"), fs::Permissions::from_mode(0o600))
                .unwrap();
            let result = trash.discard_metadata(&pending, Quiescent::asserted());
            fs::set_permissions(fx.trash().join("files"), fs::Permissions::from_mode(0o700))
                .unwrap();
            assert!(
                matches!(result, Err(DiscardError::Changed(Changed::Unverifiable))),
                "{result:?}"
            );
            assert!(fx.trash().join("info/pending.trashinfo").exists());
        });
    }

    #[test]
    fn a_discard_rechecks_the_entry() {
        let fx = Fx::new();
        let trash = reservation(&fx);
        let pending = entries(&trash)
            .into_iter()
            .find(|entry| entry.name() == "pending")
            .unwrap();
        // The paused writer finishes: its payload arrives.
        fs::write(fx.trash().join("files/pending"), "late").unwrap();
        assert!(matches!(
            trash.discard_metadata(&pending, Quiescent::asserted()),
            Err(DiscardError::Changed(Changed::Payload))
        ));
        assert!(fx.trash().join("info/pending.trashinfo").exists());
        let whole = entries(&trash)
            .into_iter()
            .find(|entry| entry.name() == "pending")
            .unwrap();
        assert!(matches!(
            trash.discard_metadata(&whole, Quiescent::asserted()),
            Err(DiscardError::PayloadPresent)
        ));
        fs::remove_file(fx.trash().join("files/pending")).unwrap();
        let pending = entries(&trash)
            .into_iter()
            .find(|entry| entry.name() == "pending")
            .unwrap();
        fs::write(
            fx.trash().join("info/pending.trashinfo"),
            "[Trash Info]\nPath=/other\n",
        )
        .unwrap();
        assert!(matches!(
            trash.discard_metadata(&pending, Quiescent::asserted()),
            Err(DiscardError::Changed(Changed::Metadata))
        ));
    }
}

mod sizes {
    use super::*;

    /// `du -s -B1` of `path`: the reference for allocated sizes.
    fn du(path: &Path) -> u64 {
        let out = std::process::Command::new("du")
            .arg("-s")
            .arg("-B1")
            .arg(path)
            .output()
            .unwrap();
        assert!(out.status.success());
        String::from_utf8(out.stdout)
            .unwrap()
            .split_whitespace()
            .next()
            .unwrap()
            .parse()
            .unwrap()
    }

    #[test]
    fn the_cache_format() {
        let parsed = parse_directory_sizes(
            b"16384 15803468 Documents\n8192 -5 Another%20Folder\n1 2 raw space\n3 4 %25%0Aodd%E9\n\
              x 1 bad\n1 x bad\n1 2\n1 2 a/b\n1 2 a%2Fb\n1 2 .\n1 2 %2E%2E\n1 2 nul%00\n1 2 \n\
              99999999999999999999 1 big\n1 2 Documents\n1 2 bad%zz\n",
            usize::MAX,
        );
        assert_eq!((parsed.rejected, parsed.truncated), (12, false));
        let (entries, problems) = (parsed.entries, parsed.problems);
        let names: Vec<&[u8]> = entries.iter().map(|entry| entry.name.as_bytes()).collect();
        assert_eq!(
            names,
            [
                &b"Documents"[..],
                b"Another Folder",
                b"raw space",
                b"%\nodd\xE9"
            ]
        );
        assert_eq!((entries[0].size, entries[0].mtime), (16384, 15_803_468));
        assert_eq!(entries[1].mtime, -5);
        let kinds: Vec<(usize, CacheProblemKind)> = problems
            .into_iter()
            .map(|problem| (problem.line, problem.kind))
            .collect();
        assert_eq!(
            kinds,
            [
                (5, CacheProblemKind::Size),
                (6, CacheProblemKind::Mtime),
                (7, CacheProblemKind::Fields),
                (8, CacheProblemKind::Name),
                (9, CacheProblemKind::Name),
                (10, CacheProblemKind::Name),
                (11, CacheProblemKind::Name),
                (12, CacheProblemKind::Name),
                (13, CacheProblemKind::Name),
                (14, CacheProblemKind::Size),
                (15, CacheProblemKind::Duplicate),
                (16, CacheProblemKind::Escape(EscapeProblem::Percent(3))),
            ]
        );
        let written = format_directory_sizes(&entries);
        assert_eq!(
            written,
            b"16384 15803468 Documents\n8192 -5 Another%20Folder\n1 2 raw%20space\n3 4 %25%0Aodd%E9\n"
        );
        let again = parse_directory_sizes(&written, usize::MAX);
        assert_eq!((again.entries, again.rejected), (entries, 0));
    }

    fn sized(fx: &Fx) -> Trash {
        let dir = fx.work.join("big");
        fs::create_dir_all(dir.join("sub")).unwrap();
        fs::write(dir.join("one"), vec![1; 10_000]).unwrap();
        fs::write(dir.join("sub/two"), vec![2; 30_000]).unwrap();
        fs::hard_link(dir.join("one"), dir.join("sub/one-again")).unwrap();
        let _ = fx.context.trash(&dir).unwrap();
        fx.file("plain", &"p".repeat(5_000));
        let _ = fx.context.trash(fx.work.join("plain")).unwrap();
        fx.open_home()
    }

    fn cache(fx: &Fx) -> String {
        fs::read_to_string(fx.trash().join("directorysizes")).unwrap()
    }

    #[test]
    fn sizes_match_du_and_are_cached() {
        let fx = Fx::new();
        let trash = sized(&fx);
        let files = fx.trash().join("files");
        let expected =
            du(&files.join("big")) + fs::metadata(files.join("plain")).unwrap().blocks() * 512;
        let report = trash.size(&SizeOptions::default(), &|| false).unwrap();
        assert!(report.is_complete(), "{:?}", report.problems);
        assert_eq!(report.bytes, expected);
        assert!(matches!(report.cache_read, CacheRead::Missing));
        assert!(matches!(
            report.cache_write,
            CacheWrite::Written(Durability::Synced)
        ));
        let mtime = fs::metadata(fx.trash().join("info/big.trashinfo"))
            .unwrap()
            .mtime();
        assert_eq!(
            cache(&fx),
            format!("{} {mtime} big\n", du(&files.join("big")))
        );
        assert_eq!(mode(&fx.trash().join("directorysizes")) & 0o077, 0);
        // The cache is used while the trash info file's time is the same:
        // here a stale size, by design.
        fs::write(files.join("big/sub/more"), vec![3; 50_000]).unwrap();
        let cached = trash.size(&SizeOptions::default(), &|| false).unwrap();
        assert_eq!(cached.bytes, expected);
        assert!(matches!(
            cached.cache_read,
            CacheRead::Read { entries: 1, .. }
        ));
        // A new trash info time invalidates it.
        let info = fs::File::options()
            .write(true)
            .open(fx.trash().join("info/big.trashinfo"))
            .unwrap();
        info.set_modified(at(WHEN + 60)).unwrap();
        let fresh = trash.size(&SizeOptions::default(), &|| false).unwrap();
        assert_eq!(
            fresh.bytes,
            du(&files.join("big")) + fs::metadata(files.join("plain")).unwrap().blocks() * 512
        );
        assert_eq!(
            cache(&fx),
            format!("{} {} big\n", du(&files.join("big")), WHEN + 60)
        );
        // Ignored: measured anyway, nothing written.
        fs::write(fx.trash().join("directorysizes"), "1 1 big\n").unwrap();
        let ignored = trash
            .size(
                &SizeOptions {
                    cache: CacheUse::Ignore,
                },
                &|| false,
            )
            .unwrap();
        assert_eq!(ignored.bytes, fresh.bytes);
        assert_eq!(cache(&fx), "1 1 big\n");
    }

    #[test]
    fn a_bad_cache_costs_only_a_measurement() {
        let fx = Fx::new();
        let trash = sized(&fx);
        let truth = trash
            .size(
                &SizeOptions {
                    cache: CacheUse::Ignore,
                },
                &|| false,
            )
            .unwrap()
            .bytes;
        fs::write(fx.trash().join("directorysizes"), "garbage\n1 2 /etc\n").unwrap();
        let report = trash.size(&SizeOptions::default(), &|| false).unwrap();
        assert_eq!(report.bytes, truth);
        assert!(matches!(
            &report.cache_read,
            CacheRead::Read {
                entries: 0,
                rejected: 2,
                ..
            }
        ));
        // A symlinked cache is not followed, and is replaced as a link.
        let outside = fx.root.join("outside");
        fs::write(&outside, "1 1 big\n").unwrap();
        fs::remove_file(fx.trash().join("directorysizes")).unwrap();
        symlink(&outside, fx.trash().join("directorysizes")).unwrap();
        let report = trash.size(&SizeOptions::default(), &|| false).unwrap();
        assert_eq!(report.bytes, truth);
        assert!(matches!(report.cache_read, CacheRead::Unusable(_)));
        assert_eq!(fs::read_to_string(&outside).unwrap(), "1 1 big\n");
        assert!(
            !fs::symlink_metadata(fx.trash().join("directorysizes"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
        // A failed write leaves the old cache and no temporary file.
        let before = cache(&fx);
        fault::fail(Stage::WriteCache, 0);
        let report = trash.size(&SizeOptions::default(), &|| false).unwrap();
        assert!(matches!(report.cache_write, CacheWrite::Failed(_)));
        assert_eq!(cache(&fx), before);
        assert_eq!(names(&fx.trash()), ["directorysizes", "files", "info"]);
        // Listing and restoring do not look at it.
        fs::write(fx.trash().join("directorysizes"), "\0\0\0").unwrap();
        assert_eq!(entries(&trash).len(), 2);
    }

    #[test]
    fn concurrent_updates_leave_a_valid_cache() {
        let fx = Fx::new();
        let trash = sized(&fx);
        let truth = trash
            .size(
                &SizeOptions {
                    cache: CacheUse::Ignore,
                },
                &|| false,
            )
            .unwrap()
            .bytes;
        std::thread::scope(|scope| {
            for _ in 0..8 {
                scope.spawn(|| {
                    for _ in 0..10 {
                        assert_eq!(
                            trash
                                .size(&SizeOptions::default(), &|| false)
                                .unwrap()
                                .bytes,
                            truth
                        );
                    }
                });
            }
        });
        let parsed = parse_directory_sizes(cache(&fx).as_bytes(), usize::MAX);
        assert_eq!(parsed.rejected, 0);
        assert_eq!(parsed.entries.len(), 1);
        assert_eq!(names(&fx.trash()), ["directorysizes", "files", "info"]);
    }

    #[test]
    fn hostile_caches_take_bounded_memory() {
        let newlines = vec![b'\n'; 1 << 20];
        let parsed = parse_directory_sizes(&newlines, 100);
        assert_eq!(
            parsed.rejected,
            1 << 20,
            "each newline ends an empty, bad line"
        );
        assert_eq!(parsed.problems.len(), MAX_CACHE_PROBLEMS);
        assert_eq!(parsed.problems[0].line, 1);
        let many: Vec<u8> = (0..1000)
            .flat_map(|i| format!("1 1 n{i}\n").into_bytes())
            .collect();
        let parsed = parse_directory_sizes(&many, 10);
        assert_eq!(parsed.entries.len(), 10);
        assert!(parsed.truncated);
        assert_eq!(parsed.rejected, 0);
    }

    #[test]
    fn a_directory_without_metadata_is_measured_not_cached() {
        let fx = Fx::new();
        let trash = sized(&fx);
        fs::remove_file(fx.trash().join("info/big.trashinfo")).unwrap();
        let report = trash.size(&SizeOptions::default(), &|| false).unwrap();
        assert!(report.bytes > 0);
        assert_eq!(cache(&fx), "");
    }
}

mod mount_list {
    use super::*;

    #[test]
    fn mountinfo_lines() {
        let text = b"22 1 8:1 / / rw,relatime shared:1 - ext4 /dev/sda1 rw\n\
            35 22 0:31 / /media/my\\040disk rw master:2 shared:3 - vfat /dev/sdb1 rw\n\
            40 22 0:40 /sub /srv/bind rw - btrfs /dev/sdc rw\n";
        let mounts = parse_mountinfo(text).unwrap();
        assert_eq!(mounts.len(), 3);
        assert_eq!(mounts[1].mount_point, Path::new("/media/my disk"));
        assert_eq!((mounts[1].id, mounts[1].parent), (35, 22));
        assert_eq!(mounts[1].fs_type, "vfat");
        assert_eq!(mounts[2].source, Path::new("/dev/sdc"));
        for bad in [
            &b"22 1 8:1 / / rw shared:1 ext4 /dev/sda1 rw\n"[..],
            b"x 1 8:1 / / rw - ext4 a rw\n",
            b"1 1 0:1 / rel rw - t s o\n",
            b"1 1 0:1 / /a\\09 rw - t s o\n",
        ] {
            assert!(
                matches!(parse_mountinfo(bad), Err(MountsError::Malformed(1))),
                "{:?}",
                String::from_utf8_lossy(bad)
            );
        }
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn this_process_has_a_root_mount() {
        let mounts = mounts(&Limits::default()).unwrap();
        assert!(
            mounts
                .iter()
                .any(|mount| mount.mount_point == Path::new("/"))
        );
    }
}

mod planning {
    use super::*;

    /// Every path below `root`, with its mode: what a query must leave as
    /// it is.
    fn snapshot(root: &Path) -> Vec<(PathBuf, u32)> {
        let mut all = Vec::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            for item in fs::read_dir(&dir).unwrap() {
                let path = item.unwrap().path();
                let meta = fs::symlink_metadata(&path).unwrap();
                if meta.is_dir() {
                    stack.push(path.clone());
                }
                all.push((path, meta.mode()));
            }
        }
        all.sort();
        all
    }

    /// The plan for `path`, checking that finding it changed nothing.
    fn plan(fx: &Fx, context: &Context, path: &Path) -> TrashPlan {
        let before = snapshot(&fx.root);
        let plan = context.plan(path).unwrap();
        assert_eq!(snapshot(&fx.root), before, "the query changed nothing");
        plan
    }

    fn kinds(plan: &TrashPlan) -> Vec<TrashKind> {
        plan.candidates
            .iter()
            .map(|candidate| candidate.trash.kind())
            .collect()
    }

    fn top(fx: &Fx, shared_mode: Option<u32>) -> PathBuf {
        let top = fx.root.join("vol");
        fs::create_dir(&top).unwrap();
        if let Some(mode) = shared_mode {
            fs::create_dir(top.join(".Trash")).unwrap();
            fs::set_permissions(top.join(".Trash"), fs::Permissions::from_mode(mode)).unwrap();
        }
        fs::write(top.join("doc"), "d").unwrap();
        top
    }

    fn open_failure(state: &CandidateState) -> &OpenError {
        match state {
            CandidateState::Unusable(AttemptFailure::Open(err)) => err,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn the_home_trash_on_the_same_mount() {
        let fx = Fx::new();
        let source = fx.file("f", "f");
        let first = plan(&fx, &fx.context, &source);
        assert_eq!(kinds(&first), [TrashKind::Home]);
        assert!(first.skipped.is_empty(), "{:?}", first.skipped);
        assert!(
            matches!(&first.candidates[0].state, CandidateState::WouldCreate(path) if path == &fx.home),
            "{:?}",
            first.candidates[0].state
        );
        assert!(!fx.home.exists());
        // Once it exists, as it is.
        let _ = fx.context.trash(fx.file("g", "g")).unwrap();
        let second = plan(&fx, &fx.context, &source);
        assert!(matches!(second.candidates[0].state, CandidateState::Usable));
        assert_eq!(second.destination().unwrap().trash.kind(), TrashKind::Home);
    }

    #[test]
    fn an_existing_volume_trash() {
        let fx = Fx::new();
        let top = top(&fx, None);
        fs::write(top.join("seed"), "s").unwrap();
        let _ = fx
            .context
            .trash_into(&fx.context.private_trash(&top), top.join("seed"))
            .unwrap();
        fault::pretend_volume(&top);
        let plan = plan(&fx, &fx.context, &top.join("doc"));
        assert!(matches!(
            plan.skipped[0].failure,
            AttemptFailure::OtherMount
        ));
        assert_eq!(kinds(&plan), [TrashKind::Shared, TrashKind::Private]);
        let shared = open_failure(&plan.candidates[0].state);
        assert!(matches!(shared.kind, OpenErrorKind::Absent));
        assert_eq!(shared.path, top.join(".Trash"));
        assert!(matches!(plan.candidates[1].state, CandidateState::Usable));
        // And trashing agrees.
        let trashed = fx.context.trash(top.join("doc")).unwrap();
        assert_eq!(trashed.trash, plan.destination().unwrap().trash);
    }

    #[test]
    fn volume_trashes_that_would_be_created() {
        // In a sticky `.Trash`: its `$uid`.
        let fx = Fx::new();
        let top = top(&fx, Some(0o1777));
        fault::pretend_volume(&top);
        let plan = plan(&fx, &fx.context, &top.join("doc"));
        let shared = top.join(".Trash").join(uid().to_string());
        assert!(
            matches!(&plan.candidates[0].state, CandidateState::WouldCreate(path) if path == &shared)
        );
        assert!(matches!(&plan.candidates[1].state,
            CandidateState::WouldCreate(path) if path == &top.join(format!(".Trash-{}", uid()))));
        let trashed = fx.context.trash(top.join("doc")).unwrap();
        assert_eq!(trashed.trash.kind(), TrashKind::Shared);
        // Without `.Trash`: the `.Trash-$uid` fallback.
        let fx = Fx::new();
        let top = self::top(&fx, None);
        fault::pretend_volume(&top);
        let plan = self::plan(&fx, &fx.context, &top.join("doc"));
        assert!(matches!(
            open_failure(&plan.candidates[0].state).kind,
            OpenErrorKind::Absent
        ));
        let destination = plan.destination().unwrap();
        assert_eq!(destination.trash.kind(), TrashKind::Private);
        assert!(
            matches!(&destination.state, CandidateState::WouldCreate(path)
            if path == &top.join(format!(".Trash-{}", uid())))
        );
    }

    #[test]
    fn shared_directories_failing_their_checks() {
        let fx = Fx::new();
        let top = top(&fx, Some(0o0777));
        fault::pretend_volume(&top);
        let plan = plan(&fx, &fx.context, &top.join("doc"));
        assert!(matches!(
            open_failure(&plan.candidates[0].state).kind,
            OpenErrorKind::Unsafe(CheckFailure::NotSticky(0o0777))
        ));
        let fx = Fx::new();
        let top = self::top(&fx, None);
        fs::create_dir(fx.root.join("elsewhere")).unwrap();
        symlink(fx.root.join("elsewhere"), top.join(".Trash")).unwrap();
        fault::pretend_volume(&top);
        let plan = self::plan(&fx, &fx.context, &top.join("doc"));
        assert!(matches!(
            open_failure(&plan.candidates[0].state).kind,
            OpenErrorKind::Unsafe(CheckFailure::Symlink)
        ));
        assert!(matches!(
            plan.candidates[1].state,
            CandidateState::WouldCreate(_)
        ));
    }

    #[test]
    fn an_unwritable_top_directory() {
        unprivileged(|| {
            let fx = Fx::new();
            let top = top(&fx, None);
            fs::set_permissions(&top, fs::Permissions::from_mode(0o555)).unwrap();
            fault::pretend_volume(&top);
            let plan = plan(&fx, &fx.context, &top.join("doc"));
            let private = open_failure(&plan.candidates[1].state);
            assert!(
                matches!(&private.kind, OpenErrorKind::Io(err) if err.kind() == std::io::ErrorKind::PermissionDenied),
                "{private:?}"
            );
            assert_eq!(private.path, top.join(format!(".Trash-{}", uid())));
            assert!(plan.destination().is_none());
            // And trashing agrees.
            assert!(matches!(
                fx.context.trash(top.join("doc")),
                Err(TrashError::NoTrash { .. })
            ));
            fs::set_permissions(&top, fs::Permissions::from_mode(0o755)).unwrap();
        });
    }

    #[test]
    fn an_existing_trash_that_cannot_be_written() {
        unprivileged(|| {
            let fx = Fx::new();
            let _ = fx.context.trash(fx.file("seed", "s")).unwrap();
            fs::set_permissions(fx.trash().join("info"), fs::Permissions::from_mode(0o500))
                .unwrap();
            let plan = plan(&fx, &fx.context, &fx.file("f", "f"));
            let home = open_failure(&plan.candidates[0].state);
            fs::set_permissions(fx.trash().join("info"), fs::Permissions::from_mode(0o700))
                .unwrap();
            assert_eq!(home.path, fx.trash().join("info"));
        });
    }

    #[test]
    fn volume_trashing_disabled_by_policy() {
        let fx = Fx::new();
        let top = top(&fx, Some(0o1777));
        fault::pretend_volume(&top);
        let context = fx.context.clone().with_policy(Policy {
            volume_trash: false,
            ..Policy::default()
        });
        let plan = plan(&fx, &context, &top.join("doc"));
        assert!(plan.candidates.is_empty());
        let failures: Vec<&AttemptFailure> = plan
            .skipped
            .iter()
            .map(|attempt| &attempt.failure)
            .collect();
        assert!(matches!(
            failures[..],
            [
                AttemptFailure::OtherMount,
                AttemptFailure::Disabled,
                AttemptFailure::Disabled
            ]
        ));
        assert!(plan.destination().is_none());
    }

    #[test]
    fn a_dangling_link_is_not_a_directory_to_create() {
        // `$XDG_DATA_HOME` a dangling link, and below one: trashing cannot
        // create through it, and the query says so.
        for (link, data_home) in [
            ("data", "data"),
            ("data", "data/"),
            ("data", "data/."),
            ("link", "link/data"),
            ("link", "link/./data/"),
        ] {
            let fx = Fx::new();
            symlink(fx.root.join("nowhere"), fx.root.join(link)).unwrap();
            let context = Context::new(fx.root.join(data_home), uid(), &fx.work, LocalZone::utc())
                .unwrap()
                .with_clock(Clock::Fixed(at(WHEN)));
            let source = fx.file("f", "f");
            let plan = plan(&fx, &context, &source);
            let home = open_failure(&plan.candidates[0].state);
            assert_eq!(home.path, fx.root.join(link), "{data_home}");
            assert!(
                matches!(&home.kind, OpenErrorKind::Io(err) if err.kind() == std::io::ErrorKind::NotADirectory),
                "{data_home}: {home:?}"
            );
            assert!(plan.destination().is_none());
            assert!(
                matches!(context.trash(&source), Err(TrashError::NoTrash { .. })),
                "{data_home}"
            );
            assert!(!fx.root.join("nowhere").exists());
        }
    }

    #[test]
    fn sources_trashing_refuses() {
        let fx = Fx::new();
        let _ = fx.context.trash(fx.file("seed", "s")).unwrap();
        let before = snapshot(&fx.root);
        assert!(matches!(
            fx.context.plan(fx.trash()),
            Err(TrashError::InvalidSource {
                problem: SourceProblem::HoldsTrash,
                ..
            })
        ));
        assert!(matches!(
            fx.context.plan(fx.trash().join("files/seed")),
            Err(TrashError::InvalidSource {
                problem: SourceProblem::InTrash,
                ..
            })
        ));
        assert!(matches!(
            fx.context.plan(fx.work.join("missing")),
            Err(TrashError::SourceMissing { .. })
        ));
        assert!(matches!(
            fx.context.plan("/"),
            Err(TrashError::InvalidSource {
                problem: SourceProblem::NoName,
                ..
            })
        ));
        assert_eq!(snapshot(&fx.root), before);
    }
}

mod fallbacks {
    use super::*;

    /// Refuses the native non-replacing rename the next `times` times on
    /// this thread, as a file system without it would.
    fn refuse_native(times: usize) {
        for _ in 0..times {
            fault::fail(Stage::NativeRefused, 0);
        }
    }

    fn with(fx: &Fx, fallbacks: Fallbacks) -> Context {
        fx.context.clone().with_policy(Policy {
            fallbacks,
            ..Policy::default()
        })
    }

    fn info_names(fx: &Fx) -> Vec<OsString> {
        names(&fx.trash().join("info"))
    }

    fn file_names(fx: &Fx) -> Vec<OsString> {
        names(&fx.trash().join("files"))
    }

    #[test]
    fn the_native_rename_is_preferred() {
        let fx = Fx::new();
        let trashed = fx.context.trash(fx.file("f", "f")).unwrap();
        assert_eq!(trashed.method, MoveMethod::Rename);
        assert!(trashed.method.is_atomic());
    }

    #[test]
    fn a_file_is_linked_then_unlinked() {
        let fx = Fx::new();
        let source = fx.file("f", "data");
        refuse_native(1);
        let trashed = fx.context.trash(&source).unwrap();
        assert_eq!(trashed.method, MoveMethod::Link);
        assert!(!trashed.method.is_atomic());
        assert!(matches!(trashed.durability, Durability::Synced));
        assert!(!source.exists());
        assert_eq!(
            fs::read_to_string(fx.trash().join("files/f")).unwrap(),
            "data"
        );
        assert_eq!(fs::metadata(fx.trash().join("files/f")).unwrap().nlink(), 1);
        assert_eq!(info_names(&fx), ["f.trashinfo"]);
    }

    #[test]
    fn a_symbolic_link_is_linked_as_itself() {
        let fx = Fx::new();
        let target = fx.file("target", "keep");
        symlink(&target, fx.work.join("link")).unwrap();
        refuse_native(1);
        let trashed = fx.context.trash(fx.work.join("link")).unwrap();
        assert_eq!(trashed.method, MoveMethod::Link);
        assert_eq!(
            fs::read_link(fx.trash().join("files/link")).unwrap(),
            target
        );
        assert!(fs::symlink_metadata(fx.work.join("link")).is_err());
        assert_eq!(fs::read_to_string(&target).unwrap(), "keep");
    }

    #[test]
    fn a_directory_needs_the_placeholder_opt_in() {
        let fx = Fx::new();
        fs::create_dir_all(fx.work.join("d/sub")).unwrap();
        fs::write(fx.work.join("d/sub/x"), "x").unwrap();
        refuse_native(1);
        let Err(TrashError::Unsupported { unsupported, .. }) = fx.context.trash(fx.work.join("d"))
        else {
            panic!()
        };
        assert!(unsupported.cause.is_some());
        assert_eq!(fs::read_to_string(fx.work.join("d/sub/x")).unwrap(), "x");
        assert_eq!(info_names(&fx), Vec::<OsString>::new());
        assert_eq!(file_names(&fx), Vec::<OsString>::new());
        // Strict: not even files.
        refuse_native(1);
        assert!(matches!(
            with(&fx, Fallbacks::Native).trash(fx.file("f", "f")),
            Err(TrashError::Unsupported { .. })
        ));
        assert!(fx.work.join("f").exists());
        // Opted in: over a placeholder.
        refuse_native(1);
        let trashed = with(&fx, Fallbacks::LinkAndPlaceholder)
            .trash(fx.work.join("d"))
            .unwrap();
        assert_eq!(trashed.method, MoveMethod::Placeholder);
        assert_eq!(
            fs::read_to_string(fx.trash().join("files/d/sub/x")).unwrap(),
            "x"
        );
        assert!(!fx.work.join("d").exists());
    }

    #[test]
    fn names_taken_meanwhile_are_never_replaced() {
        for (stage, directory) in [
            (Stage::TakeNameFile, false),
            (Stage::TakeNameDir, false),
            (Stage::TakeNameLink, false),
            (Stage::TakeNameFile, true),
            (Stage::TakeNameDir, true),
            (Stage::TakeNameLink, true),
        ] {
            let fx = Fx::new();
            let source = if directory {
                fs::create_dir(fx.work.join("p")).unwrap();
                fs::write(fx.work.join("p/inner"), "inner").unwrap();
                fx.work.join("p")
            } else {
                fx.file("p", "payload")
            };
            refuse_native(2);
            fault::fail(stage, 0);
            let trashed = with(&fx, Fallbacks::LinkAndPlaceholder)
                .trash(&source)
                .unwrap_or_else(|err| panic!("{stage:?} {directory}: {err}"));
            assert_eq!(trashed.name, "p.2", "{stage:?} {directory}");
            let taken = fx.trash().join("files/p");
            let meta = fs::symlink_metadata(&taken).unwrap();
            match stage {
                Stage::TakeNameFile => assert_eq!(meta.len(), 0, "the intruding file is untouched"),
                Stage::TakeNameDir => assert!(fs::read_dir(&taken).unwrap().next().is_none()),
                _ => assert_eq!(fs::read_link(&taken).unwrap(), Path::new("/nonexistent")),
            }
            let moved = fx.trash().join("files/p.2");
            if directory {
                assert_eq!(fs::read_to_string(moved.join("inner")).unwrap(), "inner");
            } else {
                assert_eq!(fs::read_to_string(moved).unwrap(), "payload");
            }
            assert_eq!(info_names(&fx), ["p.2.trashinfo"], "{stage:?} {directory}");
        }
    }

    #[test]
    fn a_placeholder_filled_meanwhile_loses_nothing() {
        let fx = Fx::new();
        fs::create_dir(fx.work.join("d")).unwrap();
        fs::write(fx.work.join("d/x"), "x").unwrap();
        refuse_native(1);
        fault::fail(Stage::FillPlaceholder, 0);
        let Err(TrashError::NoTrash { attempts, .. }) =
            with(&fx, Fallbacks::LinkAndPlaceholder).trash(fx.work.join("d"))
        else {
            panic!()
        };
        assert_eq!(
            fs::read_to_string(fx.work.join("d/x")).unwrap(),
            "x",
            "the source is intact"
        );
        assert!(matches!(&attempts[0].failure,
            AttemptFailure::Move(err) if err.kind() == std::io::ErrorKind::AlreadyExists));
        let leftover = &attempts[0].leftovers[0];
        assert_eq!(leftover.kind, LeftoverKind::Placeholder);
        assert_eq!(leftover.path, fx.trash().join("files/d"));
        assert!(
            fx.trash().join("files/d/intruder").exists(),
            "what was put there stays"
        );
        assert_eq!(
            info_names(&fx),
            Vec::<OsString>::new(),
            "the reservation went"
        );
    }

    #[test]
    fn a_replaced_placeholder_is_left_to_its_owner() {
        let fx = Fx::new();
        fs::create_dir(fx.work.join("d")).unwrap();
        fs::write(fx.work.join("d/x"), "x").unwrap();
        refuse_native(2);
        fault::fail(Stage::ReplacePlaceholder, 0);
        let trashed = with(&fx, Fallbacks::LinkAndPlaceholder)
            .trash(fx.work.join("d"))
            .unwrap();
        assert_eq!(trashed.name, "d.2");
        assert!(
            fs::read_dir(fx.trash().join("files/d"))
                .unwrap()
                .next()
                .is_none()
        );
        assert_eq!(
            fs::read_to_string(fx.trash().join("files/d.2/x")).unwrap(),
            "x"
        );
    }

    #[test]
    fn placeholder_cleanup_and_its_failure() {
        for cleanup_fails in [false, true] {
            let fx = Fx::new();
            fs::create_dir(fx.work.join("d")).unwrap();
            refuse_native(1);
            fault::fail(Stage::PlaceholderRename, 0);
            if cleanup_fails {
                fault::fail(Stage::RemovePlaceholder, 0);
            }
            let Err(TrashError::NoTrash { attempts, .. }) =
                with(&fx, Fallbacks::LinkAndPlaceholder).trash(fx.work.join("d"))
            else {
                panic!()
            };
            assert!(fx.work.join("d").is_dir());
            assert!(matches!(attempts[0].failure, AttemptFailure::Move(_)));
            assert_eq!(info_names(&fx), Vec::<OsString>::new());
            if cleanup_fails {
                assert_eq!(attempts[0].leftovers.len(), 1);
                assert!(fx.trash().join("files/d").is_dir(), "reported, left");
            } else {
                assert!(attempts[0].leftovers.is_empty());
                assert_eq!(file_names(&fx), Vec::<OsString>::new());
            }
        }
    }

    #[test]
    fn a_refused_link_is_unsupported() {
        let fx = Fx::new();
        let source = fx.file("f", "f");
        refuse_native(1);
        fault::fail(Stage::LinkRefused, 0);
        let Err(TrashError::Unsupported { unsupported, .. }) = fx.context.trash(&source) else {
            panic!()
        };
        assert_eq!(
            unsupported.cause.unwrap().raw_os_error(),
            Some(rustix::io::Errno::PERM.raw_os_error())
        );
        assert!(source.exists());
        assert_eq!(info_names(&fx), Vec::<OsString>::new());
    }

    #[test]
    fn a_failed_unlink_is_rolled_back() {
        let fx = Fx::new();
        let source = fx.file("f", "data");
        refuse_native(1);
        fault::fail(Stage::UnlinkSource, 0);
        let Err(TrashError::NoTrash { attempts, .. }) = fx.context.trash(&source) else {
            panic!()
        };
        assert!(matches!(attempts[0].failure, AttemptFailure::Move(_)));
        assert_eq!(fs::read_to_string(&source).unwrap(), "data");
        assert_eq!(fs::metadata(&source).unwrap().nlink(), 1, "the link went");
        assert_eq!(file_names(&fx), Vec::<OsString>::new());
        assert_eq!(info_names(&fx), Vec::<OsString>::new());
    }

    #[test]
    fn a_failed_rollback_is_partial_and_final() {
        // Two candidates: after a partial move, the second is not tried.
        let fx = Fx::new();
        let top = fx.root.join("vol");
        fs::create_dir(&top).unwrap();
        fs::create_dir(top.join(".Trash")).unwrap();
        fs::set_permissions(top.join(".Trash"), fs::Permissions::from_mode(0o1777)).unwrap();
        fs::write(top.join("f"), "data").unwrap();
        fault::pretend_volume(&top);
        refuse_native(1);
        fault::fail(Stage::UnlinkSource, 0);
        fault::fail(Stage::Rollback, 0);
        let Err(TrashError::Partial(partial)) = fx.context.trash(top.join("f")) else {
            panic!()
        };
        let (trash, name, state) = (partial.trash, partial.name, partial.state);
        assert_eq!(state, PartialState::Duplicated);
        assert_eq!(trash.kind(), TrashKind::Shared);
        let linked = trash.path().join("files").join(&name);
        assert_eq!(fs::read_to_string(top.join("f")).unwrap(), "data");
        assert_eq!(
            fs::metadata(&linked).unwrap().ino(),
            fs::metadata(top.join("f")).unwrap().ino(),
            "one file, two names"
        );
        assert!(
            trash.path().join("info/f.trashinfo").exists(),
            "the entry is complete"
        );
        assert!(
            !top.join(format!(".Trash-{}", uid())).exists(),
            "no second copy"
        );
    }

    #[test]
    fn a_name_replaced_during_rollback_is_kept() {
        let fx = Fx::new();
        let source = fx.file("f", "data");
        refuse_native(1);
        fault::fail(Stage::UnlinkSource, 0);
        fault::fail(Stage::ReplaceDuringRollback, 0);
        let Err(TrashError::Partial(partial)) = fx.context.trash(&source) else {
            panic!()
        };
        let (rollback, state) = (partial.rollback, partial.state);
        assert_eq!(
            state,
            PartialState::Unverified,
            "the destination is not the payload"
        );
        assert_eq!(rollback.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read_to_string(&source).unwrap(), "data");
        assert_eq!(
            fs::metadata(fx.trash().join("files/f")).unwrap().len(),
            0,
            "the file put there meanwhile is not removed"
        );
        assert_eq!(
            info_names(&fx),
            Vec::<OsString>::new(),
            "no trash info file for what is not the payload"
        );
    }

    #[test]
    fn a_link_replaced_right_after_it_was_made_is_kept() {
        let fx = Fx::new();
        let source = fx.file("f", "data");
        refuse_native(1);
        fault::fail(Stage::ReplaceLinked, 0);
        let Err(TrashError::Partial(partial)) = fx.context.trash(&source) else {
            panic!()
        };
        let state = partial.state;
        assert_eq!(state, PartialState::Unverified);
        assert_eq!(
            fs::read_to_string(&source).unwrap(),
            "data",
            "the source is untouched"
        );
        assert_eq!(
            fs::metadata(fx.trash().join("files/f")).unwrap().len(),
            0,
            "the replacement is not removed"
        );
        assert_eq!(info_names(&fx), Vec::<OsString>::new());
    }

    #[test]
    fn a_source_replaced_after_the_link_leaves_the_payload_trashed() {
        // The source name now holds another file, which stays; the payload,
        // whose only name is the link, stays trashed rather than be lost to
        // a rollback.
        let fx = Fx::new();
        let source = fx.file("f", "data");
        refuse_native(1);
        fault::fail(Stage::ReplaceSource, 0);
        let trashed = fx.context.trash(&source).unwrap();
        assert_eq!(trashed.method, MoveMethod::Link);
        assert_eq!(
            fs::read_to_string(fx.trash().join("files/f")).unwrap(),
            "data"
        );
        assert_eq!(
            fs::metadata(&source).unwrap().len(),
            0,
            "the newcomer stays"
        );
        assert_eq!(info_names(&fx), ["f.trashinfo"]);
    }

    #[test]
    fn the_link_is_synced_before_the_source_goes() {
        let fx = Fx::new();
        let source = fx.file("f", "data");
        refuse_native(1);
        fault::fail(Stage::SyncLink, 0);
        let Err(TrashError::NoTrash { attempts, .. }) = fx.context.trash(&source) else {
            panic!()
        };
        assert!(matches!(attempts[0].failure, AttemptFailure::Move(_)));
        assert_eq!(fs::read_to_string(&source).unwrap(), "data");
        assert_eq!(
            fs::metadata(&source).unwrap().nlink(),
            1,
            "the link was undone"
        );
        assert_eq!(file_names(&fx), Vec::<OsString>::new());
    }

    #[test]
    fn an_uninspectable_source_is_no_completed_move() {
        // Both names still exist; the source's directory cannot be searched:
        // neither removed nor taken as gone, and the trash entry kept.
        unprivileged(|| {
            let fx = Fx::new();
            let source = fx.file("f", "data");
            refuse_native(1);
            fault::fail(Stage::RevokeSourceSearch, 0);
            let result = fx.context.trash(&source);
            fs::set_permissions(&fx.work, fs::Permissions::from_mode(0o755)).unwrap();
            let Err(TrashError::Partial(partial)) = result else {
                panic!("{result:?}")
            };
            assert_eq!(partial.state, PartialState::Unverified);
            assert!(partial.metadata_kept);
            assert_eq!(fs::read_to_string(&source).unwrap(), "data");
            assert_eq!(
                fs::read_to_string(fx.trash().join("files/f")).unwrap(),
                "data"
            );
            assert_eq!(info_names(&fx), ["f.trashinfo"]);
        });
    }

    #[test]
    fn an_uninspectable_link_keeps_its_metadata() {
        unprivileged(|| {
            let fx = Fx::new();
            let source = fx.file("f", "data");
            refuse_native(1);
            fault::fail(Stage::RevokeDestinationSearch, 0);
            let result = fx.context.trash(&source);
            fs::set_permissions(fx.trash().join("files"), fs::Permissions::from_mode(0o700))
                .unwrap();
            let Err(TrashError::Partial(partial)) = result else {
                panic!("{result:?}")
            };
            assert_eq!(partial.state, PartialState::Unverified);
            assert!(partial.metadata_kept, "the name may hold the payload");
            assert_eq!(info_names(&fx), ["f.trashinfo"]);
            assert_eq!(fs::read_to_string(&source).unwrap(), "data");
            assert_eq!(
                fs::read_to_string(fx.trash().join("files/f")).unwrap(),
                "data"
            );
        });
    }

    #[test]
    fn a_target_link_in_place_of_the_symlink_is_kept() {
        // Another process's hard link to the symbolic link's target, where
        // the link was made: not known to be ours, not removed.
        let fx = Fx::new();
        let target = fx.file("target", "keep");
        symlink(&target, fx.work.join("link")).unwrap();
        refuse_native(1);
        fault::fail(Stage::ReplaceLinkedWithTarget, 0);
        let Err(TrashError::Partial(partial)) = fx.context.trash(fx.work.join("link")) else {
            panic!()
        };
        assert_eq!(partial.state, PartialState::Unverified);
        assert_eq!(
            fs::metadata(&target).unwrap().nlink(),
            2,
            "the other link stays"
        );
        assert_eq!(
            fs::metadata(fx.trash().join("files/link")).unwrap().ino(),
            fs::metadata(&target).unwrap().ino()
        );
        assert_eq!(fs::read_link(fx.work.join("link")).unwrap(), target);
        assert!(
            !partial.metadata_kept,
            "the name is confirmed not the payload"
        );
        assert_eq!(info_names(&fx), Vec::<OsString>::new());
    }

    #[test]
    fn restore_never_takes_an_uninspectable_trash_for_done() {
        unprivileged(|| {
            let fx = Fx::new();
            let _ = fx.context.trash(fx.file("f", "data")).unwrap();
            let trash = fx.open_home();
            let entry = entries(&trash).remove(0);
            refuse_native(1);
            fault::fail(Stage::RevokeSourceSearch, 0);
            let result = trash.restore(&entry, &RestoreOptions::default());
            fs::set_permissions(fx.trash().join("files"), fs::Permissions::from_mode(0o700))
                .unwrap();
            let err = result.unwrap_err();
            assert!(
                matches!(
                    err.failure,
                    RestoreFailure::Partial {
                        state: PartialState::Unverified,
                        ..
                    }
                ),
                "{err}"
            );
            assert!(
                fx.trash().join("info/f.trashinfo").exists(),
                "the metadata stays"
            );
            assert!(fx.trash().join("files/f").exists());
            assert!(fx.work.join("f").exists());
        });
    }

    #[test]
    fn an_uninspectable_placeholder_is_reported() {
        unprivileged(|| {
            let fx = Fx::new();
            fs::create_dir(fx.work.join("d")).unwrap();
            fs::write(fx.work.join("d/x"), "x").unwrap();
            refuse_native(1);
            fault::fail(Stage::RevokeDestinationSearch, 0);
            let result = with(&fx, Fallbacks::LinkAndPlaceholder).trash(fx.work.join("d"));
            fs::set_permissions(fx.trash().join("files"), fs::Permissions::from_mode(0o700))
                .unwrap();
            let Err(TrashError::NoTrash { attempts, .. }) = result else {
                panic!("{result:?}")
            };
            let placeholder = attempts[0]
                .leftovers
                .iter()
                .find(|leftover| leftover.kind == LeftoverKind::Placeholder)
                .expect("the placeholder left is reported");
            assert_eq!(placeholder.path, fx.trash().join("files/d"));
            assert!(
                fs::read_dir(&placeholder.path).unwrap().next().is_none(),
                "still empty"
            );
            assert_eq!(fs::read_to_string(fx.work.join("d/x")).unwrap(), "x");
        });
    }

    #[test]
    fn other_failures_are_not_fallback_attempts() {
        unprivileged(|| {
            let fx = Fx::new();
            let source = fx.file("ro/f", "data");
            fs::set_permissions(fx.work.join("ro"), fs::Permissions::from_mode(0o555)).unwrap();
            let result = fx.context.trash(&source);
            fs::set_permissions(fx.work.join("ro"), fs::Permissions::from_mode(0o755)).unwrap();
            let Err(TrashError::NoTrash { attempts, .. }) = result else {
                panic!("{result:?}")
            };
            assert!(
                matches!(&attempts[0].failure, AttemptFailure::Move(err) if err.kind() == std::io::ErrorKind::PermissionDenied)
            );
            assert_eq!(file_names(&fx), Vec::<OsString>::new(), "no link was tried");
            assert_eq!(fs::metadata(&source).unwrap().nlink(), 1);
        });
    }

    #[test]
    fn durability_after_a_link_is_reported() {
        let fx = Fx::new();
        refuse_native(1);
        fault::fail(Stage::SyncAfterMove, 0);
        let trashed = fx.context.trash(fx.file("f", "f")).unwrap();
        assert_eq!(trashed.method, MoveMethod::Link);
        assert!(matches!(trashed.durability, Durability::NotSynced(_)));
    }

    #[test]
    fn restoring_through_the_fallbacks() {
        let fx = Fx::new();
        let _ = fx.context.trash(fx.file("f", "data")).unwrap();
        let trash = fx.open_home();
        let entry = entries(&trash).remove(0);
        // A conflict is never replaced, by the link either.
        fs::write(fx.work.join("f"), "new").unwrap();
        refuse_native(1);
        let err = trash
            .restore(&entry, &RestoreOptions::default())
            .unwrap_err();
        assert!(matches!(err.failure, RestoreFailure::Conflict(_)));
        assert_eq!(fs::read_to_string(fx.work.join("f")).unwrap(), "new");
        fs::remove_file(fx.work.join("f")).unwrap();
        // Partial: both names, the entry kept.
        refuse_native(1);
        fault::fail(Stage::UnlinkSource, 0);
        fault::fail(Stage::Rollback, 0);
        let err = trash
            .restore(&entry, &RestoreOptions::default())
            .unwrap_err();
        assert!(
            matches!(
                err.failure,
                RestoreFailure::Partial {
                    state: PartialState::Duplicated,
                    ..
                }
            ),
            "{err}"
        );
        assert!(fx.work.join("f").exists() && fx.trash().join("files/f").exists());
        assert!(fx.trash().join("info/f.trashinfo").exists());
        fs::remove_file(fx.work.join("f")).unwrap();
        // Through.
        refuse_native(1);
        let restored = trash.restore(&entry, &RestoreOptions::default()).unwrap();
        assert_eq!(restored.method, MoveMethod::Link);
        assert_eq!(fs::read_to_string(fx.work.join("f")).unwrap(), "data");
        assert_eq!(file_names(&fx), Vec::<OsString>::new());
        assert_eq!(info_names(&fx), Vec::<OsString>::new());
    }

    #[test]
    fn restoring_a_directory_needs_the_opt_in_too() {
        let fx = Fx::new();
        fs::create_dir(fx.work.join("d")).unwrap();
        fs::write(fx.work.join("d/x"), "x").unwrap();
        let _ = fx.context.trash(fx.work.join("d")).unwrap();
        let trash = fx.open_home();
        let entry = entries(&trash).remove(0);
        refuse_native(1);
        let err = trash
            .restore(&entry, &RestoreOptions::default())
            .unwrap_err();
        assert!(matches!(err.failure, RestoreFailure::Unsupported(_)));
        let context = with(&fx, Fallbacks::LinkAndPlaceholder);
        let trash = context.open(&context.home_trash()).unwrap();
        let entry = entries(&trash).remove(0);
        refuse_native(1);
        let restored = trash.restore(&entry, &RestoreOptions::default()).unwrap();
        assert_eq!(restored.method, MoveMethod::Placeholder);
        assert_eq!(fs::read_to_string(fx.work.join("d/x")).unwrap(), "x");
    }
}

mod exclusions {
    use super::*;

    fn check(exclusions: &Exclusions, path: &str) -> Option<PathBuf> {
        match exclusions.check(Path::new(path)) {
            Ok(()) => None,
            Err(Excluded {
                reason: ExclusionReason::Rule { rule, .. },
                ..
            }) => Some(rule.path),
            Err(other) => panic!("{other:?}"),
        }
    }

    #[test]
    fn rules_match_whole_components() {
        let rules = Exclusions::new()
            .anchored(Path::new("/"))
            .mount("/mnt/a", "a")
            .anchored(Path::new("/"));
        assert_eq!(check(&rules, "/mnt/a"), Some("/mnt/a".into()));
        assert_eq!(check(&rules, "/mnt/a/b/c"), Some("/mnt/a".into()));
        assert_eq!(check(&rules, "/mnt/ab"), None);
        assert_eq!(check(&rules, "/mnt"), None);
        // Repeated separators and `.` mean nothing; `..` is followed as
        // lookup follows it, and what it passes through counts.
        assert_eq!(check(&rules, "//mnt///a/./x"), Some("/mnt/a".into()));
        assert_eq!(check(&rules, "/mnt/a/../b"), Some("/mnt/a".into()));
        assert_eq!(check(&rules, "/x/../mnt/a/y"), Some("/mnt/a".into()));
        assert_eq!(check(&rules, "/mnt/b/../ab"), None);
        let strict = rules.clone().refuse_parent_components(true);
        assert!(matches!(
            strict.check(Path::new("/mnt/b/../c")),
            Err(Excluded {
                reason: ExclusionReason::ParentComponent,
                ..
            })
        ));
        assert_eq!(check(&strict, "/mnt/b/c"), None);
    }

    #[test]
    fn nested_mounts_and_overlapping_rules() {
        let points = ["/", "/mnt/nfs", "/mnt/nfs/local", "/mnt/nfs/deep"].map(PathBuf::from);
        let rules = Exclusions::new()
            .mount("/mnt/nfs", "nfs")
            .mount("/mnt/nfs/deep", "deep")
            .with_mount_points(points)
            .anchored(Path::new("/"));
        assert_eq!(check(&rules, "/mnt/nfs/x"), Some("/mnt/nfs".into()));
        // A volume mounted inside the excluded one is its own, and reaching
        // it only passes through the excluded one's top directories.
        assert_eq!(check(&rules, "/mnt/nfs/local/x"), None);
        assert_eq!(check(&rules, "/mnt/nfs/local"), None);
        // Coming back into the excluded volume by `..` counts, even on the
        // way to the nested one again.
        assert_eq!(
            check(&rules, "/mnt/nfs/local/../local/x"),
            Some("/mnt/nfs".into())
        );
        assert_eq!(
            check(&rules, "/mnt/../mnt/nfs/local/x"),
            None,
            "`..` above it only"
        );
        // Leaving the nested volume by `..` lands in the excluded one.
        assert_eq!(
            check(&rules, "/mnt/nfs/local/../x"),
            Some("/mnt/nfs".into())
        );
        assert_eq!(
            check(&rules, "/mnt/nfs/local/../../y"),
            Some("/mnt/nfs".into())
        );
        // The deepest covering rule is reported.
        assert_eq!(
            check(&rules, "/mnt/nfs/deep/z"),
            Some("/mnt/nfs/deep".into())
        );
        // A subtree rule covers nested volumes too.
        let subtree = Exclusions::new()
            .subtree("/mnt/nfs", "all")
            .with_mount_points(["/mnt/nfs/local".into()])
            .anchored(Path::new("/"));
        assert_eq!(check(&subtree, "/mnt/nfs/local/x"), Some("/mnt/nfs".into()));
        // Without known mount points, a mount rule reaches everywhere below.
        let unbounded = Exclusions::new()
            .mount("/mnt/nfs", "nfs")
            .anchored(Path::new("/"));
        assert_eq!(
            check(&unbounded, "/mnt/nfs/local/x"),
            Some("/mnt/nfs".into())
        );
    }

    #[test]
    fn exclusions_from_a_mount_list() {
        let mount = |id, point: &str, fs_type: &str| Mount {
            id,
            parent: 1,
            mount_point: point.into(),
            fs_type: fs_type.into(),
            source: "src".into(),
        };
        let mounts = [
            mount(1, "/", "ext4"),
            mount(2, "/net", "nfs4"),
            mount(3, "/net/cache", "tmpfs"),
        ];
        let rules = Exclusions::from_mounts(&mounts, |mount| mount.fs_type.starts_with("nfs"));
        assert_eq!(rules.rules().len(), 1);
        assert_eq!(rules.rules()[0].label, "nfs4");
        assert_eq!(check(&rules, "/net/x"), Some("/net".into()));
        assert_eq!(check(&rules, "/net/cache/x"), None);
        assert_eq!(check(&rules, "/home/x"), None);
    }

    fn excluding(fx: &Fx, cwd: &Path, exclusions: Exclusions) -> Context {
        Context::new(&fx.home, uid(), cwd, LocalZone::utc())
            .unwrap()
            .with_policy(Policy {
                exclusions,
                ..Policy::default()
            })
    }

    #[test]
    fn an_excluded_source_is_never_touched() {
        let fx = Fx::new();
        let net = fx.root.join("net");
        fs::create_dir(&net).unwrap();
        fs::write(net.join("f"), "f").unwrap();
        let context = excluding(&fx, &fx.work, Exclusions::new().subtree(&net, "nfs"));
        // Existing, missing, relative, through `..`; and a relative path
        // from an excluded working directory.
        let inside = excluding(&fx, &net, Exclusions::new().subtree(&net, "nfs"));
        let cases: [(&Context, PathBuf); 5] = [
            (&context, net.join("f")),
            (&context, net.join("missing/deeper")),
            (&context, PathBuf::from("../net/f")),
            (&context, fx.work.join("../net/./f")),
            (&inside, PathBuf::from("f")),
        ];
        for (context, path) in cases {
            let _ = sys::take_probes();
            let trashed = context.trash(&path);
            let planned = context.plan(&path);
            let into = context.trash_into(&context.home_trash(), &path);
            assert_eq!(
                sys::take_probes(),
                Vec::new(),
                "{}: nothing was touched",
                path.display()
            );
            for result in [trashed.err(), planned.err(), into.err()] {
                assert!(
                    matches!(result, Some(TrashError::Excluded(_))),
                    "{}: {result:?}",
                    path.display()
                );
            }
        }
        assert!(net.join("f").exists());
        assert!(!fx.home.exists());
    }

    #[test]
    fn an_alias_is_caught_once_resolved() {
        // A symbolic link into an excluded place: the path does not say
        // so; resolving it does (it was touched to resolve it).
        let fx = Fx::new();
        let net = fx.root.join("net");
        fs::create_dir(&net).unwrap();
        fs::write(net.join("f"), "f").unwrap();
        symlink(&net, fx.work.join("alias")).unwrap();
        let context = excluding(&fx, &fx.work, Exclusions::new().subtree(&net, "nfs"));
        let _ = sys::take_probes();
        assert!(matches!(
            context.trash(fx.work.join("alias/f")),
            Err(TrashError::Excluded(_))
        ));
        assert!(
            !sys::take_probes().is_empty(),
            "documented: aliases are reached"
        );
        assert!(net.join("f").exists());
    }

    /// Paths a probe log names below `place`.
    fn probed_below(place: &Path) -> Vec<PathBuf> {
        sys::take_probes()
            .into_iter()
            .flatten()
            .filter(|path| path.starts_with(place))
            .collect()
    }

    #[test]
    fn excluded_trash_directories_are_not_probed() {
        // The data home excluded: the home trash is passed over unlooked at,
        // and the volume's trash takes the file.
        let fx = Fx::new();
        let top = fx.root.join("vol");
        fs::create_dir(&top).unwrap();
        fs::write(top.join("doc"), "d").unwrap();
        let context = excluding(&fx, &fx.work, Exclusions::new().subtree(&fx.home, "home"));
        fault::pretend_volume(&top);
        let _ = sys::take_probes();
        let plan = context.plan(top.join("doc")).unwrap();
        assert!(matches!(
            plan.skipped[0].failure,
            AttemptFailure::Excluded(_)
        ));
        let trashed = context.trash(top.join("doc")).unwrap();
        assert_eq!(trashed.trash.kind(), TrashKind::Private);
        assert_eq!(probed_below(&fx.home), Vec::<PathBuf>::new());
        assert!(!fx.home.exists());
        // A volume trash excluded: passed over, not created.
        let fx = Fx::new();
        let top = fx.root.join("vol");
        fs::create_dir(&top).unwrap();
        fs::write(top.join("doc"), "d").unwrap();
        let private = top.join(format!(".Trash-{}", uid()));
        let context = excluding(
            &fx,
            &fx.work,
            Exclusions::new().subtree(&private, "private"),
        );
        fault::pretend_volume(&top);
        let _ = sys::take_probes();
        let Err(TrashError::NoTrash { attempts, .. }) = context.trash(top.join("doc")) else {
            panic!()
        };
        assert!(
            attempts
                .iter()
                .any(|attempt| matches!(attempt.failure, AttemptFailure::Excluded(_)))
        );
        assert_eq!(probed_below(&private), Vec::<PathBuf>::new());
        assert!(!private.exists());
        assert!(top.join("doc").exists());
        // An explicit trash directory excluded: refused, not created.
        let explicit = context.trash_into(&context.private_trash(&top), top.join("doc"));
        assert!(matches!(explicit, Err(TrashError::NoTrash { .. })));
        assert!(!private.exists());
    }

    #[test]
    fn an_excluded_files_or_info_excludes_its_trash() {
        for child in ["files", "info"] {
            let fx = Fx::new();
            let _ = fx.context.trash(fx.file("seed", "s")).unwrap();
            let top = fx.root.join("vol");
            fs::create_dir(&top).unwrap();
            fs::write(top.join("doc"), "d").unwrap();
            let context = excluding(
                &fx,
                &fx.work,
                Exclusions::new().subtree(fx.trash().join(child), child),
            );
            fault::pretend_volume(&top);
            let plan = context.plan(top.join("doc")).unwrap();
            assert!(
                matches!(plan.skipped[0].failure, AttemptFailure::Excluded(_)),
                "{child}"
            );
            let _ = sys::take_probes();
            let trashed = context.trash(top.join("doc")).unwrap();
            assert_eq!(trashed.trash.kind(), TrashKind::Private, "{child}");
            assert_eq!(probed_below(&fx.trash().join(child)), Vec::<PathBuf>::new());
            assert_eq!(names(&fx.trash().join("files")), ["seed"]);
            fault::clear();
            let _ = sys::take_probes();
            // Named explicitly: refused, not probed.
            let explicit = context.trash_into(&context.home_trash(), fx.file("g", "g"));
            assert!(matches!(explicit, Err(TrashError::NoTrash { .. })));
            assert_eq!(probed_below(&fx.trash().join(child)), Vec::<PathBuf>::new());
        }
    }

    #[test]
    fn an_excluded_trash_is_still_protected_lexically() {
        let fx = Fx::new();
        let _ = fx.context.trash(fx.file("seed", "s")).unwrap();
        let context = excluding(
            &fx,
            &fx.work,
            Exclusions::new().subtree(fx.trash().join("info"), "info"),
        );
        for (path, problem) in [
            (fx.trash().join("files/seed"), SourceProblem::InTrash),
            (fx.trash().join("files"), SourceProblem::InTrash),
            (fx.home.clone(), SourceProblem::HoldsTrash),
        ] {
            let result = context.trash(&path);
            assert!(
                matches!(&result, Err(TrashError::InvalidSource { problem: found, .. }) if *found == problem),
                "{}: {result:?}",
                path.display()
            );
        }
        assert!(fx.trash().join("files/seed").exists());
        assert_eq!(names(&fx.trash().join("info")), ["seed.trashinfo"]);
    }

    #[test]
    fn a_trash_under_a_symbolic_link_is_protected_with_a_child_excluded() {
        // `$XDG_DATA_HOME` a link to the real directory; `info` excluded
        // under the link's name: the base itself is not excluded, so it is
        // resolved, and the source's real path is matched against it.
        let fx = Fx::new();
        let real = fx.root.join("realdata");
        fs::create_dir(&real).unwrap();
        symlink(&real, fx.root.join("alias")).unwrap();
        let alias = Context::new(fx.root.join("alias"), uid(), &fx.work, LocalZone::utc()).unwrap();
        let _ = alias.trash(fx.file("seed", "s")).unwrap();
        let context = alias.clone().with_policy(Policy {
            exclusions: Exclusions::new().subtree(fx.root.join("alias/Trash/info"), "info"),
            ..Policy::default()
        });
        let top = fx.root.join("vol");
        fs::create_dir(&top).unwrap();
        // Should the check fail, trashing stays inside the fixture.
        fault::pretend_volume(&top);
        for result in [
            context.trash(real.join("Trash/files/seed")),
            context.trash_into(&context.private_trash(&top), real.join("Trash/files/seed")),
        ] {
            assert!(
                matches!(
                    result,
                    Err(TrashError::InvalidSource {
                        problem: SourceProblem::InTrash,
                        ..
                    })
                ),
                "{result:?}"
            );
        }
        assert!(real.join("Trash/files/seed").exists());
        assert!(real.join("Trash/info/seed.trashinfo").exists());
    }

    #[test]
    fn an_excluded_trash_root_with_nested_children_is_excluded() {
        // The trash directory a mount of its own, excluded; its `files` and
        // `info` known mount points of their own (not excluded). The trash
        // directory itself is opened and inspected, so it counts.
        let fx = Fx::new();
        let _ = fx.context.trash(fx.file("seed", "s")).unwrap();
        let trash = fx.trash();
        let context = excluding(
            &fx,
            &fx.work,
            Exclusions::new().mount(&trash, "trash").with_mount_points([
                trash.clone(),
                trash.join("files"),
                trash.join("info"),
            ]),
        );
        let top = fx.root.join("vol");
        fs::create_dir(&top).unwrap();
        fs::write(top.join("doc"), "d").unwrap();
        fault::pretend_volume(&top);
        let plan = context.plan(top.join("doc")).unwrap();
        assert!(matches!(
            plan.skipped[0].failure,
            AttemptFailure::Excluded(_)
        ));
        let _ = sys::take_probes();
        let trashed = context.trash(top.join("doc")).unwrap();
        assert_eq!(trashed.trash.kind(), TrashKind::Private);
        assert_eq!(probed_below(&trash), Vec::<PathBuf>::new());
        fault::clear();
        let _ = sys::take_probes();
        let explicit = context.trash_into(&context.home_trash(), fx.file("g", "g"));
        assert!(matches!(explicit, Err(TrashError::NoTrash { .. })));
        assert_eq!(probed_below(&trash), Vec::<PathBuf>::new());
        assert_eq!(names(&trash.join("files")), ["seed"]);
    }

    #[test]
    fn other_places_and_listing_are_unaffected() {
        let fx = Fx::new();
        let net = fx.root.join("net");
        fs::create_dir(&net).unwrap();
        let context = excluding(&fx, &fx.work, Exclusions::new().mount(&net, "nfs"));
        let trashed = context.trash(fx.file("f", "f")).unwrap();
        assert_eq!(trashed.trash.kind(), TrashKind::Home);
        // A per-volume trash in the excluded place is still listed.
        fs::write(net.join("g"), "g").unwrap();
        let _ = fx
            .context
            .trash_into(&fx.context.private_trash(&net), net.join("g"))
            .unwrap();
        let discovery = context.discover([&net]);
        assert_eq!(discovery.trashes.len(), 2);
    }
}

mod home_link {
    use super::*;

    /// A directory for the home trash to be kept in, under another name.
    fn kept(fx: &Fx, rel: &str) -> PathBuf {
        let target = fx.root.join(rel);
        fs::create_dir_all(&target).unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o700)).unwrap();
        target
    }

    /// `$XDG_DATA_HOME/Trash` made a symbolic link to `to`.
    fn link_home(fx: &Fx, to: impl AsRef<Path>) {
        fs::create_dir_all(&fx.home).unwrap();
        symlink(to, fx.trash()).unwrap();
    }

    fn with_policy(fx: &Fx, change: impl FnOnce(&mut Policy)) -> Context {
        let mut policy = Policy::default();
        change(&mut policy);
        fx.context.clone().with_policy(policy)
    }

    /// Why following the home trash's link failed: an error kind, or for
    /// a loop the kind `Other` (`ELOOP` has no stable kind of its own).
    fn broken(err: &OpenError) -> std::io::ErrorKind {
        match &err.kind {
            OpenErrorKind::BrokenLink { error, .. }
                if error.raw_os_error() == Some(rustix::io::Errno::LOOP.raw_os_error()) =>
            {
                std::io::ErrorKind::Other
            }
            OpenErrorKind::BrokenLink { error, .. } => error.kind(),
            other => panic!("{other:?}"),
        }
    }

    fn invalid(result: Result<Trashed, TrashError>) -> SourceProblem {
        match result {
            Err(TrashError::InvalidSource { problem, .. }) => problem,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_link_to_a_directory_of_another_name_is_the_home_trash() {
        let fx = Fx::new();
        let target = kept(&fx, "disk/my-trash");
        link_home(&fx, &target);
        assert!(Policy::default().follow_home_link, "on by default");

        let planned = fx.context.plan(fx.file("a/b", "b")).unwrap();
        let destination = planned.destination().unwrap();
        assert_eq!(destination.trash.kind(), TrashKind::Home);
        // `files` and `info` are missing in the target, and may be made.
        assert!(
            matches!(&destination.state, CandidateState::WouldCreate(first) if *first == fx.trash().join("files")),
            "{:?}",
            destination.state
        );
        assert!(!target.join("files").exists(), "planning created nothing");

        let trashed = fx.context.trash(fx.work.join("a/b")).unwrap();
        assert_eq!(trashed.trash, fx.context.home_trash());
        assert_eq!(trashed.trash.path(), fx.trash(), "named as the home trash");
        assert_eq!(fs::read_to_string(target.join("files/b")).unwrap(), "b");
        let info = fs::read_to_string(target.join("info/b.trashinfo")).unwrap();
        assert!(info.contains(&format!("\nPath={}\n", fx.work.join("a/b").display())));
        // Below `$XDG_DATA_HOME`, the path recorded is relative to it, not
        // to where the trash is kept.
        fs::write(fx.home.join("doc"), "d").unwrap();
        let _ = fx.context.trash(fx.home.join("doc")).unwrap();
        let info = fs::read_to_string(target.join("info/doc.trashinfo")).unwrap();
        assert!(info.contains("\nPath=doc\n"), "{info}");
        assert!(matches!(
            fx.context
                .plan(fx.file("c", "c"))
                .unwrap()
                .destination()
                .unwrap()
                .state,
            CandidateState::Usable
        ));

        let trash = fx.open_home();
        assert_eq!(trash.link_target(), Some(target.as_path()));
        let discovery = fx.context.discover(Vec::<PathBuf>::new());
        assert!(discovery.is_complete());
        assert_eq!(discovery.trashes[0].link_target(), Some(target.as_path()));
        let listed = entries(&trash);
        assert_eq!(listed.len(), 2);
        let doc = listed.iter().find(|entry| entry.name() == "doc").unwrap();
        assert_eq!(doc.origin(), &Origin::Known(fx.home.join("doc")));
        let restored = trash.restore(doc, &RestoreOptions::default()).unwrap();
        assert_eq!(restored.destination, fx.home.join("doc"));
        assert_eq!(fs::read_to_string(fx.home.join("doc")).unwrap(), "d");
        assert!(
            trash
                .size(&SizeOptions::default(), &|| false)
                .unwrap()
                .bytes
                > 0
        );
        let b = entries(&trash).remove(0);
        trash
            .erase(&b, &EraseOptions::default(), &|| false)
            .unwrap();
        assert_eq!(names(&target.join("files")), Vec::<OsString>::new());
        fs::write(target.join("info/pending.trashinfo"), "[Trash Info]\n").unwrap();
        let pending = entries(&trash).remove(0);
        trash
            .discard_metadata(&pending, Quiescent::asserted())
            .unwrap();
        assert_eq!(names(&target.join("info")), Vec::<OsString>::new());
        assert!(fs::symlink_metadata(fx.trash()).unwrap().is_symlink());
    }

    #[test]
    fn a_chain_of_links_relative_and_absolute() {
        let fx = Fx::new();
        let target = kept(&fx, "real/kept");
        // `Trash` -> `../hop` (relative to `$XDG_DATA_HOME`) -> the target
        // through another link on the way.
        symlink(fx.root.join("real"), fx.root.join("via")).unwrap();
        symlink("via/kept", fx.root.join("hop")).unwrap();
        link_home(&fx, "../hop");
        let trashed = fx.context.trash(fx.file("f", "f")).unwrap();
        assert_eq!(trashed.trash.kind(), TrashKind::Home);
        assert_eq!(fs::read_to_string(target.join("files/f")).unwrap(), "f");
        assert_eq!(fx.open_home().link_target(), Some(target.as_path()));
    }

    #[test]
    fn links_that_lead_to_no_directory_create_nothing() {
        let fx = Fx::new();
        let missing = fx.root.join("disk/missing");
        fs::create_dir(fx.root.join("disk")).unwrap();
        fs::write(fx.root.join("disk/file"), "").unwrap();
        fs::create_dir(&fx.home).unwrap();
        let cases: [(PathBuf, std::io::ErrorKind); 3] = [
            (missing.clone(), std::io::ErrorKind::NotFound),
            (fx.root.join("disk/file"), std::io::ErrorKind::NotADirectory),
            // A loop: the link names itself.
            (PathBuf::from("Trash"), std::io::ErrorKind::Other),
        ];
        for (to, kind) in cases {
            let _ = fs::remove_file(fx.trash());
            symlink(&to, fx.trash()).unwrap();
            let err = fx.context.open(&fx.context.home_trash()).unwrap_err();
            assert_eq!(err.path, fx.trash(), "{to:?}");
            assert_eq!(broken(&err), kind, "{to:?}");
            // Listed as a problem, not as an absent trash.
            let discovery = fx.context.discover(Vec::<PathBuf>::new());
            assert!(discovery.trashes.is_empty());
            assert_eq!(broken(&discovery.problems[0]), kind);
            // Planning and trashing: unusable, and nothing made.
            let source = fx.file("f", "f");
            let planned = fx.context.plan(&source).unwrap();
            assert!(planned.destination().is_none(), "{to:?}");
            match &planned.candidates[0].state {
                CandidateState::Unusable(AttemptFailure::Open(err)) => {
                    assert_eq!(broken(err), kind);
                }
                other => panic!("{other:?}"),
            }
            match fx.context.trash(&source) {
                Err(TrashError::NoTrash { attempts, .. }) => {
                    assert_eq!(attempts.len(), 1, "the home trash only: {attempts:?}");
                    assert!(
                        matches!(&attempts[0].failure, AttemptFailure::Open(err) if broken(err) == kind)
                    );
                }
                other => panic!("{other:?}"),
            }
            assert!(source.exists());
            assert!(!missing.exists(), "nothing was created through the link");
            assert_eq!(names(&fx.root.join("disk")), ["file"]);
        }
    }

    #[test]
    fn the_target_must_pass_the_home_trash_checks() {
        let fx = Fx::new();
        let target = kept(&fx, "disk/kept");
        link_home(&fx, &target);
        let other =
            Context::new(&fx.home, uid().wrapping_add(1), &fx.work, LocalZone::utc()).unwrap();
        let err = other.open(&other.home_trash()).unwrap_err();
        assert_eq!(err.path, fx.trash());
        assert!(matches!(
            err.kind,
            OpenErrorKind::Unsafe(CheckFailure::Owner { found, .. }) if found == uid()
        ));
        fs::set_permissions(&target, fs::Permissions::from_mode(0o777)).unwrap();
        let err = fx.context.open(&fx.context.home_trash()).unwrap_err();
        assert!(matches!(
            err.kind,
            OpenErrorKind::Unsafe(CheckFailure::Writable(0o777))
        ));
        let source = fx.file("f", "f");
        assert!(matches!(
            fx.context.trash(&source),
            Err(TrashError::NoTrash { .. })
        ));
        assert!(!target.join("files").exists());
        // `files` and `info` in the target are checked as always.
        fs::set_permissions(&target, fs::Permissions::from_mode(0o700)).unwrap();
        let _ = fx.context.trash(&source).unwrap();
        fs::set_permissions(target.join("files"), fs::Permissions::from_mode(0o770)).unwrap();
        let err = fx.context.open(&fx.context.home_trash()).unwrap_err();
        assert_eq!(err.path, fx.trash().join("files"));
        assert!(matches!(
            err.kind,
            OpenErrorKind::Unsafe(CheckFailure::Writable(0o770))
        ));
        fs::set_permissions(target.join("files"), fs::Permissions::from_mode(0o700)).unwrap();
        fs::rename(target.join("info"), fx.root.join("info")).unwrap();
        symlink(fx.root.join("info"), target.join("info")).unwrap();
        assert!(matches!(
            fx.context.open(&fx.context.home_trash()).unwrap_err().kind,
            OpenErrorKind::Unsafe(CheckFailure::Symlink)
        ));
    }

    #[test]
    fn with_the_policy_off_a_link_fails_its_check() {
        let fx = Fx::new();
        let target = kept(&fx, "disk/kept");
        link_home(&fx, &target);
        let context = with_policy(&fx, |policy| policy.follow_home_link = false);
        assert!(matches!(
            context.open(&context.home_trash()).unwrap_err().kind,
            OpenErrorKind::Unsafe(CheckFailure::Symlink)
        ));
        let source = fx.file("f", "f");
        assert!(matches!(
            context.trash(&source),
            Err(TrashError::NoTrash { .. })
        ));
        assert!(matches!(
            context.plan(&source).unwrap().candidates[0].state,
            CandidateState::Unusable(AttemptFailure::Open(OpenError {
                kind: OpenErrorKind::Unsafe(CheckFailure::Symlink),
                ..
            }))
        ));
        assert!(!target.join("files").exists());
        // The link is an ordinary link then, and can be trashed as one.
        let elsewhere = context.private_trash(&fx.root);
        let trashed = context.trash_into(&elsewhere, fx.trash()).unwrap();
        assert!(
            fs::symlink_metadata(elsewhere.path().join("files").join(&trashed.name))
                .unwrap()
                .is_symlink()
        );
        assert!(target.exists());
    }

    #[test]
    fn the_link_its_target_and_what_holds_them_are_not_trashed() {
        let fx = Fx::new();
        let target = kept(&fx, "disk/kept");
        link_home(&fx, &target);
        let _ = fx.context.trash(fx.file("f", "f")).unwrap();
        symlink(&target, fx.work.join("alias")).unwrap();
        let cases: [(PathBuf, SourceProblem); 6] = [
            (fx.trash(), SourceProblem::HomeTrashLink),
            (
                fx.root.join("data/../data/Trash/"),
                SourceProblem::HomeTrashLink,
            ),
            (target.clone(), SourceProblem::HoldsTrash),
            (fx.root.join("disk"), SourceProblem::HoldsTrash),
            (target.join("files/f"), SourceProblem::InTrash),
            (fx.work.join("alias/files"), SourceProblem::InTrash),
        ];
        for (path, problem) in &cases {
            assert_eq!(invalid(fx.context.trash(path)), *problem, "{path:?}");
            assert_eq!(
                invalid(fx.context.trash_into(&fx.context.home_trash(), path)),
                *problem,
                "{path:?}"
            );
            assert!(
                matches!(fx.context.plan(path), Err(TrashError::InvalidSource { problem: found, .. }) if found == *problem),
                "{path:?}"
            );
        }
        // Into another trash too.
        let elsewhere = fx.context.private_trash(&fx.root);
        assert_eq!(
            invalid(fx.context.trash_into(&elsewhere, &target)),
            SourceProblem::HoldsTrash
        );
        // The link is refused even when it leads nowhere.
        fs::remove_file(fx.trash()).unwrap();
        symlink(fx.root.join("missing"), fx.trash()).unwrap();
        assert_eq!(
            invalid(fx.context.trash(fx.trash())),
            SourceProblem::HomeTrashLink
        );
        assert!(fs::symlink_metadata(fx.trash()).is_ok());
    }

    #[test]
    fn exclusions_reach_the_target_and_links_on_the_way() {
        let fx = Fx::new();
        let net = fx.root.join("net");
        fs::create_dir(&net).unwrap();
        symlink(net.join("hop-target"), fx.root.join("hop")).unwrap();
        let top = fx.root.join("vol");
        fs::create_dir(&top).unwrap();
        let context = with_policy(&fx, |policy| {
            policy.exclusions = Exclusions::new().subtree(&net, "nfs");
        });
        // Each leads to a missing directory in the excluded place: an
        // exclusion, not a broken link, shows it was refused unlooked at.
        let cases = [
            net.join("kept"),
            PathBuf::from("../net/kept"),
            fx.root.join("hop"),
        ];
        for to in cases {
            let _ = fs::remove_file(fx.trash());
            link_home(&fx, &to);
            fault::pretend_volume(&top);
            let source = top.join("doc");
            fs::write(&source, "d").unwrap();
            let planned = context.plan(&source).unwrap();
            assert!(
                matches!(&planned.skipped[0], Attempt { trash: Some(trash), failure: AttemptFailure::Excluded(_), .. } if trash.kind() == TrashKind::Home),
                "{to:?}: {planned:?}"
            );
            let trashed = context.trash(&source).unwrap();
            assert_eq!(trashed.trash.kind(), TrashKind::Private, "{to:?}");
            assert!(matches!(
                trashed.fallbacks[0].failure,
                AttemptFailure::Excluded(_)
            ));
            assert!(matches!(
                context.trash_into(&context.home_trash(), fx.file("g", "g")),
                Err(TrashError::NoTrash { attempts, .. }) if matches!(attempts[0].failure, AttemptFailure::Excluded(_))
            ));
            fault::clear();
            // Listing does not use the exclusions: it finds the link broken.
            assert_eq!(
                broken(&context.open(&context.home_trash()).unwrap_err()),
                std::io::ErrorKind::NotFound
            );
        }
        assert_eq!(names(&net), Vec::<OsString>::new());
    }

    #[test]
    fn a_link_retargeted_after_it_is_read_is_not_followed_again() {
        let fx = Fx::new();
        let target = kept(&fx, "disk/kept");
        let elsewhere = kept(&fx, "elsewhere");
        link_home(&fx, &target);
        fault::swap_to(&elsewhere);
        fault::fail(Stage::RetargetHomeLink, 0);
        let _ = fx.context.trash(fx.file("f", "f")).unwrap();
        assert_eq!(fs::read_to_string(target.join("files/f")).unwrap(), "f");
        assert_eq!(names(&elsewhere), Vec::<OsString>::new());
        // A later operation follows the link as it is now.
        let _ = fx.context.trash(fx.file("g", "g")).unwrap();
        assert_eq!(fs::read_to_string(elsewhere.join("files/g")).unwrap(), "g");
    }

    #[test]
    fn a_link_swapped_in_on_the_way_is_refused() {
        let fx = Fx::new();
        let _ = kept(&fx, "disk/kept");
        let elsewhere = kept(&fx, "elsewhere");
        link_home(&fx, "../disk/kept");
        fault::swap_to(&elsewhere);
        // `disk`, after it was looked at as a directory, becomes a link,
        // which opening without following refuses (`ELOOP`, or `ENOTDIR`
        // as Linux has it for a directory open).
        fault::fail(Stage::SwapOnTheWay, 0);
        let source = fx.file("f", "f");
        match fx.context.trash(&source) {
            Err(TrashError::NoTrash { attempts, .. }) => assert!(
                matches!(&attempts[0].failure, AttemptFailure::Open(OpenError { kind: OpenErrorKind::Io(err), .. }) if [rustix::io::Errno::LOOP, rustix::io::Errno::NOTDIR].map(|errno| Some(errno.raw_os_error())).contains(&err.raw_os_error())),
                "{attempts:?}"
            ),
            other => panic!("{other:?}"),
        }
        assert!(source.exists());
        assert_eq!(names(&elsewhere), Vec::<OsString>::new());
    }

    #[test]
    fn an_operation_and_an_open_trash_stay_on_the_directory_opened() {
        let fx = Fx::new();
        let target = kept(&fx, "disk/kept");
        let elsewhere = kept(&fx, "elsewhere");
        link_home(&fx, &target);
        // Retargeted, and the target renamed, once it is open: the
        // trashing goes on into it.
        fault::swap_to(&elsewhere);
        fault::fail(Stage::MoveHomeTarget, 0);
        let _ = fx.context.trash(fx.file("f", "f")).unwrap();
        let moved = fx.root.join("disk/kept.moved");
        assert_eq!(fs::read_to_string(moved.join("files/f")).unwrap(), "f");
        assert_eq!(names(&elsewhere), Vec::<OsString>::new());
        // An open trash, likewise.
        fs::remove_file(fx.trash()).unwrap();
        symlink(&moved, fx.trash()).unwrap();
        let trash = fx.open_home();
        fs::remove_file(fx.trash()).unwrap();
        symlink(&elsewhere, fx.trash()).unwrap();
        assert_eq!(trash.link_target(), Some(moved.as_path()));
        let listed = entries(&trash);
        assert_eq!(listed.len(), 1);
        let _ = trash
            .restore(&listed[0], &RestoreOptions::default())
            .unwrap();
        assert_eq!(fs::read_to_string(fx.work.join("f")).unwrap(), "f");
        // A new open sees the link as it is now.
        assert!(matches!(
            fx.context.open(&fx.context.home_trash()).unwrap_err(),
            OpenError { kind: OpenErrorKind::Absent, path } if path == fx.trash().join("files")
        ));
    }

    #[test]
    fn another_contexts_home_trash_is_followed_as_its_own() {
        let fx = Fx::new();
        let mine = kept(&fx, "disk/mine");
        link_home(&fx, &mine);
        let theirs = kept(&fx, "disk/theirs");
        let other_home = fx.root.join("other-data");
        fs::create_dir(&other_home).unwrap();
        symlink(&theirs, other_home.join("Trash")).unwrap();
        let other = Context::new(&other_home, uid(), &fx.work, LocalZone::utc()).unwrap();
        let trashed = fx
            .context
            .trash_into(&other.home_trash(), fx.file("f", "f"))
            .unwrap();
        assert_eq!(trashed.trash, other.home_trash());
        assert_eq!(fs::read_to_string(theirs.join("files/f")).unwrap(), "f");
        assert!(!mine.join("files").exists(), "not into this context's own");
        let opened = fx.context.open(&other.home_trash()).unwrap();
        assert_eq!(opened.link_target(), Some(theirs.as_path()));
        // Another's plain home trash is used as it is.
        let plain = Context::new(fx.root.join("plain"), uid(), &fx.work, LocalZone::utc()).unwrap();
        let _ = fx
            .context
            .trash_into(&plain.home_trash(), fx.file("g", "g"))
            .unwrap();
        assert!(fx.root.join("plain/Trash/files/g").exists());
        assert!(!mine.join("files").exists());
    }

    #[test]
    fn a_target_failing_its_checks_still_holds_what_it_holds() {
        let fx = Fx::new();
        let target = kept(&fx, "disk/kept");
        link_home(&fx, &target);
        let _ = fx.context.trash(fx.file("f", "f")).unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o777)).unwrap();
        let elsewhere = fx.context.private_trash(&fx.root);
        for path in [target.join("files/f"), target.clone()] {
            assert!(matches!(
                fx.context.trash_into(&elsewhere, &path),
                Err(TrashError::InvalidSource { .. })
            ));
            assert!(matches!(
                fx.context.trash(&path),
                Err(TrashError::InvalidSource { .. })
            ));
        }
        assert!(target.join("files/f").exists());
        assert!(!elsewhere.path().exists());
        fs::set_permissions(&target, fs::Permissions::from_mode(0o700)).unwrap();
    }

    #[test]
    fn a_relative_target_is_checked_as_configured_too() {
        let fx = Fx::new();
        let data = fx.root.join("data");
        fs::create_dir(&data).unwrap();
        let _ = kept(&fx, "data/kept");
        symlink(&data, fx.root.join("alias")).unwrap();
        symlink("kept", data.join("Trash")).unwrap();
        let alias = fx.root.join("alias");
        let policy = Policy {
            exclusions: Exclusions::new().subtree(alias.join("kept"), "nfs"),
            ..Policy::default()
        };
        let context = Context::new(&alias, uid(), &fx.work, LocalZone::utc())
            .unwrap()
            .with_policy(policy);
        let source = fx.file("f", "f");
        assert!(matches!(
            context.trash_into(&context.home_trash(), &source),
            Err(TrashError::NoTrash { attempts, .. }) if matches!(attempts[0].failure, AttemptFailure::Excluded(_))
        ));
        let planned = context.plan(&source).unwrap();
        assert!(matches!(
            planned.skipped[0].failure,
            AttemptFailure::Excluded(_)
        ));
        assert!(source.exists());
        assert!(!data.join("kept/files").exists());
    }

    #[test]
    fn directories_on_the_way_need_only_search_permission() {
        if !cfg!(any(target_os = "linux", target_os = "android")) {
            return;
        }
        unprivileged(|| {
            let fx = Fx::new();
            let target = kept(&fx, "disk/kept");
            link_home(&fx, &target);
            let disk = fx.root.join("disk");
            fs::set_permissions(&disk, fs::Permissions::from_mode(0o111)).unwrap();
            let trashed = fx.context.trash(fx.file("f", "f"));
            let opened = fx.context.open(&fx.context.home_trash()).map(|_| ());
            fs::set_permissions(&disk, fs::Permissions::from_mode(0o755)).unwrap();
            assert_eq!(trashed.unwrap().trash.kind(), TrashKind::Home);
            opened.unwrap();
            assert_eq!(fs::read_to_string(target.join("files/f")).unwrap(), "f");
        });
    }

    #[test]
    fn an_unreadable_target_still_holds_what_it_holds() {
        unprivileged(|| {
            let fx = Fx::new();
            let target = kept(&fx, "disk/kept");
            link_home(&fx, &target);
            let _ = fx.context.trash(fx.file("f", "f")).unwrap();
            // Searchable, not readable: reached, but not opened for reading.
            fs::set_permissions(&target, fs::Permissions::from_mode(0o300)).unwrap();
            let elsewhere = fx.context.private_trash(&fx.root);
            let into = fx.context.trash_into(&elsewhere, target.join("files/f"));
            let trashed = fx.context.trash(target.join("files/f"));
            fs::set_permissions(&target, fs::Permissions::from_mode(0o700)).unwrap();
            for result in [into, trashed] {
                assert!(
                    matches!(
                        result,
                        Err(TrashError::InvalidSource {
                            problem: SourceProblem::InTrash,
                            ..
                        })
                    ),
                    "{result:?}"
                );
            }
            assert!(target.join("files/f").exists());
        });
    }

    #[test]
    fn every_spelling_of_the_target_is_checked_with_files_and_info() {
        let fx = Fx::new();
        let data = fx.root.join("data");
        fs::create_dir(&data).unwrap();
        let _ = kept(&fx, "data/kept");
        symlink(&data, fx.root.join("alias")).unwrap();
        let alias = fx.root.join("alias");
        let source = fx.file("f", "f");
        // A relative target under the data home's alias, an absolute one
        // through the alias, and a link on the way to it: the exclusion
        // names only `files` or `info` there.
        symlink("alias/kept", fx.root.join("hop")).unwrap();
        for to in [
            PathBuf::from("kept"),
            alias.join("kept"),
            fx.root.join("hop"),
        ] {
            let _ = fs::remove_file(data.join("Trash"));
            symlink(&to, data.join("Trash")).unwrap();
            for child in ["files", "info"] {
                let policy = Policy {
                    exclusions: Exclusions::new().subtree(alias.join("kept").join(child), "nfs"),
                    ..Policy::default()
                };
                let context = Context::new(&alias, uid(), &fx.work, LocalZone::utc())
                    .unwrap()
                    .with_policy(policy);
                assert!(
                    matches!(
                        context.trash_into(&context.home_trash(), &source),
                        Err(TrashError::NoTrash { attempts, .. }) if matches!(attempts[0].failure, AttemptFailure::Excluded(_))
                    ),
                    "{to:?} {child}"
                );
            }
        }
        assert!(source.exists());
        assert!(!data.join("kept/files").exists());
    }

    #[test]
    fn an_unreadable_data_home_is_still_followed() {
        if !cfg!(any(target_os = "linux", target_os = "android")) {
            return;
        }
        unprivileged(|| {
            let fx = Fx::new();
            let target = kept(&fx, "disk/kept");
            link_home(&fx, &target);
            let _ = fx.context.trash(fx.file("f", "f")).unwrap();
            // Searchable, not readable: the link can be read without
            // listing the directory.
            fs::set_permissions(&fx.home, fs::Permissions::from_mode(0o300)).unwrap();
            let elsewhere = fx.context.private_trash(&fx.root);
            let into = fx.context.trash_into(&elsewhere, target.join("files/f"));
            let trashed = fx.context.trash(fx.file("g", "g"));
            let opened = fx.context.open(&fx.context.home_trash()).map(|_| ());
            fs::set_permissions(&fx.home, fs::Permissions::from_mode(0o700)).unwrap();
            assert!(
                matches!(
                    into,
                    Err(TrashError::InvalidSource {
                        problem: SourceProblem::InTrash,
                        ..
                    })
                ),
                "{into:?}"
            );
            assert_eq!(trashed.unwrap().trash.kind(), TrashKind::Home);
            opened.unwrap();
            assert!(target.join("files/f").exists());
            assert!(target.join("files/g").exists());
        });
    }

    #[test]
    fn an_excluded_files_or_info_still_marks_the_target_as_trash() {
        for child in ["files", "info"] {
            let fx = Fx::new();
            let target = kept(&fx, "disk/kept");
            link_home(&fx, &target);
            let _ = fx.context.trash(fx.file("f", "f")).unwrap();
            let context = with_policy(&fx, |policy| {
                policy.exclusions = Exclusions::new().subtree(target.join(child), "nfs");
            });
            let elsewhere = context.private_trash(&fx.root);
            let moved = context.trash_into(&elsewhere, target.join("files/f"));
            // In an excluded `files`, the source is refused from its path
            // already; with `info` excluded, it is still known as trashed.
            let refused = match &moved {
                Err(TrashError::Excluded(_)) => child == "files",
                Err(TrashError::InvalidSource {
                    problem: SourceProblem::InTrash,
                    ..
                }) => true,
                _ => false,
            };
            assert!(refused, "{child}: {moved:?}");
            assert!(matches!(
                context.trash_into(&elsewhere, &target),
                Err(TrashError::InvalidSource {
                    problem: SourceProblem::HoldsTrash,
                    ..
                })
            ));
            // The home trash itself is not used.
            assert!(matches!(
                context.trash_into(&context.home_trash(), fx.file("g", "g")),
                Err(TrashError::NoTrash { attempts, .. }) if matches!(attempts[0].failure, AttemptFailure::Excluded(_))
            ));
            fault::pretend_volume(&fx.root);
            let planned = context.plan(fx.file("h", "h")).unwrap();
            assert!(matches!(
                planned.skipped[0].failure,
                AttemptFailure::Excluded(_)
            ));
            fault::clear();
            assert!(target.join("files/f").exists());
            assert!(!elsewhere.path().exists());
        }
    }

    #[test]
    fn an_excluded_logical_files_or_info_still_marks_the_target_as_trash() {
        for child in ["files", "info"] {
            let fx = Fx::new();
            let target = kept(&fx, "disk/kept");
            link_home(&fx, &target);
            let _ = fx.context.trash(fx.file("f", "f")).unwrap();
            let context = with_policy(&fx, |policy| {
                policy.exclusions = Exclusions::new().subtree(fx.trash().join(child), "nfs");
            });
            let elsewhere = context.private_trash(&fx.root);
            assert!(
                matches!(
                    context.trash_into(&elsewhere, target.join("files/f")),
                    Err(TrashError::InvalidSource {
                        problem: SourceProblem::InTrash,
                        ..
                    })
                ),
                "{child}"
            );
            assert!(matches!(
                context.trash_into(&context.home_trash(), fx.file("g", "g")),
                Err(TrashError::NoTrash { attempts, .. }) if matches!(attempts[0].failure, AttemptFailure::Excluded(_))
            ));
            assert!(target.join("files/f").exists());
            assert!(!elsewhere.path().exists());
        }
    }

    #[test]
    fn a_data_home_that_cannot_be_opened_is_located_by_path() {
        let fx = Fx::new();
        let target = kept(&fx, "disk/kept");
        link_home(&fx, &target);
        let _ = fx.context.trash(fx.file("f", "f")).unwrap();
        let elsewhere = fx.context.private_trash(&fx.root);
        // Opening `$XDG_DATA_HOME` fails, as a read-only open of a
        // directory that can be searched but not read does.
        fault::fail(Stage::OpenDataHome, 0);
        assert!(matches!(
            fx.context.trash_into(&elsewhere, target.join("files/f")),
            Err(TrashError::InvalidSource {
                problem: SourceProblem::InTrash,
                ..
            })
        ));
        assert!(target.join("files/f").exists());
        assert!(!elsewhere.path().exists());
    }

    #[test]
    fn locating_by_path_checks_what_following_checks() {
        let fx = Fx::new();
        let target = kept(&fx, "disk/kept");
        symlink(fx.root.join("disk"), fx.root.join("via")).unwrap();
        symlink("via/kept", fx.root.join("hop")).unwrap();
        link_home(&fx, "../hop");
        let home = fx.context.home_trash();
        let none = Exclusions::new().anchored(Path::new("/"));
        assert_eq!(crate::dir::locate(&home, &none), Some(target.clone()));
        let info = Exclusions::new()
            .subtree(target.join("info"), "nfs")
            .anchored(Path::new("/"));
        assert_eq!(crate::dir::locate(&home, &info), Some(target.clone()));
        let disk = Exclusions::new()
            .subtree(fx.root.join("disk"), "nfs")
            .anchored(Path::new("/"));
        assert_eq!(crate::dir::locate(&home, &disk), None);
        fs::remove_file(fx.trash()).unwrap();
        symlink(fx.root.join("missing"), fx.trash()).unwrap();
        assert_eq!(crate::dir::locate(&home, &none), None);
    }
}
