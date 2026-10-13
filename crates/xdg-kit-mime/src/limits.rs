//! Limits on reading a MIME database.

/// The deepest rule nesting [`Limits::max_rule_depth`] can allow, even with
/// [`Limits::unlimited`]; larger values count as this one. Rules are checked,
/// copied, matched and dropped recursively; this depth fits a 2 MiB thread
/// stack. On x86-64 with rustc 1.98.1, a 2 MiB stack held rules 1,493 deep in
/// a debug build and 4,656 deep in a release build.
pub const MAX_RULE_DEPTH: usize = 1024;

/// The default of [`Limits::max_diagnostics`].
pub(crate) const DEFAULT_DIAGNOSTICS: usize = 100;

/// Limits on reading a database's files. Functions without `_with` use
/// [`Limits::default`]; `None` means no limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct Limits {
    /// The largest file read, in bytes; defaults to
    /// [`xdg_kit_basedir::MAX_FILE_LEN`]. Larger files are reported as unreadable.
    /// With `None`, files may be any size, and memory use grows with the file.
    ///
    /// Per-type XML files with many entity declarations can use much more memory
    /// than their file size. Approximate measured peaks for large files are:
    ///
    /// - 15 times the size for a long chain of entities, each referring to the next;
    /// - 10 times for many references or `ATTLIST` defaults, including standalone ones;
    /// - 8 times for many parameter entities, read separately or nested;
    /// - 20 to 35 times with no [`Limits::max_namespace_bindings`].
    ///
    /// Memory stays linear in the file's size. In addition,
    /// [`Limits::entity_expansion_extra`] can add up to about 16 times its own
    /// value regardless of file size: about 1 MiB at the default setting.
    /// A 2 KiB file whose included `ATTLIST` defaults consumed the whole extra
    /// budget used 768 KiB in measurements. Small files can therefore use a
    /// much larger multiple of their size in memory.
    ///
    /// Root detection reads caller-supplied data without applying this limit
    /// and uses roughly the same amount of memory for similar data.
    pub max_file_len: Option<u64>,
    /// The maximum diagnostics per file, 100 by default, followed by one line
    /// reporting that there are more. With `None`, every problem is listed.
    /// Memory use then grows roughly with the file's size, and can be many
    /// times larger because each diagnostic also stores the file's path.
    pub max_diagnostics: Option<usize>,
    /// The longest string read from a `mime.cache`, in bytes: 4,096 by default.
    /// Longer strings are unreadable; the search for a string's end stops at
    /// this length. With `None`, each search can reach the end of the file, so
    /// reading time can grow with the square of its size.
    pub max_cache_string: Option<usize>,
    /// The bytes a `mime.cache` reader may copy out, per byte of the file:
    /// 8 by default, plus [`Limits::cache_copy_extra`]. Strings are shared
    /// by many entries in a real cache; the budget stops a file that
    /// points many entries at one large region. `None`: no budget, so
    /// memory can grow with the square of the file's size.
    pub cache_copy_per_byte: Option<usize>,
    /// The bytes a `mime.cache` reader may copy out besides
    /// [`Limits::cache_copy_per_byte`]: 64 KiB by default.
    pub cache_copy_extra: usize,
    /// The deepest nesting of `magic` and `treemagic` tests, 64 by
    /// default (the top level is 0). A deeper test is skipped, with a
    /// diagnostic. Values above [`MAX_RULE_DEPTH`] count as that.
    pub max_rule_depth: usize,
    /// The maximum number of namespace declarations (`xmlns` and `xmlns:`
    /// attributes) across all open elements: 128 by default. Counts shadowed
    /// declarations too, but excludes `xmlns:xml`, which binds nothing new.
    /// Applies to per-type XML files and documents examined for their root element.
    /// At the first tag that exceeds the limit, skips the rest of a per-type
    /// file with a diagnostic, or leaves a document's root element unmatched.
    /// With `None`, memory use grows with the number of declarations.
    ///
    /// Regardless of this limit, each distinct namespace bound to a prefix is
    /// kept once until the file has been read. This memory use is bounded by
    /// the file's size ([`Limits::max_file_len`]). Approximate measured peaks
    /// are 15 times the size with an entity-heavy document type, or 20 to 35
    /// times with `None` and many declarations; memory stays linear in the
    /// file's size.
    ///
    /// With or without a limit, time is linear in the file's size within the
    /// entity expansion budget ([`Limits::entity_expansion_per_byte`]), except
    /// for `ATTLIST` defaults in an open document type. Finding the entity loops
    /// each default reaches at its position takes about log2 n passes over the
    /// internal subset's n entities and references: n log n time, with linear
    /// memory use.
    pub max_namespace_bindings: Option<usize>,
    /// The bytes entities may expand to per byte of input: 1 by default,
    /// plus [`Limits::entity_expansion_extra`]. Applies to per-type XML files
    /// and documents examined for their root element, with separate budgets:
    ///
    /// - Internal parameter entities included by a document type may use its
    ///   length times this value, plus the extra. Exceeding that budget makes
    ///   the document type declare nothing.
    /// - All namespace declarations together may use the file's length times
    ///   this value, plus the extra. A declaration that exceeds it is unreadable.
    ///   Each application of an internal subset's `ATTLIST` default for
    ///   `xmlns`, a prefixed `xmlns:` declaration, or `xml:lang` charges its
    ///   name and literal as if written. The first tag whose defaults exceed
    ///   the budget skips the rest of a per-type file with a diagnostic, or
    ///   leaves a document's root element unmatched. If a default's entity
    ///   expansion exceeds the budget, only that value is unreadable, as for
    ///   a written declaration.
    ///
    /// Both budgets count bytes read, keeping work linear in the file's size.
    /// Memory use also grows with this value. Included bytes can contain
    /// `ATTLIST` default references, each stored in 24 bytes: for a 3-byte `&e;`
    /// reference, 8 bytes per input byte. The buffer doubles as it grows, so
    /// storage can reach 16 bytes per included byte.
    ///
    /// `None` disables the budget and protection against entity bombs. Nested
    /// entities can make time and memory grow exponentially with file size,
    /// up to the memory the system grants, and a long `ATTLIST` default given
    /// to many elements costs its length each time. Every allocation sized by an
    /// expansion reserves memory first. If an expansion or its copy exceeds
    /// `usize` or allocation fails, the namespace declaration is unreadable.
    /// If included defaults cannot be held, the document type declares nothing.
    /// Namespaces bound to prefixes are retained until the file has been read,
    /// even after their elements close, so with `None` whether a later binding
    /// fits depends on earlier bindings: increasing the allowed memory can make
    /// a binding readable, then unreadable, then readable again.
    ///
    /// Small allocations do not use this check: Rust's standard library aborts
    /// if they fail. After a large expansion leaves almost no free memory, a
    /// small allocation can still abort the process, as in any Rust code. On
    /// systems that overcommit memory, the process can also be killed when it
    /// uses memory the system granted but does not have.
    pub entity_expansion_per_byte: Option<usize>,
    /// The bytes entities may expand to besides
    /// [`Limits::entity_expansion_per_byte`]: 64 KiB by default.
    pub entity_expansion_extra: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_file_len: Some(xdg_kit_basedir::MAX_FILE_LEN),
            max_diagnostics: Some(DEFAULT_DIAGNOSTICS),
            max_cache_string: Some(4096),
            cache_copy_per_byte: Some(8),
            cache_copy_extra: 64 * 1024,
            max_rule_depth: 64,
            max_namespace_bindings: Some(128),
            entity_expansion_per_byte: Some(1),
            entity_expansion_extra: 64 * 1024,
        }
    }
}

