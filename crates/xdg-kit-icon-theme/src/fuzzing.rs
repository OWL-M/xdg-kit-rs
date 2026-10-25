//! Entry points for the fuzz targets in `fuzz/`. Built only with
//! `--cfg fuzzing`, which `cargo fuzz` sets; not part of the API.

use std::sync::LazyLock;

use crate::size::{RuleKind, SizingRule};
use crate::theme::ThemeDir;
use crate::{Formats, IconRequest, IconThemes, Limits, LoadOptions};

fn declared() -> Vec<ThemeDir> {
    let rule = |kind, size| SizingRule {
        kind,
        size,
        scale: 1,
        min: size,
        max: size,
        threshold: 2,
    };
    vec![
        ThemeDir {
            path: "16x16/apps".to_owned(),
            rule: rule(RuleKind::Fixed, 16),
        },
        ThemeDir {
            path: "scalable/apps".to_owned(),
            rule: rule(RuleKind::Scalable, 128),
        },
    ]
}

/// Parses `bytes` as a GTK `icon-theme.cache` under the default and under
/// tight limits: a hostile cache is a refusal or a bounded parse, never a
/// panic, a hang or an oversized allocation, and what parses lists only
/// the declared directories.
pub fn gtk_cache(bytes: &[u8]) {
    let declared = declared();
    let tight = Limits {
        max_cache_entries: 8,
        ..Limits::default()
    };
    for limits in [Limits::default(), tight] {
        match crate::cache::parse(bytes, &declared, &limits) {
            Ok(cache) => {
                for dir in &declared {
                    for (name, formats) in
                        cache.by_dir.get(&dir.path).map_or(&[][..], Vec::as_slice)
                    {
                        assert!(!name.is_empty());
                        assert_eq!(*formats & !7, 0, "only the three format bits");
                    }
                }
            }
            Err(unusable) => {
                // The refusal reason is already a formatted String.
                drop(unusable);
            }
        }
    }
}

/// The empty snapshot adversarial names are resolved against. Built once:
/// the shape of the input never changes what loading does.
static EMPTY: LazyLock<IconThemes> = LazyLock::new(|| {
    IconThemes::from_roots(
        Vec::new(),
        "fuzz",
        Limits::default(),
        LoadOptions::default(),
        Vec::new(),
        Vec::new(),
    )
});

/// Runs the lookups, the fallback-generating one included, over an empty
/// snapshot with `data` as the name: an adversarial name must terminate
/// without panicking, however it is hyphenated.
pub fn lookup(data: &[u8]) {
    let Ok(name) = std::str::from_utf8(data) else {
        return;
    };
    let request = IconRequest {
        size: 16,
        scale: 1,
        formats: Formats::default(),
    };
    // The snapshot has no themes, but its unthemed fallback may hold the
    // machine's real /usr/share/pixmaps: the result is exercised, not
    // assumed empty.
    let _ = EMPTY.lookup(name, &request);
    let _ = EMPTY.lookup_with_fallbacks(name, &request);
}
