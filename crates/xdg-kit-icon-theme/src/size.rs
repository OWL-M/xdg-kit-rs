//! A theme directory's sizing rule: whether it matches a request
//! exactly, and how far it is otherwise. The formulas are written out
//! here (and in the design) because the spec's published pseudocode is
//! unreliable in its threshold branch; all arithmetic runs in `u64`, so
//! the public `u32` request fields cannot overflow it.

/// How a directory's `Type` interprets its size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuleKind {
    /// Exactly `size`.
    Fixed,
    /// Anything in `min..=max`.
    Scalable,
    /// `size`, give or take `threshold`.
    Threshold,
}

/// One theme directory's sizing rule, as `index.theme` declares it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SizingRule {
    /// How the directory's size is interpreted.
    pub kind: RuleKind,
    /// The nominal size (`Size`).
    pub size: u32,
    /// The scale the directory serves (`Scale`, default 1).
    pub scale: u32,
    /// `MinSize` (default `size`).
    pub min: u32,
    /// `MaxSize` (default `size`).
    pub max: u32,
    /// `Threshold` (default 2).
    pub threshold: u32,
}

impl SizingRule {
    /// Whether the directory matches a request of `size` at `scale`
    /// exactly: equal scale, and the size within the rule's window
    /// (fixed: equal; threshold: `size ± threshold`, the lower bound
    /// clamped at zero; scalable: `min..=max`).
    #[must_use]
    pub fn matches(&self, size: u32, scale: u32) -> bool {
        if self.scale != scale {
            return false;
        }
        match self.kind {
            RuleKind::Fixed => self.size == size,
            RuleKind::Threshold => {
                let size = u64::from(size);
                let floor = u64::from(self.size).saturating_sub(u64::from(self.threshold));
                let ceiling = u64::from(self.size) + u64::from(self.threshold);
                size >= floor && size <= ceiling
            }
            RuleKind::Scalable => size >= self.min && size <= self.max,
        }
    }

    /// The distance to a physical request of `physical` pixels
    /// (`size × scale`), any scale: fixed and threshold directories
    /// measure from their own physical size, scalable ones are zero
    /// inside their physical range and measure to the nearer endpoint
    /// outside it.
    #[must_use]
    pub fn distance(&self, physical: u64) -> u64 {
        let scale = u64::from(self.scale);
        match self.kind {
            RuleKind::Fixed | RuleKind::Threshold => {
                (u64::from(self.size) * scale).abs_diff(physical)
            }
            RuleKind::Scalable => {
                let floor = u64::from(self.min) * scale;
                let ceiling = u64::from(self.max) * scale;
                if physical < floor {
                    floor - physical
                } else {
                    physical.saturating_sub(ceiling)
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixed(size: u32, scale: u32) -> SizingRule {
        SizingRule {
            kind: RuleKind::Fixed,
            size,
            scale,
            min: size,
            max: size,
            threshold: 2,
        }
    }

    #[test]
    fn the_reviewed_high_dpi_arithmetic_holds() {
        // A request of size 16 at scale 2 is 32 physical pixels.
        let request = 16u64 * 2;
        assert!(fixed(16, 2).matches(16, 2), "the exact directory");
        assert!(!fixed(32, 1).matches(16, 2), "scale differs");
        assert!(!fixed(32, 2).matches(16, 2), "size differs");
        assert_eq!(fixed(32, 1).distance(request), 0, "physical 32");
        assert_eq!(fixed(32, 2).distance(request), 32, "physical 64");
    }

    #[test]
    fn threshold_windows_clamp_at_zero() {
        let wide = SizingRule {
            kind: RuleKind::Threshold,
            size: 2,
            scale: 1,
            min: 2,
            max: 2,
            threshold: 10,
        };
        assert!(wide.matches(1, 1), "the floor clamps at zero");
        assert!(wide.matches(12, 1));
        assert!(!wide.matches(13, 1));
        assert_eq!(wide.distance(40), 38, "measured from the nominal size");
    }

    #[test]
    fn scalable_ranges_measure_to_the_nearer_endpoint() {
        let range = SizingRule {
            kind: RuleKind::Scalable,
            size: 128,
            scale: 2,
            min: 8,
            max: 256,
            threshold: 2,
        };
        assert!(range.matches(8, 2) && range.matches(256, 2));
        assert!(!range.matches(257, 2) && !range.matches(7, 2));
        assert_eq!(range.distance(16), 0, "inside [16, 512]");
        assert_eq!(range.distance(4), 12);
        assert_eq!(range.distance(1000), 488);
    }

    #[test]
    fn extreme_values_cannot_overflow() {
        let huge = SizingRule {
            kind: RuleKind::Threshold,
            size: u32::MAX,
            scale: u32::MAX,
            min: u32::MAX,
            max: u32::MAX,
            threshold: u32::MAX,
        };
        assert!(huge.matches(1, u32::MAX), "the window reaches down to zero");
        let _ = huge.distance(u64::from(u32::MAX));
        let scalable = SizingRule {
            kind: RuleKind::Scalable,
            ..huge
        };
        let _ = scalable.distance(0);
    }
}
