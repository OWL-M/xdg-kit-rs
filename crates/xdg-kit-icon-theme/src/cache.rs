//! Reading GTK's `icon-theme.cache` (gtk-update-icon-cache, format
//! major version 1), defensively and read-only: a sound, fresh cache is
//! an enumeration shortcut for one root's copy of a theme; anything else
//! falls back to scanning, with a diagnostic. The file is parsed from
//! bytes — never mapped — with every offset bounds-checked, bucket
//! chains cycle-guarded, and directory indices validated against the
//! cache's own list *and* the theme's declared directories.

use std::collections::HashMap;
use std::path::Path;
use std::time::SystemTime;

use crate::index::{PNG, SVG, XPM};
use crate::theme::ThemeDir;
use crate::{Limits, work};

/// GTK's suffix flags.
const FLAG_XPM: u16 = 1;
const FLAG_SVG: u16 = 2;
const FLAG_PNG: u16 = 4;

/// One root's cache for one theme: icon names and formats per declared
/// directory.
#[derive(Debug, Default)]
pub(crate) struct ThemeCache {
    pub by_dir: HashMap<String, Vec<(String, u8)>>,
}

/// Reads the cache beside `theme_dir` (one root's copy of a theme), when
/// there is one. `Ok(None)` means no cache file; `Err` is why an
/// existing one is unusable (stale, truncated, hostile, unsupported) —
/// the caller scans instead and keeps the reason as a diagnostic.
pub(crate) fn read_theme_cache(
    theme_dir: &Path,
    declared: &[ThemeDir],
    limits: &Limits,
) -> Result<Option<ThemeCache>, String> {
    let path = theme_dir.join("icon-theme.cache");
    let bytes = match xdg_kit_basedir::read_regular_file_with(&path, limits.max_file_len) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(format!("cannot be read: {err}")),
    };
    // Freshness: the GTK rule (no older than the theme directory),
    // extended to every declared subdirectory the cache would stand in
    // for — a changed subdirectory does not touch the top-level mtime.
    let cache_mtime = mtime(&path).ok_or("the cache has no modification time")?;
    let mut covered = vec![theme_dir.to_owned()];
    covered.extend(declared.iter().map(|dir| theme_dir.join(&dir.path)));
    for dir in covered {
        work::count(1);
        if let Some(changed) = mtime(&dir)
            && changed > cache_mtime
        {
            return Err(format!("older than {}", dir.display()));
        }
    }
    parse(&bytes, declared, limits).map(Some)
}

fn mtime(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).ok()?.modified().ok()
}

/// Records one image entry under its directory — when the theme declares
/// that directory and the flags name a format this crate serves.
fn record(
    cache: &mut ThemeCache,
    dirs: &[Option<&str>],
    dir_index: usize,
    flags: u16,
    name: &str,
) -> Result<(), String> {
    let Some(dir) = dirs
        .get(dir_index)
        .ok_or("a directory index past the list")?
    else {
        // Valid in the cache, undeclared by the theme.
        return Ok(());
    };
    let formats = format_mask(flags);
    if formats != 0 {
        cache
            .by_dir
            .entry((*dir).to_owned())
            .or_default()
            .push((name.to_owned(), formats));
    }
    Ok(())
}

/// Hashed membership of the declared directories: each cached directory
/// costs its own bytes, not a scan of the declared list.
fn declared_set(
    declared: &[ThemeDir],
) -> xdg_kit_basedir::__hashed::Set<xdg_kit_basedir::__hashed::Str<crate::work::Counted>> {
    let mut known = xdg_kit_basedir::__hashed::Set::new();
    for dir in declared {
        work::count(1);
        known.insert(dir.path.as_str());
    }
    known
}

/// A cached icon name must be a filename stem: a path-shaped one would
/// manufacture candidates no scan could produce.
fn filename_stem(name: &str) -> Result<(), String> {
    if name.is_empty() || name == "." || name == ".." || name.contains('/') {
        return Err("an icon name that is not a filename".to_owned());
    }
    Ok(())
}

