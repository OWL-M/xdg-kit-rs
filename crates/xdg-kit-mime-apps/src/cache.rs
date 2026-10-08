//! Writes `mimeinfo.cache`, the file `update-desktop-database`
//! (desktop-file-utils) creates in an `applications` folder to list the
//! types listed by each desktop entry. The file is outside the MIME
//! Applications spec. This crate does not read it; it writes it for other
//! programs, including `GLib`, which uses it for a folder's associations.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};

use xdg_kit_desktop_entry::DesktopEntry;

use crate::TypePattern;

/// The `mimeinfo.cache` for the `applications` folder `folder`, as
/// `update-desktop-database` writes it: `[MIME Cache]`, then one
/// `type=id;id;` line per type, both in byte order, for every desktop entry
/// under the folder (sub-folders give IDs joined by `-`) that is not
/// deleted ([`DesktopEntry::hidden`]) and is in UTF-8
/// ([`DesktopEntry::supports_encoding`]), as the tool lists them. Unlike
/// `AppIndex`, entries of no or unknown `Type` are listed, as the tool does.
/// Items of `MimeType=` that are not types or `media/*` wildcards (see
/// [`TypePattern::parse`]) are left out, `*/*` too, as the tool does.
///
/// # Errors
///
/// When `folder` cannot be listed. Files that cannot be read are skipped.
pub fn mimeinfo_cache(folder: &Path) -> io::Result<Vec<u8>> {
    let mut types: BTreeMap<String, BTreeSet<Vec<u8>>> = BTreeMap::new();
    let mut visited = HashSet::new();
    collect(folder, b"", &mut visited, &mut types, true)?;
    let mut out = b"[MIME Cache]\n".to_vec();
    for (mime, ids) in types {
        out.extend_from_slice(mime.as_bytes());
        out.push(b'=');
        for id in ids {
            out.extend_from_slice(&id);
            out.push(b';');
        }
        out.push(b'\n');
    }
    Ok(out)
}

/// Writes [`mimeinfo_cache`] of `folder` to `folder/mimeinfo.cache`
/// atomically, by writing and syncing a new file in the folder, then
/// renaming it. Like `update-desktop-database`, this replaces a link named
/// `mimeinfo.cache` without following it and uses the default permissions.
///
/// # Errors
///
/// When `folder` cannot be listed or the file cannot be written.
pub fn write_mimeinfo_cache(folder: &Path) -> io::Result<()> {
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let bytes = mimeinfo_cache(folder)?;
    // A name left over from a crashed writer is skipped, not an error.
    let (temporary, mut file) = loop {
        let temporary = folder.join(format!(
            ".mimeinfo.cache.{}.{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
        {
            Ok(file) => break (temporary, file),
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {}
            Err(err) => return Err(err),
        }
    };
    let written = (|| {
        file.write_all(&bytes)?;
        file.sync_all()?;
        fs::rename(&temporary, folder.join("mimeinfo.cache"))?;
        // The rename itself is durable once the folder is synced.
        fs::File::open(folder)?.sync_all()
    })();
    if written.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    written
}

fn collect(
    dir: &Path,
    prefix: &[u8],
    visited: &mut HashSet<(u64, u64)>,
    types: &mut BTreeMap<String, BTreeSet<Vec<u8>>>,
    top: bool,
) -> io::Result<()> {
    // A folder reached again through a link is listed once, under the first
    // name in byte order, as `AppIndex` gives its files IDs
    // (`update-desktop-database` lists every path).
    let metadata = fs::metadata(dir)?;
    if !visited.insert((metadata.dev(), metadata.ino())) {
        return Ok(());
    }
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(err) if top => return Err(err),
        Err(_) => return Ok(()),
    };
    let mut entries: Vec<_> = entries.flatten().collect();
    entries.sort_by_key(fs::DirEntry::file_name);
    for entry in entries {
        let name = entry.file_name();
        let path = entry.path();
        let Ok(metadata) = fs::metadata(&path) else {
            continue;
        };
        let mut id = prefix.to_vec();
        id.extend_from_slice(name.as_bytes());
        if metadata.is_dir() {
            id.push(b'-');
            collect(&path, &id, visited, types, false)?;
        } else if metadata.is_file() && name.as_bytes().ends_with(b".desktop") {
            // Only regular files: reading a FIFO would wait for a writer.
            let Ok(entry) = DesktopEntry::read(&path) else {
                continue;
            };
            // As `update-desktop-database`: not deleted and in UTF-8. Unlike
            // `AppIndex`, entries of no or unknown `Type` are listed.
            if entry.hidden() || !entry.supports_encoding() {
                continue;
            }
            let file = entry.key_file();
            for item in file.list("Desktop Entry", "MimeType").unwrap_or_default() {
                let item = item.trim_end();
                if TypePattern::parse(item).is_some_and(|pattern| pattern != TypePattern::Any) {
                    types.entry(item.to_owned()).or_default().insert(id.clone());
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn types_and_ids_in_byte_order() {
        let dir = tempfile::tempdir().unwrap();
        let write = |name: &str, text: &str| {
            let path = dir.path().join(name);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, text).unwrap();
        };
        write(
            "b.desktop",
            "[Desktop Entry]\nType=Application\nName=B\nExec=b\nMimeType=text/plain;image/*;*/*;not a type;\n",
        );
        write(
            "kde/a.desktop",
            "[Desktop Entry]\nType=Application\nName=A\nExec=a\nMimeType=text/plain;\n",
        );
        write(
            "hidden.desktop",
            "[Desktop Entry]\nType=Application\nName=H\nExec=h\nHidden=true\nMimeType=text/plain;\n",
        );
        write("notes.txt", "MimeType=text/plain;\n");
        assert_eq!(
            String::from_utf8(mimeinfo_cache(dir.path()).unwrap()).unwrap(),
            "[MIME Cache]\nimage/*=b.desktop;\ntext/plain=b.desktop;kde-a.desktop;\n"
        );
        write_mimeinfo_cache(dir.path()).unwrap();
        assert_eq!(
            fs::read(dir.path().join("mimeinfo.cache")).unwrap(),
            mimeinfo_cache(dir.path()).unwrap()
        );
        assert!(mimeinfo_cache(&dir.path().join("missing")).is_err());
    }

    #[test]
    fn special_files_and_linked_folders() {
        let dir = tempfile::tempdir().unwrap();
        let entry = "[Desktop Entry]\nType=Application\nName=N\nExec=n\nMimeType=text/plain;\n";
        fs::create_dir_all(dir.path().join("z")).unwrap();
        fs::write(dir.path().join("z/nested.desktop"), entry).unwrap();
        std::os::unix::fs::symlink("z", dir.path().join("a")).unwrap();
        // `mknodat` is not in rustix on Apple systems.
        #[cfg(not(target_vendor = "apple"))]
        rustix::fs::mknodat(
            rustix::fs::CWD,
            dir.path().join("blocked.desktop"),
            rustix::fs::FileType::Fifo,
            rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
            0,
        )
        .unwrap();
        assert_eq!(
            String::from_utf8(mimeinfo_cache(dir.path()).unwrap()).unwrap(),
            "[MIME Cache]\ntext/plain=a-nested.desktop;\n",
            "the FIFO is skipped, and the folder is listed once, under its first name"
        );
    }
}
