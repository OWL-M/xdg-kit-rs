//! End-to-end fixtures: chains, selection, precedence, fallbacks.

use std::fs;
use std::path::Path;

use xdg_kit_basedir::BaseDirs;

use crate::{Formats, IconRequest, IconThemes, Limits, LoadOptions};

/// Base dirs whose icon roots are exactly the given directories
/// (`HOME/.icons` first, then `data_home/icons`).
pub(crate) fn base(home: &Path, data: &Path) -> BaseDirs {
    let (home, data) = (home.to_owned(), data.to_owned());
    BaseDirs::from_vars(move |name| match name {
        "HOME" => Some(home.clone().into_os_string()),
        "XDG_DATA_HOME" => Some(data.clone().into_os_string()),
        "XDG_DATA_DIRS" => Some("/nonexistent-data".into()),
        _ => None,
    })
    .unwrap()
}

/// Writes a theme's `index.theme` under `root` and returns its dir.
pub(crate) fn theme(root: &Path, id: &str, body: &str) -> std::path::PathBuf {
    let dir = root.join("icons").join(id);
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("index.theme"), body).unwrap();
    dir
}

/// Creates an empty icon file.
pub(crate) fn icon(theme_dir: &Path, sub: &str, name: &str) {
    let dir = theme_dir.join(sub);
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join(name), "").unwrap();
}

pub(crate) fn request(size: u32, scale: u32) -> IconRequest {
    IconRequest {
        size,
        scale,
        formats: Formats::default(),
    }
}

const SIMPLE: &str = "[Icon Theme]\nName=T\nDirectories=16,32,16@2\n\
    [16]\nSize=16\nType=Fixed\n[32]\nSize=32\nType=Fixed\n\
    [16@2]\nSize=16\nScale=2\nType=Fixed\n";

#[test]
fn selection_follows_the_specs_phases() {
    let home = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    let dir = theme(data.path(), "t", SIMPLE);
    icon(&dir, "16", "a.png");
    icon(&dir, "32", "a.png");
    icon(&dir, "16@2", "a.png");
    let themes = IconThemes::load(&base(home.path(), data.path()), "t");
    // Exact at scale 2 is the 16@2 directory, not physical-32 at scale 1.
    let hit = themes.lookup("a", &request(16, 2)).unwrap();
    assert!(hit.path().ends_with("t/16@2/a.png"), "{hit:?}");
    assert!(hit.sizing().unwrap().matches(16, 2));
    // With no exact match (size 20 scale 2, physical 40): 32@1 has
    // distance 8, 16@2 distance 8, 16@1 distance 24 — the tie breaks by
    // directory order, and "16@2" is declared after "32".
    let near = themes.lookup("a", &request(20, 2)).unwrap();
    assert!(near.path().ends_with("t/32/a.png"), "{near:?}");
    // The reviewer's case: size 16 scale 2 with only 32@1 and 32@2 on
    // offer picks physical 32 over physical 64.
    let coarse_dir = theme(
        data.path(),
        "coarse",
        "[Icon Theme]\nName=C\nDirectories=32,32@2\n\
         [32]\nSize=32\nType=Fixed\n[32@2]\nSize=32\nScale=2\nType=Fixed\n",
    );
    icon(&coarse_dir, "32", "b.png");
    icon(&coarse_dir, "32@2", "b.png");
    let coarse = IconThemes::load(&base(home.path(), data.path()), "coarse");
    let b = coarse.lookup("b", &request(16, 2)).unwrap();
    assert!(b.path().ends_with("coarse/32/b.png"), "{b:?}");
}

#[test]
fn themes_beat_parents_and_chains_close_with_hicolor() {
    let home = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    let child = theme(
        data.path(),
        "child",
        "[Icon Theme]\nName=C\nInherits=mid-a, mid-b\nDirectories=16\n[16]\nSize=16\nType=Fixed\n",
    );
    let mid_a = theme(
        data.path(),
        "mid-a",
        "[Icon Theme]\nName=A\nInherits=child\nDirectories=16\n[16]\nSize=16\nType=Fixed\n",
    );
    let mid_b = theme(
        data.path(),
        "mid-b",
        "[Icon Theme]\nName=B\nInherits=mid-a\nDirectories=16\n[16]\nSize=16\nType=Fixed\n",
    );
    let hicolor = theme(
        data.path(),
        "hicolor",
        "[Icon Theme]\nName=H\nDirectories=16\n[16]\nSize=16\nType=Fixed\n",
    );
    icon(&mid_a, "16", "x.png");
    icon(&mid_b, "16", "x.png");
    icon(&hicolor, "16", "x.png");
    icon(&hicolor, "16", "only-fallback.png");
    let themes = IconThemes::load(&base(home.path(), data.path()), "child");
    // Depth-first, cycle-guarded, deduplicated, hicolor appended.
    let chain: Vec<_> = themes.chain().collect();
    assert_eq!(chain, ["child", "mid-a", "mid-b", "hicolor"]);
    let x = themes.lookup("x", &request(16, 1)).unwrap();
    assert_eq!(x.theme(), Some("mid-a"), "the earlier theme wins");
    let fallback = themes.lookup("only-fallback", &request(16, 1)).unwrap();
    assert_eq!(fallback.theme(), Some("hicolor"));
    let _ = child;
}

#[test]
fn multi_name_precedence_prefers_the_current_themes_generic_icon() {
    let home = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    let current = theme(
        data.path(),
        "current",
        "[Icon Theme]\nName=C\nInherits=parent\nDirectories=16\n[16]\nSize=16\nType=Fixed\n",
    );
    let parent = theme(
        data.path(),
        "parent",
        "[Icon Theme]\nName=P\nDirectories=16\n[16]\nSize=16\nType=Fixed\n",
    );
    icon(&current, "16", "network-wired.png");
    icon(&parent, "16", "network-wired-disconnected.png");
    let themes = IconThemes::load(&base(home.path(), data.path()), "current");
    let hit = themes
        .lookup_with_fallbacks("network-wired-disconnected", &request(16, 1))
        .unwrap();
    assert_eq!(hit.theme(), Some("current"), "generic-in-current wins");
    assert_eq!(hit.matched_name(), "network-wired");
    // The plain lookup still finds the specific one in the parent.
    let specific = themes
        .lookup("network-wired-disconnected", &request(16, 1))
        .unwrap();
    assert_eq!(specific.theme(), Some("parent"));
}