/// GTK's suffix flags, as the index's format mask.
fn format_mask(flags: u16) -> u8 {
    let mut formats = 0u8;
    if flags & FLAG_PNG != 0 {
        formats |= PNG;
    }
    if flags & FLAG_SVG != 0 {
        formats |= SVG;
    }
    if flags & FLAG_XPM != 0 {
        formats |= XPM;
    }
    formats
}

/// Parses the cache bytes into per-directory listings.
fn parse(bytes: &[u8], declared: &[ThemeDir], limits: &Limits) -> Result<ThemeCache, String> {
    // All offsets come from untrusted 32-bit fields: every sum is
    // checked, so a hostile value is a refusal on 32-bit targets too,
    // never an overflow.
    let read_u16 = |at: usize| -> Result<u16, String> {
        at.checked_add(2)
            .and_then(|end| bytes.get(at..end))
            .map(|lane| u16::from_be_bytes([lane[0], lane[1]]))
            .ok_or_else(|| "truncated".to_owned())
    };
    let read_u32 = |at: usize| -> Result<u32, String> {
        at.checked_add(4)
            .and_then(|end| bytes.get(at..end))
            .map(|lane| u32::from_be_bytes([lane[0], lane[1], lane[2], lane[3]]))
            .ok_or_else(|| "truncated".to_owned())
    };
    // `base + head + stride * at`, checked.
    let slot = |base: usize, head: usize, stride: usize, at: usize| -> Result<usize, String> {
        stride
            .checked_mul(at)
            .and_then(|offset| offset.checked_add(head))
            .and_then(|offset| offset.checked_add(base))
            .ok_or_else(|| "an offset past addressable memory".to_owned())
    };
    let read_str = |at: usize| -> Result<&str, String> {
        let rest = bytes.get(at..).ok_or("a string offset past the file")?;
        let nul = rest
            .iter()
            .take(1024)
            .position(|&byte| byte == 0)
            .ok_or("an unterminated string")?;
        work::count(nul);
        std::str::from_utf8(&rest[..nul]).map_err(|_| "a string that is not UTF-8".to_owned())
    };
    if (read_u16(0)?, read_u16(2)?) != (1, 0) {
        return Err("an unsupported cache version".to_owned());
    }
    let hash_offset = read_u32(4)? as usize;
    let dir_list_offset = read_u32(8)? as usize;
    // The cache's own directory list, mapped onto the declared
    // directories; indices into it that the theme does not declare are
    // dropped (a cache cannot smuggle a directory in).
    let n_dirs = read_u32(dir_list_offset)? as usize;
    if n_dirs > limits.max_cache_entries {
        return Err("more directories than the limit".to_owned());
    }
    // The whole offset table must fit the file before anything sized by
    // its count is allocated: without configured limits, the count alone
    // must never become an allocation request.
    if slot(dir_list_offset, 4, 4, n_dirs)? > bytes.len() {
        return Err("a directory table past the file".to_owned());
    }
    let known = declared_set(declared);
    let mut dirs: Vec<Option<&str>> = Vec::with_capacity(n_dirs);
    for at in 0..n_dirs {
        work::count(1);
        let name = read_str(read_u32(slot(dir_list_offset, 4, 4, at)?)? as usize)?;
        dirs.push(known.contains(name).then_some(name));
    }
    let mut cache = ThemeCache::default();
    let n_buckets = read_u32(hash_offset)? as usize;
    if n_buckets > limits.max_cache_entries {
        return Err("more buckets than the limit".to_owned());
    }
    if slot(hash_offset, 4, 4, n_buckets)? > bytes.len() {
        return Err("a bucket table past the file".to_owned());
    }
    // A revisited node offset is a cycle (or chains sharing a tail),
    // detected exactly and independently of the configurable limits —
    // `Limits::unlimited` must not turn a cyclic file into a hang. The
    // record budget below is the aggregate bound: shared image lists
    // expand once per referencing node, and every expansion counts.
    let mut visited = std::collections::HashSet::new();
    let mut expanded = 0usize;
    for bucket in 0..n_buckets {
        let mut node = read_u32(slot(hash_offset, 4, 4, bucket)?)?;
        while node != 0xFFFF_FFFF {
            work::count(1);
            if !visited.insert(node) {
                return Err("a cycle in the hash chains".to_owned());
            }
            if visited.len() > limits.max_cache_entries {
                return Err("more entries than the limit".to_owned());
            }
            let at = node as usize;
            let next = read_u32(at)?;
            let name = read_str(read_u32(slot(at, 4, 0, 0)?)? as usize)?;
            filename_stem(name)?;
            let images = read_u32(slot(at, 8, 0, 0)?)? as usize;
            let n_images = read_u32(images)? as usize;
            expanded = expanded.saturating_add(n_images);
            if expanded > limits.max_cache_entries {
                return Err("more image records than the limit".to_owned());
            }
            for image in 0..n_images {
                work::count(1 + name.len());
                let image_at = slot(images, 4, 8, image)?;
                let dir_index = read_u16(image_at)? as usize;
                let flags = read_u16(slot(image_at, 2, 0, 0)?)?;
                record(&mut cache, &dirs, dir_index, flags, name)?;
            }
            node = next;
        }
    }
    Ok(cache)
}

