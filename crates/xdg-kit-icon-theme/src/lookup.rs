//! The one resolver: the spec's algorithm over the snapshot's candidate
//! records, and the two name-list operations layered on it.

use std::path::PathBuf;

use crate::index::{Candidate, IconThemes, LoadedTheme, PNG, SVG, Unthemed, XPM};
use crate::size::SizingRule;
use crate::work;

/// What is being asked for: a nominal size at a scale, in the formats
/// the caller can use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IconRequest {
    /// The nominal size in logical pixels.
    pub size: u32,
    /// The scale factor (1 for ordinary displays).
    pub scale: u32,
    /// Which file formats the caller can render.
    pub formats: Formats,
}

/// The formats a caller accepts. Within one candidate, `png` beats `svg`
/// beats `xpm`, the spec's extension order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Formats {
    /// Accept PNG files.
    pub png: bool,
    /// Accept SVG files.
    pub svg: bool,
    /// Accept XPM files.
    pub xpm: bool,
}

impl Default for Formats {
    fn default() -> Self {
        Self {
            png: true,
            svg: true,
            xpm: true,
        }
    }
}

impl Formats {
    /// The accepted formats as the index's mask.
    fn mask(self) -> u8 {
        let mut mask = 0;
        if self.png {
            mask |= PNG;
        }
        if self.svg {
            mask |= SVG;
        }
        if self.xpm {
            mask |= XPM;
        }
        mask
    }
}

/// A found icon's file format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IconFormat {
    /// A `.png` file.
    Png,
    /// An `.svg` file.
    Svg,
    /// An `.xpm` file.
    Xpm,
}

impl IconFormat {
    /// The best format in `mask`, in the spec's extension order.
    fn best(mask: u8) -> Option<Self> {
        if mask & PNG != 0 {
            Some(Self::Png)
        } else if mask & SVG != 0 {
            Some(Self::Svg)
        } else if mask & XPM != 0 {
            Some(Self::Xpm)
        } else {
            None
        }
    }

    fn extension(self) -> &'static str {
        match self {
            Self::Png => "png",
            Self::Svg => "svg",
            Self::Xpm => "xpm",
        }
    }
}

/// A resolved icon: a candidate from the snapshot, not a guarantee the
/// file still exists — the caller opens it and handles that race.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Icon {
    path: PathBuf,
    matched: String,
    theme: Option<String>,
    format: IconFormat,
    rule: Option<SizingRule>,
}

impl Icon {
    /// Where the icon file was when the snapshot was built.
    #[must_use]
    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    /// Which of the requested (or generated fallback) names hit.
    #[must_use]
    pub fn matched_name(&self) -> &str {
        &self.matched
    }

    /// The theme it came from; `None` for an unthemed fallback hit.
    #[must_use]
    pub fn theme(&self) -> Option<&str> {
        self.theme.as_deref()
    }

    /// The file's format.
    #[must_use]
    pub fn format(&self) -> IconFormat {
        self.format
    }

    /// The directory's sizing rule, which tells an exact hit from a
    /// scaled candidate; `None` for an unthemed fallback hit, which has
    /// none.
    #[must_use]
    pub fn sizing(&self) -> Option<&SizingRule> {
        self.rule.as_ref()
    }
}

impl IconThemes {
    /// Looks one exact name up: the spec's algorithm — each theme of the
    /// chain in turn (exact size match first, then the closest by the
    /// spec's distance), then the unthemed fallback.
    #[must_use]
    pub fn lookup(&self, name: &str, request: &IconRequest) -> Option<Icon> {
        self.lookup_names(&[name], request)
    }

    /// Looks ordered alternatives up: **every name is tried within one
    /// theme before its parent is consulted** (the spec's multi-name
    /// algorithm), so a generic name in the current theme beats a more
    /// specific one in a parent.
    #[must_use]
    pub fn lookup_names(&self, names: &[&str], request: &IconRequest) -> Option<Icon> {
        let mask = request.formats.mask();
        if mask == 0 {
            return None;
        }
        for theme in &self.chain {
            for name in names {
                work::count(1);
                if let Some(icon) = self.in_theme(theme, name, request, mask) {
                    return Some(icon);
                }
            }
        }
        for name in names {
            work::count(1);
            if let Some(icon) = self.unthemed(name, mask) {
                return Some(icon);
            }
        }
        None
    }