#[test]
fn candidate_precedence_is_subdirectory_then_root_then_extension() {
    let home = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    // The theme is defined in the data root; the home root carries only
    // icon files for it.
    let data_theme = theme(data.path(), "t", SIMPLE);
    let home_theme = home.path().join(".icons").join("t");
    icon(&home_theme, "32", "a.svg");
    icon(&data_theme, "32", "a.png");
    icon(&data_theme, "16", "a.xpm");
    let themes = IconThemes::load(&base(home.path(), data.path()), "t");
    // Subdirectory beats root: "16" (declared first) in the data root
    // beats "32" anywhere, even though only an xpm lives there.
    let at16 = themes.lookup("a", &request(16, 1)).unwrap();
    assert!(at16.path().ends_with("t/16/a.xpm"), "{at16:?}");
    // Within one subdirectory, the earlier root beats the later, even
    // when the later root's format would win inside one candidate.
    let at32 = themes.lookup("a", &request(32, 1)).unwrap();
    assert!(
        at32.path().starts_with(home.path()),
        "the home root's svg wins over the data root's png: {at32:?}"
    );
    assert_eq!(at32.format(), crate::IconFormat::Svg);
    // Format filtering falls through to the next root's png.
    let no_svg = themes
        .lookup(
            "a",
            &IconRequest {
                size: 32,
                scale: 1,
                formats: Formats {
                    svg: false,
                    ..Formats::default()
                },
            },
        )
        .unwrap();
    assert_eq!(no_svg.format(), crate::IconFormat::Png);
    assert!(no_svg.path().starts_with(data.path()));
}

#[test]
fn the_first_parseable_index_theme_defines_the_theme() {
    let home = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    // A malformed definition in the higher-priority home root.
    let home_theme = home.path().join(".icons").join("t");
    fs::create_dir_all(&home_theme).unwrap();
    fs::write(home_theme.join("index.theme"), "[Other]\nX=1\n").unwrap();
    let data_theme = theme(data.path(), "t", SIMPLE);
    icon(&data_theme, "16", "a.png");
    let themes = IconThemes::load(&base(home.path(), data.path()), "t");
    assert!(
        themes.lookup("a", &request(16, 1)).is_some(),
        "the next root's file defines the theme"
    );
    assert!(
        themes
            .diagnostics()
            .iter()
            .any(|d| d.message.contains("defines no theme")),
        "{:?}",
        themes.diagnostics()
    );
}

#[test]
fn the_unthemed_fallback_comes_last_and_carries_no_theme() {
    let home = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    let dir = theme(data.path(), "t", SIMPLE);
    icon(&dir, "16", "themed.png");
    // A loose file directly in the icons root.
    fs::write(data.path().join("icons").join("loose.png"), "").unwrap();
    fs::write(data.path().join("icons").join("themed.png"), "").unwrap();
    let themes = IconThemes::load(&base(home.path(), data.path()), "t");
    let from_theme = themes.lookup("themed", &request(16, 1)).unwrap();
    assert_eq!(
        from_theme.theme(),
        Some("t"),
        "the theme wins over the root file"
    );
    let loose = themes.lookup("loose", &request(16, 1)).unwrap();
    assert_eq!(loose.theme(), None);
    assert!(loose.sizing().is_none());
    assert!(loose.path().ends_with("icons/loose.png"));
    assert!(themes.lookup("absent", &request(16, 1)).is_none());
}

#[test]
fn missing_themes_are_diagnosed_and_limits_hold() {
    let home = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    let dir = theme(
        data.path(),
        "t",
        "[Icon Theme]\nName=T\nInherits=ghost\nDirectories=16\n[16]\nSize=16\nType=Fixed\n",
    );
    icon(&dir, "16", "a.png");
    icon(&dir, "16", "b.png");
    let limits = Limits {
        max_icons: 1,
        ..Limits::default()
    };
    let themes = IconThemes::load_with(
        &base(home.path(), data.path()),
        "t",
        &limits,
        LoadOptions::default(),
    );
    assert!(
        themes
            .diagnostics()
            .iter()
            .any(|d| d.message.contains("\"ghost\" is not installed"))
    );
    assert!(
        themes
            .diagnostics()
            .iter()
            .any(|d| d.message.contains("more than 1 icons"))
    );
}

mod cache {
    use std::os::unix::fs::PermissionsExt;

    use super::*;
    use crate::IconFormat;
    use crate::cache::builder;

    /// GTK's flag bits, for the builder.
    const F_XPM: u16 = 1;
    const F_SVG: u16 = 2;
    const F_PNG: u16 = 4;

    fn write_cache(theme_dir: &Path, bytes: &[u8]) {
        fs::write(theme_dir.join("icon-theme.cache"), bytes).unwrap();
    }

    /// Moves a file's timestamps `seconds` into the past: tmpfs rounds
    /// mtimes to the timer tick, so "written later" alone cannot prove
    /// staleness in a fast test.
    fn backdate(path: &Path, seconds: u32) {
        let stamp = rustix::fs::stat(path).unwrap();
        let then = rustix::fs::Timespec {
            tv_sec: stamp.st_mtime - i64::from(seconds),
            tv_nsec: 0,
        };
        rustix::fs::utimensat(
            rustix::fs::CWD,
            path,
            &rustix::fs::Timestamps {
                last_access: then,
                last_modification: then,
            },
            rustix::fs::AtFlags::empty(),
        )
        .unwrap();
    }

    const TWO_DIRS: &str = "[Icon Theme]\nName=T\nDirectories=16,32\n\
        [16]\nSize=16\nType=Fixed\n[32]\nSize=32\nType=Fixed\n";

