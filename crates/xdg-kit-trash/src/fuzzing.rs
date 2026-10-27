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
