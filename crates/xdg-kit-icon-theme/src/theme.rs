//! One theme's `index.theme`, and the cheap listing of installed themes.

use std::path::{Path, PathBuf};

use xdg_kit_basedir::BaseDirs;
use xdg_kit_desktop_entry::KeyFile;
use xdg_kit_locale::Locales;

use crate::size::{RuleKind, SizingRule};
use crate::{Diagnostic, Limits, work};

/// The group a theme's definition lives in.
const GROUP: &str = "Icon Theme";

/// One subdirectory a theme declares, with its sizing rule.
#[derive(Debug, Clone)]
pub(crate) struct ThemeDir {
    /// The declared path, relative to the theme's directory.
    pub path: String,
    pub rule: SizingRule,
}

/// A parsed `index.theme`: the parts lookup and listing need.
#[derive(Debug)]
pub(crate) struct ThemeFile {
    pub inherits: Vec<String>,
    pub dirs: Vec<ThemeDir>,
    pub hidden: bool,
    /// The key file, kept for localized listing fields.
    pub file: KeyFile,
}

/// Splits one of the spec's comma-separated lists (the desktop-entry
/// semicolon rules do not apply here).
pub(crate) fn comma_list(value: &str) -> Vec<String> {
    work::count(value.len());
    value
        .split(',')
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(str::to_owned)
        .collect()
}

/// Parses `bytes` as an `index.theme`. `None` (with a diagnostic) when
/// the file has no `Icon Theme` group or no usable directory list —
/// such a file does not define a theme, and the next root's may.
pub(crate) fn parse(
    bytes: &[u8],
    path: &Path,
    diagnostics: &mut Vec<Diagnostic>,
) -> Option<ThemeFile> {
    work::count(bytes.len());
    let file = KeyFile::parse(bytes);
    let value = |key: &str| file.string(GROUP, key);
    let Some(listed) = value("Directories") else {
        diagnostics.push(Diagnostic::new(
            Some(path.to_owned()),
            "no Directories in the Icon Theme group; the file defines no theme",
        ));
        return None;
    };
    let mut names = comma_list(&listed);
    if let Some(scaled) = value("ScaledDirectories") {
        names.extend(comma_list(&scaled));
    }
    let mut dirs = Vec::with_capacity(names.len());
    for name in names {
        work::count(1 + name.len());
        if !descendant(&name) {
            diagnostics.push(Diagnostic::new(
                Some(path.to_owned()),
                format!("the directory \"{name}\" is skipped: not a relative subdirectory"),
            ));
            continue;
        }
        match directory(&file, &name) {
            Ok(rule) => dirs.push(ThemeDir { path: name, rule }),
            Err(problem) => diagnostics.push(Diagnostic::new(
                Some(path.to_owned()),
                format!("the directory \"{name}\" is skipped: {problem}"),
            )),
        }
    }
    if dirs.is_empty() {
        diagnostics.push(Diagnostic::new(
            Some(path.to_owned()),
            "no usable directory; the file defines no theme",
        ));
        return None;
    }
    let inherits = value("Inherits").map_or_else(Vec::new, |list| comma_list(&list));
    let hidden = value("Hidden").as_deref() == Some("true");
    Some(ThemeFile {
        inherits,
        dirs,
        hidden,
        file,
    })
}

/// Whether a declared directory stays inside the theme: a nonempty
/// relative path with no `.` or `..` components (the spec describes
/// theme subdirectories; anything else would redirect enumeration).
pub(crate) fn descendant(path: &str) -> bool {
    work::count(path.len());
    !path.is_empty()
        && !path.starts_with('/')
        && path
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != "..")
}

/// Whether a theme id is one directory name.
pub(crate) fn single_component(id: &str) -> bool {
    work::count(id.len());
    !id.is_empty() && id != "." && id != ".." && !id.bytes().any(|byte| byte == b'/' || byte == 0)
}