    #[test]
    fn a_valid_cache_is_used_proven_by_an_unscannable_directory() {
        let home = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let dir = theme(data.path(), "t", TWO_DIRS);
        icon(&dir, "16", "a.png");
        // Make the subdirectory unreadable; only the cache can know its
        // contents now.
        fs::set_permissions(dir.join("16"), fs::Permissions::from_mode(0o000)).unwrap();
        if fs::read_dir(dir.join("16")).is_ok() {
            // root reads anything; the proof cannot run.
            fs::set_permissions(dir.join("16"), fs::Permissions::from_mode(0o755)).unwrap();
            return;
        }
        write_cache(&dir, &builder::cache(&["16", "32"], &[("a", 0, F_PNG)]));
        let themes = IconThemes::load(&base(home.path(), data.path()), "t");
        let hit = themes
            .lookup("a", &request(16, 1))
            .expect("found through the cache, not readdir");
        assert!(hit.path().ends_with("t/16/a.png"));
        fs::set_permissions(dir.join("16"), fs::Permissions::from_mode(0o755)).unwrap();
        // Scan-only loading honors the policy and cannot see it… it can
        // now that the directory is readable again; prove the knob by
        // the diagnostics instead: a corrupt cache is not even looked at.
        write_cache(&dir, b"garbage");
        let scanned = IconThemes::load_with(
            &base(home.path(), data.path()),
            "t",
            &Limits::default(),
            LoadOptions { caches: false },
        );
        assert!(scanned.lookup("a", &request(16, 1)).is_some());
        assert!(
            !scanned
                .diagnostics()
                .iter()
                .any(|d| d.message.contains("icon cache")),
            "scan-only loading never judges caches: {:?}",
            scanned.diagnostics()
        );
    }

    #[test]
    fn stale_corrupt_and_hostile_caches_fall_back_with_diagnostics() {
        let home = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let dir = theme(data.path(), "t", TWO_DIRS);
        icon(&dir, "16", "a.png");
        let load = |expect_diag: &str| {
            let themes = IconThemes::load(&base(home.path(), data.path()), "t");
            assert!(
                themes.lookup("a", &request(16, 1)).is_some(),
                "the scan fallback still finds the icon"
            );
            assert!(
                themes
                    .diagnostics()
                    .iter()
                    .any(|d| d.message.contains(expect_diag)),
                "{expect_diag}: {:?}",
                themes.diagnostics()
            );
        };
        // Stale: the subdirectory changes after the cache was written
        // (the cache is backdated — a same-tick mtime is the documented
        // undetectable case, as it is for GTK's own rule).
        write_cache(&dir, &builder::cache(&["16", "32"], &[("a", 0, F_PNG)]));
        backdate(&dir.join("icon-theme.cache"), 10);
        icon(&dir, "16", "later.png");
        load("older than");
        // Truncated.
        write_cache(&dir, &[0, 1, 0, 0, 0, 0]);
        load("truncated");
        // Wrong version.
        write_cache(&dir, &[0, 2, 0, 0, 0, 0, 0, 12, 0, 0, 0, 12, 0, 0, 0, 0]);
        load("unsupported cache version");
        // A directory index past the list.
        write_cache(&dir, &builder::cache(&["16"], &[("a", 9, F_PNG)]));
        load("past the list");
        // A cyclic bucket chain: a node pointing at itself.
        let mut cyclic = builder::cache(&["16"], &[("a", 0, F_PNG)]);
        let len = cyclic.len();
        let node_at = u32::try_from(len - 12).unwrap();
        cyclic[len - 12..len - 8].copy_from_slice(&node_at.to_be_bytes());
        write_cache(&dir, &cyclic);
        load("a cycle in the hash chains");
    }

    #[test]
    fn undeclared_directories_in_a_cache_are_dropped() {
        let home = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let dir = theme(data.path(), "t", TWO_DIRS);
        write_cache(
            &dir,
            &builder::cache(&["16", "evil"], &[("a", 0, F_PNG), ("smuggled", 1, F_PNG)]),
        );
        let themes = IconThemes::load(&base(home.path(), data.path()), "t");
        assert!(themes.lookup("a", &request(16, 1)).is_some());
        assert!(
            themes.lookup("smuggled", &request(16, 1)).is_none(),
            "a cache cannot add directories the theme does not declare"
        );
    }

    #[test]
    fn cached_and_scanned_loads_resolve_identically() {
        let home = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        // Two roots, duplicate names, mixed formats.
        let data_dir = theme(data.path(), "t", TWO_DIRS);
        let home_dir = home.path().join(".icons").join("t");
        icon(&data_dir, "16", "a.png");
        icon(&data_dir, "32", "a.svg");
        icon(&data_dir, "32", "b.xpm");
        icon(&home_dir, "32", "a.xpm");
        write_cache(
            &data_dir,
            &builder::cache(
                &["16", "32"],
                &[("a", 0, F_PNG), ("a", 1, F_SVG), ("b", 1, F_XPM)],
            ),
        );
        write_cache(&home_dir, &builder::cache(&["32"], &[("a", 0, F_XPM)]));
        let cached = IconThemes::load(&base(home.path(), data.path()), "t");
        let scanned = IconThemes::load_with(
            &base(home.path(), data.path()),
            "t",
            &Limits::default(),
            LoadOptions { caches: false },
        );
        for (name, size) in [("a", 16), ("a", 32), ("b", 32), ("b", 16), ("missing", 16)] {
            let via_cache = cached.lookup(name, &request(size, 1));
            let via_scan = scanned.lookup(name, &request(size, 1));
            assert_eq!(via_cache, via_scan, "{name} at {size}");
        }
    }