#[cfg(test)]
pub(crate) mod builder {
    //! A hand-rolled cache writer for the tests (and nothing else): the
    //! same layout gtk-update-icon-cache produces, byte for byte enough
    //! for the reader.

    /// Builds a version-1 cache with every icon in one bucket chain.
    pub(crate) fn cache(dirs: &[&str], icons: &[(&str, u16, u16)]) -> Vec<u8> {
        let multi: Vec<(&str, Vec<(u16, u16)>)> = icons
            .iter()
            .map(|(name, dir, flags)| (*name, vec![(*dir, *flags)]))
            .collect();
        cache_multi(dirs, &multi)
    }

    /// As [`cache`], with several image records per icon.
    pub(crate) fn cache_multi(dirs: &[&str], icons: &[(&str, Vec<(u16, u16)>)]) -> Vec<u8> {
        // (name, dir index, flags)
        let mut out = vec![0u8; 12];
        out[0..4].copy_from_slice(&[0, 1, 0, 0]); // major 1, minor 0
        // Directory list first.
        let dir_list_offset = out.len();
        out.extend_from_slice(&u32::try_from(dirs.len()).unwrap().to_be_bytes());
        let mut dir_slots = Vec::new();
        for _ in dirs {
            dir_slots.push(out.len());
            out.extend_from_slice(&[0; 4]);
        }
        for (slot, name) in dir_slots.iter().zip(dirs) {
            let at = u32::try_from(out.len()).unwrap();
            out[*slot..*slot + 4].copy_from_slice(&at.to_be_bytes());
            out.extend_from_slice(name.as_bytes());
            out.push(0);
        }
        // One bucket; its chain holds every icon.
        let hash_offset = out.len();
        out.extend_from_slice(&1u32.to_be_bytes());
        let bucket_slot = out.len();
        out.extend_from_slice(&0xFFFF_FFFFu32.to_be_bytes());
        let mut previous_slot = bucket_slot;
        for (name, images) in icons {
            // The image list for this icon.
            let images_at = u32::try_from(out.len()).unwrap();
            out.extend_from_slice(&u32::try_from(images.len()).unwrap().to_be_bytes());
            for (dir, flags) in images {
                out.extend_from_slice(&dir.to_be_bytes());
                out.extend_from_slice(&flags.to_be_bytes());
                out.extend_from_slice(&[0; 4]); // image data offset, unused
            }
            let name_at = u32::try_from(out.len()).unwrap();
            out.extend_from_slice(name.as_bytes());
            out.push(0);
            let node_at = u32::try_from(out.len()).unwrap();
            out.extend_from_slice(&0xFFFF_FFFFu32.to_be_bytes()); // next
            out.extend_from_slice(&name_at.to_be_bytes());
            out.extend_from_slice(&images_at.to_be_bytes());
            out[previous_slot..previous_slot + 4].copy_from_slice(&node_at.to_be_bytes());
            previous_slot = node_at as usize;
        }
        let hash = u32::try_from(hash_offset).unwrap();
        out[4..8].copy_from_slice(&hash.to_be_bytes());
        let dir_list = u32::try_from(dir_list_offset).unwrap();
        out[8..12].copy_from_slice(&dir_list.to_be_bytes());
        out
    }
}