/// The sizing rule of one declared directory, or why it is unusable
/// (skipping is policy; the resolver still guards).
fn directory(file: &KeyFile, name: &str) -> Result<SizingRule, String> {
    let value = |key: &str| file.string(name, key);
    let number = |key: &str| -> Result<Option<u32>, String> {
        match value(key) {
            None => Ok(None),
            Some(text) => text
                .trim()
                .parse::<u32>()
                .map(Some)
                .map_err(|_| format!("{key} is not a number")),
        }
    };
    let Some(size) = number("Size")? else {
        return Err("no Size".to_owned());
    };
    let scale = number("Scale")?.unwrap_or(1);
    if scale == 0 {
        return Err("a Scale of zero".to_owned());
    }
    let kind = match value("Type").as_deref() {
        Some("Fixed") => RuleKind::Fixed,
        Some("Scalable") => RuleKind::Scalable,
        // The spec's default, unknown values included.
        _ => RuleKind::Threshold,
    };
    let min = number("MinSize")?.unwrap_or(size);
    let max = number("MaxSize")?.unwrap_or(size);
    if kind == RuleKind::Scalable && min > max {
        return Err("MinSize is above MaxSize".to_owned());
    }
    Ok(SizingRule {
        kind,
        size,
        scale,
        min,
        max,
        threshold: number("Threshold")?.unwrap_or(2),
    })
}

/// One installed theme, as a picker lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ThemeListing {
    /// The directory name the theme is loaded by.
    pub id: String,
    /// `Name`, translated for the given locales.
    pub name: Option<String>,
    /// `Comment`, translated for the given locales.
    pub comment: Option<String>,
    /// Whether the theme asks not to be shown to users (`Hidden`): an
    /// implementation detail of another theme, usually.
    pub hidden: bool,
    /// `Example`: an icon name that shows the theme off.
    pub example: Option<String>,
}

/// The themes installed under `base_dirs`' icon roots, by directory
/// name, first root first (reading only each theme's first readable
/// `index.theme`). Cheap enough for a picker; loading is separate and
/// scoped.
#[must_use]
pub fn themes(base_dirs: &BaseDirs, locales: &Locales) -> Vec<ThemeListing> {
    let limits = Limits::default();
    let mut listed = Vec::new();
    let mut defined = xdg_kit_basedir::__hashed::Set::<
        xdg_kit_basedir::__hashed::Str<crate::work::Counted>,
    >::new();
    for root in crate::index::icon_roots(base_dirs) {
        let Ok(entries) = std::fs::read_dir(&root) else {
            continue;
        };
        let mut ids: Vec<_> = entries
            .filter_map(Result::ok)
            // Follow symlinks: a linked theme directory loads, so it
            // lists (`metadata`, not the entry's own type).
            .filter(|entry| std::fs::metadata(entry.path()).is_ok_and(|found| found.is_dir()))
            .filter_map(|entry| entry.file_name().into_string().ok())
            .collect();
        ids.sort_unstable();
        for id in ids {
            // The id hashed and, when new, copied (counted by the set).
            work::count(1);
            if defined.contains(&id) {
                continue;
            }
            let index = root.join(&id).join("index.theme");
            let Ok(bytes) = xdg_kit_basedir::read_regular_file_with(&index, limits.max_file_len)
            else {
                continue;
            };
            // The same gate loading uses: the first *usable* definition
            // across the roots wins; an unusable higher-priority file
            // hides nothing.
            let mut ignored = Vec::new();
            let Some(theme) = parse(&bytes, &index, &mut ignored) else {
                continue;
            };
            defined.insert(&id);
            listed.push(listing(&id, &theme, locales));
        }
    }
    listed
}

/// The localized listing fields of a loaded theme's own file.
pub(crate) fn listing(id: &str, theme: &ThemeFile, locales: &Locales) -> ThemeListing {
    ThemeListing {
        id: id.to_owned(),
        name: theme.file.locale_string(GROUP, "Name", locales),
        comment: theme.file.locale_string(GROUP, "Comment", locales),
        hidden: theme.hidden,
        example: theme.file.string(GROUP, "Example"),
    }
}