    #[test]
    fn the_real_gtk_fixture_reads_identically_to_a_scan() {
        let home = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let dir = theme(
            data.path(),
            "fixture",
            "[Icon Theme]\nName=Fixture\nDirectories=16x16/apps,scalable/apps\n\
             [16x16/apps]\nSize=16\nType=Fixed\n\
             [scalable/apps]\nSize=128\nType=Scalable\nMinSize=8\nMaxSize=256\n",
        );
        icon(&dir, "16x16/apps", "alpha.png");
        icon(&dir, "16x16/apps", "beta.png");
        icon(&dir, "16x16/apps", "beta.xpm");
        icon(&dir, "scalable/apps", "alpha.svg");
        let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/gtk-icon-theme.cache");
        fs::copy(fixture, dir.join("icon-theme.cache")).unwrap();
        let cached = IconThemes::load(&base(home.path(), data.path()), "fixture");
        assert!(
            !cached
                .diagnostics()
                .iter()
                .any(|d| d.message.contains("icon cache")),
            "the real cache is sound and fresh: {:?}",
            cached.diagnostics()
        );
        let scanned = IconThemes::load_with(
            &base(home.path(), data.path()),
            "fixture",
            &Limits::default(),
            LoadOptions { caches: false },
        );
        for (name, size) in [("alpha", 16), ("alpha", 128), ("beta", 16), ("beta", 64)] {
            assert_eq!(
                cached.lookup(name, &request(size, 1)),
                scanned.lookup(name, &request(size, 1)),
                "{name} at {size}"
            );
        }
        let svg = cached.lookup("alpha", &request(128, 1)).unwrap();
        assert_eq!(svg.format(), IconFormat::Svg);
    }
}

mod refreshing {
    use super::*;

    #[test]
    fn refresh_sees_every_kind_of_change() {
        let home = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let dir = theme(data.path(), "t", SIMPLE);
        icon(&dir, "16", "a.png");
        let loaded = IconThemes::load(&base(home.path(), data.path()), "t");
        assert!(loaded.refresh().is_none(), "nothing changed");
        // An icon added to a subdirectory (the top level is untouched).
        icon(&dir, "16", "b.png");
        let step1 = loaded.refresh().expect("the subdirectory changed");
        assert!(step1.lookup("b", &request(16, 1)).is_some());
        assert!(step1.refresh().is_none());
        // An index.theme edited in place.
        fs::write(
            dir.join("index.theme"),
            SIMPLE.replace("Directories=16,32,16@2", "Directories=16"),
        )
        .unwrap();
        let step2 = step1.refresh().expect("the definition changed");
        assert!(step2.refresh().is_none());
        // A higher-priority definition appearing in the home root.
        let home_theme = home.path().join(".icons").join("t");
        fs::create_dir_all(&home_theme).unwrap();
        fs::write(
            home_theme.join("index.theme"),
            "[Icon Theme]\nName=Override\nDirectories=48\n[48]\nSize=48\nType=Fixed\n",
        )
        .unwrap();
        let step3 = step2.refresh().expect("a would-be location appeared");
        icon(&home_theme, "48", "big.png");
        let step4 = step3.refresh().expect("the new theme's directory grew");
        assert!(step4.lookup("big", &request(48, 1)).is_some());
        assert_eq!(
            step4.lookup("big", &request(48, 1)).unwrap().theme(),
            Some("t")
        );
    }

    #[test]
    fn an_appearing_inherited_theme_is_noticed() {
        let home = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let dir = theme(
            data.path(),
            "t",
            "[Icon Theme]\nName=T\nInherits=ghost\nDirectories=16\n[16]\nSize=16\nType=Fixed\n",
        );
        icon(&dir, "16", "a.png");
        let loaded = IconThemes::load(&base(home.path(), data.path()), "t");
        assert!(loaded.lookup("ghostly", &request(16, 1)).is_none());
        let ghost = theme(
            data.path(),
            "ghost",
            "[Icon Theme]\nName=G\nDirectories=16\n[16]\nSize=16\nType=Fixed\n",
        );
        icon(&ghost, "16", "ghostly.png");
        let appeared = loaded.refresh().expect("the absent parent appeared");
        assert_eq!(
            appeared.lookup("ghostly", &request(16, 1)).unwrap().theme(),
            Some("ghost")
        );
    }

    #[test]
    fn a_rebuild_scans_what_changed_instead_of_its_cache() {
        use crate::cache::builder;
        let home = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let dir = theme(data.path(), "t", SIMPLE);
        icon(&dir, "16", "a.png");
        fs::write(
            dir.join("icon-theme.cache"),
            builder::cache(&["16"], &[("a", 0, 4)]),
        )
        .unwrap();
        let loaded = IconThemes::load(&base(home.path(), data.path()), "t");
        assert!(loaded.lookup("a", &request(16, 1)).is_some());
        // A new icon lands; the rebuild must not serve the stale cache
        // even if mtime granularity would let it pass its own check.
        icon(&dir, "16", "b.png");
        let rebuilt = loaded.refresh().expect("the directory changed");
        assert!(
            rebuilt.lookup("b", &request(16, 1)).is_some(),
            "{:?}",
            rebuilt.diagnostics()
        );
    }
}

mod listing_and_linearity {
    use std::fmt::Write as _;

    use super::*;
    use crate::{Locales, themes};

