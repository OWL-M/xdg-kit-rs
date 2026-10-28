//! Entry points for the fuzz targets in `fuzz/`. Built only with
//! `--cfg fuzzing`, which `cargo fuzz` sets; not part of the API.

use std::path::{Component, Path};

use crate::info::{self, AbsolutePaths};
use crate::{
    DeletionDate, Field, TrashInfo, format_directory_sizes, parse_directory_sizes, parse_mountinfo,
};

/// Parses `bytes` as a trash info file and holds it to the crate's
/// contracts: a decoded `Path` written back reads back the same bytes; a
/// resolved origin is absolute, ends in a name and has no `..` or NUL, and
/// in a per-volume trash under the default policy stays below the top
/// directory; a valid date prints in a form that parses to itself.
pub fn trash_info(bytes: &[u8]) {
    let Ok(parsed) = TrashInfo::parse(bytes) else {
        return;
    };
    if let Field::Valid(date) = parsed.deletion_date() {
        assert_eq!(
            DeletionDate::parse(date.to_string().as_bytes()),
            Some(*date)
        );
    }
    let Field::Valid(path) = parsed.path() else {
        return;
    };
    let date = DeletionDate::new(2000, 1, 1, 0, 0, 0).expect("a valid date");
    let written = info::format(path, date);
    let again = TrashInfo::parse(&written).expect("the header written");
    assert_eq!(
        again.path(),
        &Field::Valid(path.clone()),
        "the path round-trips"
    );
    assert_eq!(again.deletion_date(), &Field::Valid(date));
    let top = Path::new("/top");
    for volume in [
        None,
        Some(AbsolutePaths::UnderTopdir),
        Some(AbsolutePaths::Reject),
    ] {
        let Ok(resolved) = info::resolve(path, top, volume) else {
            continue;
        };
        assert!(resolved.is_absolute());
        assert!(matches!(
            resolved.components().next_back(),
            Some(Component::Normal(_))
        ));
        assert!(
            resolved
                .components()
                .all(|part| part != Component::ParentDir)
        );
        assert!(!std::os::unix::ffi::OsStrExt::as_bytes(resolved.as_os_str()).contains(&0));
        if volume.is_some() {
            assert!(resolved.starts_with(top) && resolved != top);
        }
    }
}

/// Parses `bytes` as `directorysizes`: what is kept writes out and reads
/// back the same, with nothing rejected.
pub fn directory_sizes(bytes: &[u8]) {
    let parsed = parse_directory_sizes(bytes, 1_000);
    assert!(parsed.entries.len() <= 1_000);
    assert!(parsed.problems.len() <= crate::MAX_CACHE_PROBLEMS);
    assert!(parsed.problems.len() <= parsed.rejected);
    let written = format_directory_sizes(&parsed.entries);
    let again = parse_directory_sizes(&written, 1_000);
    assert_eq!(again.entries, parsed.entries);
    assert_eq!((again.rejected, again.truncated), (0, false));
}

/// Parses `bytes` as `/proc/self/mountinfo`: never panics, and every mount
/// point is absolute.
pub fn mountinfo(bytes: &[u8]) {
    if let Ok(mounts) = parse_mountinfo(bytes) {
        assert!(mounts.iter().all(|mount| mount.mount_point.is_absolute()));
    }
}

/// Checks `bytes` as a path against fixed exclusions: never panics, and
/// the answer does not change when separators are repeated or `.` added.
pub fn exclusions(bytes: &[u8]) {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt as _;

    use crate::Exclusions;

    let rules = Exclusions::new()
        .mount("/a", "a")
        .subtree("/b/c", "c")
        .with_mount_points(["/a".into(), "/a/n".into(), "/b".into()])
        .anchored(Path::new("/"));
    let mut path = b"/".to_vec();
    path.extend_from_slice(bytes);
    let plain = rules.check(Path::new(OsStr::from_bytes(&path))).is_ok();
    let doubled: Vec<u8> = path
        .iter()
        .flat_map(|&byte| {
            if byte == b'/' {
                vec![b'/', b'/']
            } else {
                vec![byte]
            }
        })
        .collect();
    assert_eq!(
        rules.check(Path::new(OsStr::from_bytes(&doubled))).is_ok(),
        plain
    );
    let mut dotted = b"/./".to_vec();
    dotted.extend_from_slice(bytes);
    assert_eq!(
        rules.check(Path::new(OsStr::from_bytes(&dotted))).is_ok(),
        plain
    );
}