    /// Looks a name up with the Icon Naming spec's specificity fallback:
    /// the generated chain (`network-wired-disconnected` →
    /// `network-wired` → `network`, a `-symbolic` suffix preserved while
    /// shortening) through [`IconThemes::lookup_names`]. The naming spec
    /// itself advises applications not to lean on this fallback, and
    /// MIME-type icon names do not follow the hyphen convention — this
    /// is a convenience for the names that do. Styles are never crossed
    /// here; a caller wanting GIO's behavior (opposite-style
    /// alternatives appended after the chain) builds that list and calls
    /// `lookup_names`.
    #[must_use]
    pub fn lookup_with_fallbacks(&self, name: &str, request: &IconRequest) -> Option<Icon> {
        let chain = fallback_chain(name);
        let refs: Vec<&str> = chain.iter().map(String::as_str).collect();
        self.lookup_names(&refs, request)
    }

    /// The spec's two passes over one theme's candidates for one name.
    fn in_theme(
        &self,
        theme: &LoadedTheme,
        name: &str,
        request: &IconRequest,
        mask: u8,
    ) -> Option<Icon> {
        // The lookup; the map counts the name's bytes.
        work::count(1);
        let candidates = theme.names.get(name)?;
        let usable = |candidate: &Candidate| candidate.formats & mask;
        // Pass one: the first exact match in (directory, root) order.
        for candidate in candidates {
            work::count(1);
            let rule = theme.dirs[candidate.dir as usize].rule;
            if usable(candidate) != 0 && rule.matches(request.size, request.scale) {
                return Some(self.themed_icon(theme, name, candidate, usable(candidate)));
            }
        }
        // Pass two: the smallest distance, first in order on ties.
        let physical = u64::from(request.size) * u64::from(request.scale);
        let mut best: Option<(u64, &Candidate)> = None;
        for candidate in candidates {
            work::count(1);
            if usable(candidate) == 0 {
                continue;
            }
            let distance = theme.dirs[candidate.dir as usize].rule.distance(physical);
            if best.is_none_or(|(won, _)| distance < won) {
                best = Some((distance, candidate));
            }
        }
        best.map(|(_, candidate)| self.themed_icon(theme, name, candidate, usable(candidate)))
    }

    fn themed_icon(
        &self,
        theme: &LoadedTheme,
        name: &str,
        candidate: &Candidate,
        usable: u8,
    ) -> Icon {
        let format = IconFormat::best(usable).expect("filtered before");
        let dir = &theme.dirs[candidate.dir as usize];
        let path = self.roots[candidate.root as usize]
            .join(&theme.id)
            .join(&dir.path)
            .join(format!("{name}.{}", format.extension()));
        Icon {
            path,
            matched: name.to_owned(),
            theme: Some(theme.id.clone()),
            format,
            rule: Some(dir.rule),
        }
    }

    /// The unthemed fallback: the first fallback directory holding the
    /// name, best format within it.
    fn unthemed(&self, name: &str, mask: u8) -> Option<Icon> {
        work::count(1);
        let found = self.fallback.get(name)?;
        let hit: &Unthemed = found.iter().find(|place| {
            work::count(1);
            place.formats & mask != 0
        })?;
        let format = IconFormat::best(hit.formats & mask).expect("filtered above");
        Some(Icon {
            path: self.fallback_dirs[hit.dir as usize]
                .join(format!("{name}.{}", format.extension())),
            matched: name.to_owned(),
            theme: None,
            format,
            rule: None,
        })
    }
}

/// The Icon Naming spec's specificity chain for `name`, most specific
/// first, a `-symbolic` suffix preserved while shortening.
pub(crate) fn fallback_chain(name: &str) -> Vec<String> {
    work::count(name.len());
    let (base, symbolic) = match name.strip_suffix("-symbolic") {
        Some(base) if !base.is_empty() => (base, true),
        _ => (name, false),
    };
    let mut chain = Vec::new();
    let mut rest = base;
    loop {
        chain.push(if symbolic {
            format!("{rest}-symbolic")
        } else {
            rest.to_owned()
        });
        match rest.rsplit_once('-') {
            Some((shorter, _)) if !shorter.is_empty() => rest = shorter,
            _ => break,
        }
    }
    chain
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fallback_chains_shorten_and_keep_the_style() {
        let chain = |name: &str| fallback_chain(name);
        assert_eq!(
            chain("network-wired-disconnected"),
            ["network-wired-disconnected", "network-wired", "network"]
        );
        assert_eq!(
            chain("network-wired-disconnected-symbolic"),
            [
                "network-wired-disconnected-symbolic",
                "network-wired-symbolic",
                "network-symbolic"
            ],
            "the style suffix survives the shortening"
        );
        assert_eq!(chain("folder"), ["folder"]);
        assert_eq!(chain("folder-symbolic"), ["folder-symbolic"]);
        assert_eq!(chain("-symbolic"), ["-symbolic"], "no empty base");
    }
}