    #[test]
    fn installed_themes_list_with_their_picker_fields() {
        let home = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        theme(
            data.path(),
            "shown",
            "[Icon Theme]\nName=Shown\nName[de]=Gezeigt\nComment=Nice\nDirectories=16\n\
             [16]\nSize=16\nType=Fixed\n",
        );
        theme(
            data.path(),
            "internal",
            "[Icon Theme]\nName=Internal\nHidden=true\nExample=folder\nDirectories=16\n\
             [16]\nSize=16\nType=Fixed\n",
        );
        // A directory without an index.theme is not a theme.
        fs::create_dir_all(data.path().join("icons").join("junk")).unwrap();
        // The home root shadows the data root's definition of one id
        // (written directly: $HOME/.icons is itself the root).
        let mine = home.path().join(".icons").join("shown");
        fs::create_dir_all(&mine).unwrap();
        fs::write(
            mine.join("index.theme"),
            "[Icon Theme]\nName=Mine\nDirectories=16\n[16]\nSize=16\nType=Fixed\n",
        )
        .unwrap();
        let listed = themes(&base(home.path(), data.path()), &Locales::new(["de"]));
        let ids: Vec<_> = listed.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(ids, ["shown", "internal"]);
        assert_eq!(listed[0].name.as_deref(), Some("Mine"), "first root wins");
        assert!(listed[1].hidden);
        assert_eq!(listed[1].example.as_deref(), Some("folder"));
        // A loaded snapshot lists its own theme too, localized.
        let loaded = IconThemes::load(&base(home.path(), data.path()), "internal");
        let own = loaded.listing(&Locales::new(Vec::<String>::new())).unwrap();
        assert_eq!(own.name.as_deref(), Some("Internal"));
        assert!(own.hidden);
        let absent = IconThemes::load(&base(home.path(), data.path()), "nowhere");
        assert!(
            absent
                .listing(&Locales::new(Vec::<String>::new()))
                .is_none()
        );
    }

    #[test]
    fn loading_is_linear_in_icon_files() {
        let home = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let dir = theme(data.path(), "t", SIMPLE);
        crate::work::assert_linear(
            300,
            |n| {
                for at in 0..n {
                    icon(&dir, "16", &format!("i{at}.png"));
                }
                let base = base(home.path(), data.path());
                let (loaded, steps) = crate::work::steps(|| IconThemes::load(&base, "t"));
                assert!(loaded.lookup("i0", &request(16, 1)).is_some());
                (steps, n * 8)
            },
            // Per file: the name examined, hashed and copied, the
            // candidate bookkeeping; per theme a bounded number of
            // directory and stamp steps.
            |n, bytes| 2 * (12 * (n + 64) + 4 * bytes),
        );
    }

    #[test]
    fn lookups_touch_only_the_names_candidates() {
        let home = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let dir = theme(data.path(), "t", SIMPLE);
        crate::work::assert_linear(
            300,
            |n| {
                let mut name_bytes = 0;
                for at in 0..n {
                    icon(&dir, "16", &format!("i{at}.png"));
                    name_bytes += format!("i{at}").len();
                }
                let loaded = IconThemes::load(&base(home.path(), data.path()), "t");
                let (hits, steps) = crate::work::steps(|| {
                    (0..n)
                        .filter(|at| loaded.lookup(&format!("i{at}"), &request(16, 1)).is_some())
                        .count()
                });
                assert_eq!(hits, n);
                (steps, name_bytes)
            },
            // Per lookup: a few steps and one candidate; per name byte:
            // formatted, hashed and compared, and copied into the result.
            |n, bytes| 2 * (8 * (n + 8) + 5 * bytes),
        );
    }

    #[test]
    fn cache_parsing_is_linear_in_entries() {
        use crate::cache::builder;
        let home = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let dir = theme(data.path(), "t", SIMPLE);
        fs::create_dir_all(dir.join("16")).unwrap();
        crate::work::assert_linear(
            300,
            |n| {
                let mut names = String::new();
                let icons: Vec<(String, u16, u16)> = (0..n)
                    .map(|at| {
                        let name = format!("i{at}");
                        let _ = write!(names, "{name}");
                        (name, 0, 4)
                    })
                    .collect();
                let refs: Vec<(&str, u16, u16)> = icons
                    .iter()
                    .map(|(name, dir, flags)| (name.as_str(), *dir, *flags))
                    .collect();
                fs::write(dir.join("icon-theme.cache"), builder::cache(&["16"], &refs)).unwrap();
                let base = base(home.path(), data.path());
                let (loaded, steps) = crate::work::steps(|| IconThemes::load(&base, "t"));
                assert!(loaded.lookup("i0", &request(16, 1)).is_some());
                (steps, names.len())
            },
            // Per entry: the node walked, the name read and copied twice,
            // the candidate bookkeeping; a bounded constant besides.
            |n, bytes| 2 * (14 * (n + 64) + 6 * bytes),
        );
    }
}

mod sixth_round_regressions {
    use super::*;
    use crate::cache::builder;
    use crate::{Locales, themes};