impl Limits {
    /// Removes all limits except the rule depth ceiling, [`MAX_RULE_DEPTH`].
    /// [`Limits::cache_copy_extra`] and [`Limits::entity_expansion_extra`]
    /// keep their defaults but are unused until
    /// [`Limits::cache_copy_per_byte`] or
    /// [`Limits::entity_expansion_per_byte`] is set. Memory use grows with
    /// the files read; both time and memory can grow with the square of a
    /// `mime.cache` file's size. With no entity expansion budget, the
    /// protection against entity bombs is given up: time can grow
    /// exponentially with an XML file's size, and memory with it up to what
    /// the system grants; what an expansion needs is refused if it cannot
    /// be held (which depends on the bindings read before it), but a small
    /// allocation after it can still abort
    /// ([`Limits::entity_expansion_per_byte`]). Without
    /// [`Limits::max_namespace_bindings`], XML files take time as with it.
    #[must_use]
    pub fn unlimited() -> Self {
        Self {
            max_file_len: None,
            max_diagnostics: None,
            max_cache_string: None,
            cache_copy_per_byte: None,
            cache_copy_extra: Self::default().cache_copy_extra,
            max_rule_depth: MAX_RULE_DEPTH,
            max_namespace_bindings: None,
            entity_expansion_per_byte: None,
            entity_expansion_extra: Self::default().entity_expansion_extra,
        }
    }

    /// The rule depth allowed: [`Limits::max_rule_depth`], at most
    /// [`MAX_RULE_DEPTH`].
    pub(crate) fn rule_depth(&self) -> usize {
        self.max_rule_depth.min(MAX_RULE_DEPTH)
    }
}