/// Where a theme's `index.theme` would be under `root`.
pub(crate) fn index_path(root: &Path, id: &str) -> PathBuf {
    root.join(id).join("index.theme")
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "[Icon Theme]\nName=Sample\nName[de]=Beispiel\nComment=A theme\n\
        Hidden=true\nExample=folder\nInherits=parent-a, parent-b\n\
        Directories=16x16/apps,scalable/apps,bad-scale,no-size,inverted,odd-type\n\
        ScaledDirectories=16x16@2/apps\n\
        [16x16/apps]\nSize=16\nType=Fixed\nContext=Applications\n\
        [scalable/apps]\nSize=128\nType=Scalable\nMinSize=8\nMaxSize=256\n\
        [16x16@2/apps]\nSize=16\nScale=2\nType=Fixed\n\
        [bad-scale]\nSize=16\nScale=0\n\
        [no-size]\nType=Fixed\n\
        [inverted]\nSize=16\nType=Scalable\nMinSize=20\nMaxSize=10\n\
        [odd-type]\nSize=24\nType=Whatever\nThreshold=4\n";

    #[test]
    fn the_sample_parses_with_the_right_rules_and_skips() {
        let mut diagnostics = Vec::new();
        let theme = parse(SAMPLE.as_bytes(), Path::new("x"), &mut diagnostics).unwrap();
        assert_eq!(theme.inherits, ["parent-a", "parent-b"], "comma list");
        assert!(theme.hidden);
        let paths: Vec<_> = theme.dirs.iter().map(|dir| dir.path.as_str()).collect();
        assert_eq!(
            paths,
            ["16x16/apps", "scalable/apps", "odd-type", "16x16@2/apps"]
        );
        assert_eq!(theme.dirs[0].rule.kind, RuleKind::Fixed);
        assert_eq!((theme.dirs[1].rule.min, theme.dirs[1].rule.max), (8, 256));
        // An unknown Type is the spec's default, Threshold.
        assert_eq!(theme.dirs[2].rule.kind, RuleKind::Threshold);
        assert_eq!(theme.dirs[2].rule.threshold, 4);
        assert_eq!(theme.dirs[3].rule.scale, 2, "a scaled directory");
        let skipped: Vec<_> = diagnostics.iter().map(|d| d.message.as_str()).collect();
        assert_eq!(skipped.len(), 3, "{skipped:?}");
        assert!(skipped.iter().any(|m| m.contains("Scale of zero")));
        assert!(skipped.iter().any(|m| m.contains("no Size")));
        assert!(skipped.iter().any(|m| m.contains("MinSize is above")));
    }

    #[test]
    fn files_that_define_no_theme_say_so() {
        for (text, expected) in [
            ("[Other]\nDirectories=a\n", "no Directories"),
            (
                "[Icon Theme]\nDirectories=a\n[a]\nType=Fixed\n",
                "no usable directory",
            ),
        ] {
            let mut diagnostics = Vec::new();
            assert!(parse(text.as_bytes(), Path::new("x"), &mut diagnostics).is_none());
            assert!(
                diagnostics.iter().any(|d| d.message.contains(expected)),
                "{diagnostics:?}"
            );
        }
    }

    #[test]
    fn listings_localize_and_expose_the_picker_fields() {
        let mut diagnostics = Vec::new();
        let theme = parse(SAMPLE.as_bytes(), Path::new("x"), &mut diagnostics).unwrap();
        let german = Locales::new(["de"]);
        let shown = listing("sample", &theme, &german);
        assert_eq!(shown.name.as_deref(), Some("Beispiel"));
        assert_eq!(shown.comment.as_deref(), Some("A theme"));
        assert!(shown.hidden);
        assert_eq!(shown.example.as_deref(), Some("folder"));
        let plain = listing("sample", &theme, &Locales::new(Vec::<String>::new()));
        assert_eq!(plain.name.as_deref(), Some("Sample"));
    }
}