    #[test]
    fn shared_image_lists_and_cycles_are_bounded_exactly() {
        let home = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let dir = theme(data.path(), "t", SIMPLE);
        fs::create_dir_all(dir.join("16")).unwrap();
        icon(&dir, "16", "a.png");
        // Two nodes sharing one three-image list: the aggregate record
        // budget counts both expansions (six records against a limit of
        // three), where a per-node check would pass each one.
        let mut shared = builder::cache_multi(
            &["16"],
            &[
                ("a", vec![(0, 4), (0, 2), (0, 1)]),
                ("b", vec![(0, 4), (0, 2), (0, 1)]),
            ],
        );
        // Point b's image list at a's. The bucket (at hash_offset + 4)
        // names a's node; each node is [next, name, images].
        let read_at = |bytes: &[u8], at: usize| {
            u32::from_be_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]]) as usize
        };
        let hash_offset = read_at(&shared, 4);
        let a_node = read_at(&shared, hash_offset + 4);
        let a_images = shared[a_node + 8..a_node + 12].to_vec();
        let b_node = shared.len() - 12;
        shared[b_node + 8..b_node + 12].copy_from_slice(&a_images);
        fs::write(dir.join("icon-theme.cache"), &shared).unwrap();
        let tight = Limits {
            max_cache_entries: 3,
            ..Limits::default()
        };
        let themes = IconThemes::load_with(
            &base(home.path(), data.path()),
            "t",
            &tight,
            LoadOptions::default(),
        );
        assert!(
            themes
                .diagnostics()
                .iter()
                .any(|d| d.message.contains("more image records than the limit")),
            "{:?}",
            themes.diagnostics()
        );
        // A cycle is refused even with no limits at all: detection is
        // exact, never budget-dependent.
        let mut cyclic = builder::cache(&["16"], &[("a", 0, 4)]);
        let len = cyclic.len();
        let node_at = u32::try_from(len - 12).unwrap();
        cyclic[len - 12..len - 8].copy_from_slice(&node_at.to_be_bytes());
        fs::write(dir.join("icon-theme.cache"), &cyclic).unwrap();
        let unlimited = IconThemes::load_with(
            &base(home.path(), data.path()),
            "t",
            &Limits::unlimited(),
            LoadOptions::default(),
        );
        assert!(
            unlimited
                .diagnostics()
                .iter()
                .any(|d| d.message.contains("a cycle in the hash chains")),
            "{:?}",
            unlimited.diagnostics()
        );
        assert!(unlimited.lookup("a", &request(16, 1)).is_some(), "scanned");
    }

    #[test]
    fn hostile_cache_offsets_are_refusals_not_overflow() {
        let home = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let dir = theme(data.path(), "t", SIMPLE);
        icon(&dir, "16", "a.png");
        // A directory-list offset of 0xFFFF_FFFE: on 32-bit the range
        // construction itself would overflow without checked sums.
        let mut hostile = vec![0u8, 1, 0, 0];
        hostile.extend_from_slice(&12u32.to_be_bytes());
        hostile.extend_from_slice(&0xFFFF_FFFEu32.to_be_bytes());
        hostile.extend_from_slice(&0u32.to_be_bytes());
        fs::write(dir.join("icon-theme.cache"), &hostile).unwrap();
        let themes = IconThemes::load(&base(home.path(), data.path()), "t");
        assert!(
            themes
                .diagnostics()
                .iter()
                .any(|d| d.message.contains("icon cache is not used")),
            "{:?}",
            themes.diagnostics()
        );
        assert!(themes.lookup("a", &request(16, 1)).is_some());
    }

    #[test]
    fn hicolor_is_the_final_fallback_even_when_inherited_first() {
        let home = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let child = theme(
            data.path(),
            "child",
            "[Icon Theme]\nName=C\nInherits=hicolor,parent\nDirectories=16\n\
             [16]\nSize=16\nType=Fixed\n",
        );
        let parent = theme(
            data.path(),
            "parent",
            "[Icon Theme]\nName=P\nDirectories=16\n[16]\nSize=16\nType=Fixed\n",
        );
        let hicolor = theme(
            data.path(),
            "hicolor",
            "[Icon Theme]\nName=H\nDirectories=16\n[16]\nSize=16\nType=Fixed\n",
        );
        icon(&parent, "16", "x.png");
        icon(&hicolor, "16", "x.png");
        let themes = IconThemes::load(&base(home.path(), data.path()), "child");
        assert_eq!(
            themes.chain().collect::<Vec<_>>(),
            ["child", "parent", "hicolor"],
            "hicolor moves to the end"
        );
        let x = themes.lookup("x", &request(16, 1)).unwrap();
        assert_eq!(x.theme(), Some("parent"));
        // Requesting hicolor itself keeps the natural behavior.
        let direct = IconThemes::load(&base(home.path(), data.path()), "hicolor");
        assert_eq!(direct.chain().collect::<Vec<_>>(), ["hicolor"]);
        let _ = child;
    }

    #[test]
    fn touching_the_top_level_theme_directory_is_a_change() {
        let home = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let dir = theme(data.path(), "t", SIMPLE);
        icon(&dir, "16", "a.png");
        let loaded = IconThemes::load(&base(home.path(), data.path()), "t");
        assert!(loaded.refresh().is_none());
        // The spec's notification: installers touch the theme directory.
        let bump = rustix::fs::Timestamps {
            last_access: rustix::fs::Timespec {
                tv_sec: i64::MAX / 4,
                tv_nsec: 0,
            },
            last_modification: rustix::fs::Timespec {
                tv_sec: i64::MAX / 4,
                tv_nsec: 0,
            },
        };
        rustix::fs::utimensat(rustix::fs::CWD, &dir, &bump, rustix::fs::AtFlags::empty()).unwrap();
        assert!(
            loaded.refresh().is_some(),
            "the top-level touch is the standard signal"
        );
    }

    #[test]
    fn only_regular_files_are_icons() {
        let home = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let dir = theme(data.path(), "t", SIMPLE);
        // A directory named like an icon, in the theme and the fallback.
        fs::create_dir_all(dir.join("16").join("directory.png")).unwrap();
        fs::create_dir_all(data.path().join("icons").join("loosedir.png")).unwrap();
        // A dangling symlink, and a link resolving to a real file.
        icon(&dir, "16", "real.png");
        std::os::unix::fs::symlink("real.png", dir.join("16").join("linked.png")).unwrap();
        std::os::unix::fs::symlink("gone.png", dir.join("16").join("dangling.png")).unwrap();
        let themes = IconThemes::load(&base(home.path(), data.path()), "t");
        assert!(themes.lookup("directory", &request(16, 1)).is_none());
        assert!(themes.lookup("loosedir", &request(16, 1)).is_none());
        assert!(themes.lookup("dangling", &request(16, 1)).is_none());
        assert!(themes.lookup("linked", &request(16, 1)).is_some());
    }

    #[test]
    fn listing_follows_symlinks_and_skips_unusable_definitions() {
        let home = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let real = theme(data.path(), "real-theme", SIMPLE);
        std::os::unix::fs::symlink(&real, data.path().join("icons").join("linked")).unwrap();
        // An unusable higher-priority definition must not hide the
        // usable lower-priority one — loading would use the latter.
        let broken = home.path().join(".icons").join("real-theme");
        fs::create_dir_all(&broken).unwrap();
        fs::write(broken.join("index.theme"), "[Other]\nX=1\n").unwrap();
        let listed = themes(
            &base(home.path(), data.path()),
            &Locales::new(Vec::<String>::new()),
        );
        let ids: Vec<_> = listed.iter().map(|t| t.id.as_str()).collect();
        assert!(ids.contains(&"linked"), "{ids:?}");
        assert!(ids.contains(&"real-theme"), "{ids:?}");
    }
}

mod seventh_round_regressions {
    use std::fmt::Write as _;

    use super::*;
    use crate::cache::builder;

    #[test]
    fn a_tiny_cache_cannot_request_a_huge_allocation() {
        let home = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let dir = theme(data.path(), "t", SIMPLE);
        icon(&dir, "16", "a.png");
        // Sixteen bytes declaring u32::MAX directories: the table must be
        // rejected before anything is sized by the count — even with
        // every configurable limit off.
        let mut tiny = vec![0u8, 1, 0, 0];
        tiny.extend_from_slice(&12u32.to_be_bytes()); // hash at 12
        tiny.extend_from_slice(&12u32.to_be_bytes()); // dirs at 12 too
        tiny.extend_from_slice(&u32::MAX.to_be_bytes()); // the count
        fs::write(dir.join("icon-theme.cache"), &tiny).unwrap();
        let themes = IconThemes::load_with(
            &base(home.path(), data.path()),
            "t",
            &Limits::unlimited(),
            LoadOptions::default(),
        );
        assert!(
            themes
                .diagnostics()
                .iter()
                .any(|d| d.message.contains("table past the file")),
            "{:?}",
            themes.diagnostics()
        );
        assert!(themes.lookup("a", &request(16, 1)).is_some(), "scanned");
    }

    #[test]
    fn the_chain_budget_counts_examined_ids_with_a_hicolor_allowance() {
        let home = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let mut inherits = String::new();
        for at in 0..100 {
            let _ = write!(inherits, "ghost-{at},");
        }
        theme(
            data.path(),
            "child",
            &format!(
                "[Icon Theme]\nName=C\nInherits={inherits}\nDirectories=16\n\
                 [16]\nSize=16\nType=Fixed\n"
            ),
        );
        let hicolor = theme(
            data.path(),
            "hicolor",
            "[Icon Theme]\nName=H\nDirectories=16\n[16]\nSize=16\nType=Fixed\n",
        );
        icon(&hicolor, "16", "x.png");
        let limits = Limits {
            max_chain: 2,
            ..Limits::default()
        };
        let themes = IconThemes::load_with(
            &base(home.path(), data.path()),
            "child",
            &limits,
            LoadOptions::default(),
        );
        let missing = themes
            .diagnostics()
            .iter()
            .filter(|d| d.message.contains("is not installed"))
            .count();
        assert!(
            missing <= limits.max_chain,
            "missing parents consume the examined budget: {missing}"
        );
        assert!(
            themes
                .diagnostics()
                .iter()
                .any(|d| d.message.contains("chain is cut at 2")),
        );
        // The mandatory fallback has its reserved allowance.
        assert!(themes.chain().any(|id| id == "hicolor"));
        assert!(themes.lookup("x", &request(16, 1)).is_some());
    }

    #[test]
    fn path_shaped_declarations_and_cache_names_are_rejected() {
        let home = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let dir = theme(
            data.path(),
            "t",
            "[Icon Theme]\nName=T\nDirectories=16,..,/abs,a/./b\n\
             [16]\nSize=16\nType=Fixed\n[..]\nSize=16\nType=Fixed\n\
             [/abs]\nSize=16\nType=Fixed\n[a/./b]\nSize=16\nType=Fixed\n",
        );
        // An icon outside the theme that `..` would reach.
        fs::write(data.path().join("icons").join("outside.png"), "").unwrap();
        icon(&dir, "16", "a.png");
        let themes = IconThemes::load(&base(home.path(), data.path()), "t");
        let hit = themes.lookup("outside", &request(16, 1));
        assert!(
            hit.is_none_or(|icon| icon.theme().is_none()),
            "a declared `..` must not serve the parent directory's files as themed"
        );
        let skipped = themes
            .diagnostics()
            .iter()
            .filter(|d| d.message.contains("not a relative subdirectory"))
            .count();
        assert_eq!(skipped, 3, "{:?}", themes.diagnostics());
        // A traversal-shaped icon name poisons the whole cache.
        fs::write(
            dir.join("icon-theme.cache"),
            builder::cache(&["16"], &[("../../../outside", 0, 4)]),
        )
        .unwrap();
        let themes = IconThemes::load(&base(home.path(), data.path()), "t");
        assert!(
            themes
                .diagnostics()
                .iter()
                .any(|d| d.message.contains("not a filename")),
            "{:?}",
            themes.diagnostics()
        );
        assert!(themes.lookup("../../../outside", &request(16, 1)).is_none());
        assert!(themes.lookup("a", &request(16, 1)).is_some(), "scanned");
        // A path-shaped theme id never reaches the filesystem.
        let odd = IconThemes::load(&base(home.path(), data.path()), "../t");
        assert!(
            odd.diagnostics()
                .iter()
                .any(|d| d.message.contains("is not a theme name")),
            "{:?}",
            odd.diagnostics()
        );
    }

    #[test]
    fn repeated_names_across_many_directories_load_linearly() {
        let home = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        crate::work::assert_linear(
            60,
            |n| {
                // One icon name in n declared directories: the merge must
                // not scan the name's earlier candidates per insertion.
                let mut dirs = String::new();
                let mut sections = String::new();
                for at in 0..n {
                    let _ = write!(dirs, "d{at},");
                    let _ = write!(sections, "[d{at}]\nSize={}\nType=Fixed\n", at + 1);
                }
                let dir = theme(
                    data.path(),
                    "t",
                    &format!("[Icon Theme]\nName=T\nDirectories={dirs}\n{sections}"),
                );
                let mut bytes = 0;
                for at in 0..n {
                    icon(&dir, &format!("d{at}"), "same.png");
                    icon(&dir, &format!("d{at}"), "same.svg");
                    bytes += 2 * "same.png".len() + format!("d{at}").len();
                }
                let base = base(home.path(), data.path());
                let (loaded, steps) = crate::work::steps(|| IconThemes::load(&base, "t"));
                assert!(
                    loaded
                        .lookup("same", &request(1, 1))
                        .is_some_and(|icon| icon.path().to_string_lossy().contains("/d0/"))
                );
                (steps, bytes)
            },
            // Per directory: the declaration parsed, the stamps, the two
            // entries merged in one comparison each; per byte a few
            // passes.
            |n, bytes| 2 * (40 * (n + 32) + 6 * bytes),
        );
    }
}

mod eighth_round_regressions {
    use super::*;
    use crate::cache::builder;

    /// GTK's flag bits, for the builder.
    const F_SVG: u16 = 2;
    const F_PNG: u16 = 4;

    /// Moves a file's timestamps `seconds` into the future, so the file
    /// passes mtime freshness checks whatever else the test touches.
    fn postdate(path: &Path, seconds: u32) {
        let stamp = rustix::fs::stat(path).unwrap();
        let then = rustix::fs::Timespec {
            tv_sec: stamp.st_mtime + i64::from(seconds),
            tv_nsec: 0,
        };
        rustix::fs::utimensat(
            rustix::fs::CWD,
            path,
            &rustix::fs::Timestamps {
                last_access: then,
                last_modification: then,
            },
            rustix::fs::AtFlags::empty(),
        )
        .unwrap();
    }

    const ONE_DIR: &str = "[Icon Theme]\nName=T\nDirectories=48\n[48]\nSize=48\nType=Fixed\n";

    #[test]
    fn a_refused_cache_stays_refused_until_the_cache_itself_changes() {
        let home = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let dir = theme(data.path(), "t", ONE_DIR);
        icon(&dir, "48", "a.png");
        let cache_path = dir.join("icon-theme.cache");
        fs::write(&cache_path, builder::cache(&["48"], &[("a", 0, F_PNG)])).unwrap();
        // Pushed ahead, the cache passes its own freshness check whatever
        // lands in the directories below.
        postdate(&cache_path, 3600);
        let loaded = IconThemes::load(&base(home.path(), data.path()), "t");
        assert!(loaded.lookup("a", &request(48, 1)).is_some());
        // A new icon the fresh-looking cache does not list: the changed
        // subdirectory forces a scan.
        icon(&dir, "48", "b.png");
        let scanned = loaded.refresh().expect("the subdirectory changed");
        assert!(scanned.lookup("b", &request(48, 1)).is_some());
        // An unrelated change triggers another refresh. The cache still
        // passes its own check — and must stay refused, or b vanishes.
        fs::write(data.path().join("icons").join("unrelated.png"), "").unwrap();
        let again = scanned.refresh().expect("the fallback root changed");
        assert!(
            again.lookup("b", &request(48, 1)).is_some(),
            "a refused cache must not come back on an unrelated refresh: {:?}",
            again.diagnostics()
        );
        // A regenerated cache lifts the refusal: its own change still gets
        // one scanned load, then it serves again — proven by a listed
        // icon with no file behind it.
        fs::write(
            &cache_path,
            builder::cache(
                &["48"],
                &[("a", 0, F_PNG), ("b", 0, F_PNG), ("ghost", 0, F_PNG)],
            ),
        )
        .unwrap();
        postdate(&cache_path, 3600);
        let once_scanned = again.refresh().expect("the cache file changed");
        assert!(
            once_scanned.lookup("ghost", &request(48, 1)).is_none(),
            "a changed cache file gets one scanned load first"
        );
        fs::create_dir_all(home.path().join(".icons")).unwrap();
        let served = once_scanned.refresh().expect("a root appeared");
        assert!(
            served.lookup("ghost", &request(48, 1)).is_some(),
            "the regenerated cache serves again: {:?}",
            served.diagnostics()
        );
    }

    #[test]
    fn the_icon_budget_counts_files_on_every_path_and_reports_the_cut() {
        let home = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let dir = theme(data.path(), "t", ONE_DIR);
        icon(&dir, "48", "a.png");
        icon(&dir, "48", "a.svg");
        icon(&dir, "48", "b.png");
        fs::write(
            dir.join("icon-theme.cache"),
            builder::cache_multi(
                &["48"],
                &[("a", vec![(0, F_PNG | F_SVG)]), ("b", vec![(0, F_PNG)])],
            ),
        )
        .unwrap();
        let limits = Limits {
            max_icons: 1,
            ..Limits::default()
        };
        let cut = |themes: &IconThemes| {
            themes
                .diagnostics()
                .iter()
                .any(|d| d.message.contains("icons; the rest are skipped"))
        };
        let cached = IconThemes::load_with(
            &base(home.path(), data.path()),
            "t",
            &limits,
            LoadOptions::default(),
        );
        // One file's worth fits: a.png is served, a's svg and b are cut —
        // the same accounting a scan uses — and the cut is reported.
        assert!(cached.lookup("a", &request(48, 1)).is_some());
        let svg_only = IconRequest {
            size: 48,
            scale: 1,
            formats: Formats {
                png: false,
                svg: true,
                xpm: false,
            },
        };
        assert!(cached.lookup("a", &svg_only).is_none(), "a's svg was cut");
        assert!(cached.lookup("b", &request(48, 1)).is_none());
        assert!(cut(&cached), "{:?}", cached.diagnostics());
        // Scanning under the same budget reports the cut too.
        let scanned = IconThemes::load_with(
            &base(home.path(), data.path()),
            "t",
            &limits,
            LoadOptions { caches: false },
        );
        assert!(cut(&scanned), "{:?}", scanned.diagnostics());
        // So does the unthemed fallback.
        let fallback_home = tempfile::tempdir().unwrap();
        let fallback_data = tempfile::tempdir().unwrap();
        let root = fallback_data.path().join("icons");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("x.png"), "").unwrap();
        fs::write(root.join("y.png"), "").unwrap();
        let unthemed = IconThemes::load_with(
            &base(fallback_home.path(), fallback_data.path()),
            "t",
            &limits,
            LoadOptions::default(),
        );
        assert!(cut(&unthemed), "{:?}", unthemed.diagnostics());
    }
}
