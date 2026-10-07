//! `MimeApps` against `GLib`'s `gio mime` (from the dev shell), on
//! scenarios built in temporary folders.
//!
//! For each scenario and type both must give the same default application
//! and, when there is one, the same applications ("Registered") and the
//! same applications for the type itself ("Recommended"). Intentional
//! differences follow either the specs or a project decision where `GLib`
//! does not. `gio-differences.txt` lists each scenario and type with our
//! expected answer and a reason. Every listed case must still differ, and
//! our answer must match the listed one.
//!
//! `GLib` takes a folder's associations from its `mimeinfo.cache` only, so
//! `update-desktop-database` writes one into every `applications` folder
//! first; this crate never reads it. `GLib` also skips an application whose
//! `Exec` program is not on `PATH`: the scenarios run `true`, from the dev
//! shell's `PATH`. The nixpkgs build of `GLib` also reads the Nix profiles'
//! `share/applications` (their entries, `mimeinfo.cache`, `mimeapps.list`
//! and `defaults.list`), so the scenarios use made-up types and only
//! applications whose ID starts with `xkt-` (the scenarios' own) are
//! compared.

mod common;

use std::collections::BTreeMap;
use std::process::Command;

use common::{Fixture, update_desktop_database};
use xdg_kit_mime::MimeType;
use xdg_kit_mime_apps::Matched;

/// What one side says about one type.
#[derive(Debug, PartialEq, Eq)]
struct Answer {
    default: Option<String>,
    /// Only when there is a default (`gio mime` lists nothing otherwise).
    registered: Vec<String>,
    recommended: Vec<String>,
}

impl Answer {
    /// One line: `default=… recommended=…,… registered=…,…` (`-` for none).
    fn summary(&self) -> String {
        let list = |ids: &[String]| {
            if ids.is_empty() {
                "-".to_owned()
            } else {
                ids.join(",")
            }
        };
        format!(
            "default={} recommended={} registered={}",
            self.default.as_deref().unwrap_or("-"),
            list(&self.recommended),
            list(&self.registered)
        )
    }
}

fn ours(fixture: &Fixture, mime: &str) -> Answer {
    let apps = fixture.mime_apps();
    let mime = MimeType::new(mime).unwrap();
    let default = apps.default_for(&mime).map(|app| app.id().to_owned());
    let found = apps.apps_for(&mime);
    let ids = |exact_only: bool| -> Vec<String> {
        if default.is_none() {
            return Vec::new();
        }
        found
            .iter()
            .filter(|found| !exact_only || found.matched == Matched::Exact)
            .map(|found| found.app.id().to_owned())
            .collect()
    };
    Answer {
        registered: ids(false),
        recommended: ids(true),
        default,
    }
}

fn gio(fixture: &Fixture, mime: &str) -> Answer {
    let path = std::env::var_os("PATH").unwrap_or_default();
    let output = Command::new(gio_program())
        .arg("mime")
        .arg(mime)
        .env_clear()
        .env("PATH", path)
        .envs(fixture.vars())
        .output()
        .expect("gio could not be run");
    assert!(
        output.status.success(),
        "gio mime {mime} failed: {output:?}"
    );
    let text = String::from_utf8(output.stdout).unwrap();
    let ours = |id: &str| id.starts_with("xkt-");
    let mut default = None;
    let mut registered = Vec::new();
    let mut recommended = Vec::new();
    let mut list: Option<&mut Vec<String>> = None;
    for line in text.lines() {
        if let Some(id) = line.strip_prefix("\t") {
            if let Some(list) = list.as_deref_mut()
                && ours(id)
            {
                list.push(id.to_owned());
            }
        } else if line.starts_with("Default application for") {
            let id = line.rsplit(": ").next().unwrap().to_owned();
            default = ours(&id).then_some(id);
        } else if line.starts_with("Registered applications") {
            list = Some(&mut registered);
        } else if line.starts_with("Recommended applications") {
            list = Some(&mut recommended);
        } else {
            list = None;
        }
    }
    if default.is_none() {
        registered.clear();
        recommended.clear();
    }
    Answer {
        default,
        registered,
        recommended,
    }
}

/// `gio` from `PATH`, with a pointer to the dev shell when it is missing.
fn gio_program() -> std::path::PathBuf {
    let path = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&path)
        .map(|dir| dir.join("gio"))
        .find(|candidate| candidate.is_file())
        .expect("gio is not on PATH: run the tests in the dev shell (nix develop)")
}

/// A scenario: files to write (relative path, text), desktops, and the
/// types to ask about.
struct Scenario {
    name: &'static str,
    desktops: &'static [&'static str],
    apps: &'static [(&'static str, &'static str, &'static str, &'static str)],
    files: &'static [(&'static str, &'static str)],
    types: &'static [&'static str],
}

const S1: &str = "share1/applications";
const S2: &str = "share2/applications";
const LOCAL: &str = "local/applications";

const SCENARIOS: &[Scenario] = &[
    Scenario {
        name: "one-app",
        desktops: &[],
        apps: &[(S1, "xkt-a.desktop", "text/x-xkt-plain;", "")],
        files: &[],
        types: &["text/x-xkt-plain", "image/x-xkt-png"],
    },
    Scenario {
        name: "two-apps-by-id",
        desktops: &[],
        apps: &[
            (S1, "xkt-b.desktop", "text/x-xkt-plain;", ""),
            (S1, "xkt-a.desktop", "text/x-xkt-plain;", ""),
        ],
        files: &[],
        types: &["text/x-xkt-plain"],
    },
    Scenario {
        name: "user-default",
        desktops: &[],
        apps: &[
            (S1, "xkt-a.desktop", "text/x-xkt-plain;", ""),
            (S1, "xkt-b.desktop", "text/x-xkt-plain;", ""),
        ],
        files: &[(
            "config/mimeapps.list",
            "[Default Applications]\ntext/x-xkt-plain=xkt-b.desktop;\n",
        )],
        types: &["text/x-xkt-plain"],
    },
    Scenario {
        name: "default-not-associated",
        desktops: &[],
        apps: &[
            (S1, "xkt-a.desktop", "text/x-xkt-plain;", ""),
            (S1, "xkt-img.desktop", "image/x-xkt-png;", ""),
        ],
        files: &[(
            "config/mimeapps.list",
            "[Default Applications]\ntext/x-xkt-plain=xkt-img.desktop;\n",
        )],
        types: &["text/x-xkt-plain"],
    },
    Scenario {
        name: "default-next-if-missing",
        desktops: &[],
        apps: &[
            (S1, "xkt-a.desktop", "text/x-xkt-plain;", ""),
            (S1, "xkt-b.desktop", "text/x-xkt-plain;", ""),
        ],
        files: &[(
            "config/mimeapps.list",
            "[Default Applications]\ntext/x-xkt-plain=xkt-gone.desktop;xkt-b.desktop;\n",
        )],
        types: &["text/x-xkt-plain"],
    },
    Scenario {
        name: "user-added",
        desktops: &[],
        apps: &[
            (S1, "xkt-a.desktop", "text/x-xkt-plain;", ""),
            (S1, "xkt-viewer.desktop", "image/x-xkt-png;", ""),
        ],
        files: &[(
            "config/mimeapps.list",
            "[Added Associations]\ntext/x-xkt-plain=xkt-viewer.desktop;\n",
        )],
        types: &["text/x-xkt-plain", "image/x-xkt-png"],
    },
    Scenario {
        name: "user-removed",
        desktops: &[],
        apps: &[
            (S1, "xkt-a.desktop", "text/x-xkt-plain;", ""),
            (S1, "xkt-b.desktop", "text/x-xkt-plain;", ""),
        ],
        files: &[(
            "config/mimeapps.list",
            "[Removed Associations]\ntext/x-xkt-plain=xkt-a.desktop;\n",
        )],
        types: &["text/x-xkt-plain"],
    },
    Scenario {
        name: "removal-scoping",
        desktops: &[],
        apps: &[
            (S1, "xkt-a.desktop", "text/x-xkt-plain;", ""),
            (S2, "xkt-b.desktop", "text/x-xkt-plain;", ""),
        ],
        files: &[(
            "share2/applications/mimeapps.list",
            "[Removed Associations]\ntext/x-xkt-plain=xkt-a.desktop;xkt-b.desktop;\n",
        )],
        types: &["text/x-xkt-plain"],
    },
    Scenario {
        name: "addition-scoping",
        desktops: &[],
        apps: &[
            (S1, "xkt-a.desktop", "image/x-xkt-png;", ""),
            (S2, "xkt-b.desktop", "image/x-xkt-png;", ""),
        ],
        files: &[(
            "share2/applications/mimeapps.list",
            "[Added Associations]\ntext/x-xkt-plain=xkt-a.desktop;xkt-b.desktop;\n",
        )],
        types: &["text/x-xkt-plain"],
    },
    Scenario {
        name: "shadowed-copy-does-not-count",
        desktops: &[],
        apps: &[
            (S1, "xkt-a.desktop", "image/x-xkt-png;", ""),
            (S2, "xkt-a.desktop", "text/x-xkt-plain;", ""),
            (S2, "xkt-b.desktop", "text/x-xkt-plain;", ""),
        ],
        files: &[],
        types: &["text/x-xkt-plain", "image/x-xkt-png"],
    },
    Scenario {
        name: "local-overrides-share",
        desktops: &[],
        apps: &[
            (
                LOCAL,
                "xkt-a.desktop",
                "text/x-xkt-plain;",
                "NoDisplay=true\n",
            ),
            (S1, "xkt-a.desktop", "image/x-xkt-png;", ""),
        ],
        files: &[],
        types: &["text/x-xkt-plain", "image/x-xkt-png"],
    },
    Scenario {
        name: "desktop-specific-default",
        desktops: &["XKT", "Other"],
        apps: &[
            (S1, "xkt-a.desktop", "text/x-xkt-plain;", ""),
            (S1, "xkt-b.desktop", "text/x-xkt-plain;", ""),
            (S1, "xkt-c.desktop", "text/x-xkt-plain;", ""),
        ],
        files: &[
            (
                "config/mimeapps.list",
                "[Default Applications]\ntext/x-xkt-plain=xkt-b.desktop;\n",
            ),
            (
                "config/xkt-mimeapps.list",
                "[Default Applications]\ntext/x-xkt-plain=xkt-c.desktop;\n[Added Associations]\nimage/x-xkt-png=xkt-c.desktop;\n",
            ),
        ],
        types: &["text/x-xkt-plain", "image/x-xkt-png"],
    },
    Scenario {
        name: "system-defaults",
        desktops: &[],
        apps: &[
            (S1, "xkt-a.desktop", "text/x-xkt-plain;", ""),
            (S1, "xkt-b.desktop", "text/x-xkt-plain;", ""),
            (S1, "xkt-c.desktop", "image/x-xkt-png;", ""),
            (S1, "xkt-d.desktop", "image/x-xkt-png;", ""),
        ],
        files: &[
            (
                "etc/mimeapps.list",
                "[Default Applications]\ntext/x-xkt-plain=xkt-b.desktop;\n",
            ),
            (
                "share2/applications/mimeapps.list",
                "[Default Applications]\ntext/x-xkt-plain=xkt-a.desktop;\nimage/x-xkt-png=xkt-d.desktop;\n",
            ),
        ],
        types: &["text/x-xkt-plain", "image/x-xkt-png"],
    },
    Scenario {
        name: "parent-types",
        desktops: &[],
        apps: &[
            (S1, "xkt-text.desktop", "text/x-xkt-plain;", ""),
            (S1, "xkt-c.desktop", "text/x-xkt-src;", ""),
            (S1, "xkt-xml.desktop", "application/x-xkt-xml;", ""),
        ],
        files: &[(
            "config/mimeapps.list",
            "[Default Applications]\ntext/x-xkt-plain=xkt-text.desktop;\n",
        )],
        types: &["text/x-xkt-src", "image/x-xkt-svg", "text/x-xkt-plain"],
    },
    Scenario {
        name: "aliases",
        desktops: &[],
        apps: &[(S1, "xkt-c.desktop", "text/x-xkt-alias;", "")],
        files: &[],
        types: &["text/x-xkt-src", "text/x-xkt-alias"],
    },
    Scenario {
        name: "url-schemes",
        desktops: &[],
        apps: &[
            (
                S1,
                "xkt-web.desktop",
                "x-scheme-handler/xkt-web;x-scheme-handler/xkt-web2;",
                "",
            ),
            (S1, "xkt-mail.desktop", "x-scheme-handler/xkt-mail;", ""),
        ],
        files: &[],
        types: &["x-scheme-handler/xkt-web", "x-scheme-handler/xkt-mail"],
    },
    Scenario {
        name: "hidden-and-nodisplay",
        desktops: &[],
        apps: &[
            (S1, "xkt-a.desktop", "text/x-xkt-plain;", "Hidden=true\n"),
            (S1, "xkt-b.desktop", "text/x-xkt-plain;", "NoDisplay=true\n"),
        ],
        files: &[],
        types: &["text/x-xkt-plain"],
    },
    Scenario {
        name: "added-order-kept",
        desktops: &[],
        apps: &[
            (S1, "xkt-a.desktop", "image/x-xkt-png;", ""),
            (S1, "xkt-b.desktop", "image/x-xkt-png;", ""),
        ],
        files: &[(
            "config/mimeapps.list",
            "[Added Associations]\ntext/x-xkt-plain=xkt-b.desktop;xkt-a.desktop;\n",
        )],
        types: &["text/x-xkt-plain"],
    },
    Scenario {
        name: "default-in-a-higher-folder",
        desktops: &[],
        apps: &[
            (S1, "xkt-a.desktop", "text/x-xkt-plain;", ""),
            (S2, "xkt-b.desktop", "text/x-xkt-plain;", ""),
        ],
        files: &[(
            "share2/applications/mimeapps.list",
            "[Default Applications]\ntext/x-xkt-plain=xkt-a.desktop;\n",
        )],
        types: &["text/x-xkt-plain"],
    },
    Scenario {
        name: "added-and-removed-in-one-file",
        desktops: &[],
        apps: &[
            (S1, "xkt-a.desktop", "image/x-xkt-png;", ""),
            (S1, "xkt-b.desktop", "text/x-xkt-plain;", ""),
        ],
        files: &[(
            "config/mimeapps.list",
            "[Added Associations]\ntext/x-xkt-plain=xkt-a.desktop;\n[Removed Associations]\ntext/x-xkt-plain=xkt-a.desktop;\n",
        )],
        types: &["text/x-xkt-plain"],
    },
    Scenario {
        name: "only-applications",
        desktops: &[],
        apps: &[(S1, "xkt-c.desktop", "text/x-xkt-plain;", "")],
        files: &[
            (
                "share1/applications/xkt-a-link.desktop",
                "[Desktop Entry]\nType=Link\nName=L\nURL=https://example.org/\nMimeType=text/x-xkt-plain;\n",
            ),
            (
                "share1/applications/xkt-b-folder.desktop",
                "[Desktop Entry]\nType=Directory\nName=D\nMimeType=text/x-xkt-plain;\n",
            ),
            (
                "config/mimeapps.list",
                "[Default Applications]\ntext/x-xkt-plain=xkt-a-link.desktop;\n",
            ),
        ],
        types: &["text/x-xkt-plain"],
    },
    Scenario {
        name: "unhandled-scheme",
        desktops: &[],
        apps: &[
            (S1, "xkt-data.desktop", "application/octet-stream;", ""),
            (S1, "xkt-all.desktop", "*/*;", ""),
        ],
        files: &[],
        types: &["x-scheme-handler/xkt-none"],
    },
    Scenario {
        name: "default-removed",
        desktops: &[],
        apps: &[
            (S1, "xkt-a.desktop", "text/x-xkt-plain;", ""),
            (S1, "xkt-b.desktop", "text/x-xkt-plain;", ""),
        ],
        files: &[(
            "config/mimeapps.list",
            "[Default Applications]\ntext/x-xkt-plain=xkt-a.desktop;\n[Removed Associations]\ntext/x-xkt-plain=xkt-a.desktop;\n",
        )],
        types: &["text/x-xkt-plain"],
    },
    Scenario {
        name: "default-hidden",
        desktops: &[],
        apps: &[
            (S1, "xkt-a.desktop", "text/x-xkt-plain;", "Hidden=true\n"),
            (S1, "xkt-b.desktop", "text/x-xkt-plain;", ""),
        ],
        files: &[(
            "config/mimeapps.list",
            "[Default Applications]\ntext/x-xkt-plain=xkt-a.desktop;\n",
        )],
        types: &["text/x-xkt-plain"],
    },
    Scenario {
        name: "default-for-a-parent-app",
        desktops: &[],
        apps: &[
            (S1, "xkt-text.desktop", "text/x-xkt-plain;", ""),
            (S1, "xkt-c.desktop", "text/x-xkt-src;", ""),
        ],
        files: &[(
            "config/mimeapps.list",
            "[Default Applications]\ntext/x-xkt-src=xkt-text.desktop;\n",
        )],
        types: &["text/x-xkt-src", "text/x-xkt-plain"],
    },
    Scenario {
        name: "data-home-list",
        desktops: &[],
        apps: &[
            (S1, "xkt-a.desktop", "text/x-xkt-plain;", ""),
            (S1, "xkt-b.desktop", "text/x-xkt-plain;", ""),
            (S1, "xkt-c.desktop", "image/x-xkt-png;", ""),
        ],
        files: &[(
            "local/applications/mimeapps.list",
            "[Default Applications]\ntext/x-xkt-plain=xkt-b.desktop;\n[Added Associations]\ntext/x-xkt-plain=xkt-c.desktop;\n[Removed Associations]\ntext/x-xkt-plain=xkt-a.desktop;\n",
        )],
        types: &["text/x-xkt-plain"],
    },
    Scenario {
        name: "config-dirs",
        desktops: &[],
        apps: &[
            (S1, "xkt-a.desktop", "text/x-xkt-plain;", ""),
            (S1, "xkt-b.desktop", "text/x-xkt-plain;", ""),
            (S1, "xkt-c.desktop", "image/x-xkt-png;", ""),
        ],
        files: &[
            (
                "etc/mimeapps.list",
                "[Added Associations]\ntext/x-xkt-plain=xkt-c.desktop;\n[Removed Associations]\ntext/x-xkt-plain=xkt-a.desktop;\n",
            ),
            (
                "etc2/mimeapps.list",
                "[Default Applications]\ntext/x-xkt-plain=xkt-a.desktop;xkt-b.desktop;\nimage/x-xkt-png=xkt-gone.desktop;\n",
            ),
            (
                "config/mimeapps.list",
                "[Default Applications]\nimage/x-xkt-png=xkt-gone.desktop;\n",
            ),
        ],
        types: &["text/x-xkt-plain", "image/x-xkt-png"],
    },
    Scenario {
        name: "desktop-names-in-order",
        desktops: &["XKT", "Other"],
        apps: &[
            (S1, "xkt-a.desktop", "text/x-xkt-plain;", ""),
            (S1, "xkt-b.desktop", "text/x-xkt-plain;", ""),
            (S1, "xkt-c.desktop", "text/x-xkt-plain;image/x-xkt-png;", ""),
            (S1, "xkt-d.desktop", "image/x-xkt-png;", ""),
        ],
        files: &[
            (
                "config/other-mimeapps.list",
                "[Default Applications]\ntext/x-xkt-plain=xkt-b.desktop;\n",
            ),
            (
                "config/xkt-mimeapps.list",
                "[Default Applications]\ntext/x-xkt-plain=xkt-c.desktop;\n",
            ),
            (
                "share1/applications/other-mimeapps.list",
                "[Default Applications]\nimage/x-xkt-png=xkt-d.desktop;\n",
            ),
        ],
        types: &["text/x-xkt-plain", "image/x-xkt-png"],
    },
    Scenario {
        name: "hidden-masks-a-lower-copy",
        desktops: &[],
        apps: &[
            (LOCAL, "xkt-a.desktop", "text/x-xkt-plain;", "Hidden=true\n"),
            (S1, "xkt-a.desktop", "text/x-xkt-plain;", ""),
            (S1, "xkt-b.desktop", "text/x-xkt-plain;", ""),
        ],
        files: &[],
        types: &["text/x-xkt-plain"],
    },
    Scenario {
        name: "specific-addition-survives",
        desktops: &[],
        apps: &[
            (S1, "xkt-a.desktop", "image/x-xkt-png;", ""),
            (S1, "xkt-b.desktop", "text/x-xkt-plain;", ""),
        ],
        files: &[
            (
                "share1/applications/mimeapps.list",
                "[Added Associations]\ntext/x-xkt-src=xkt-a.desktop;\n",
            ),
            (
                "config/mimeapps.list",
                "[Removed Associations]\ntext/x-xkt-plain=xkt-a.desktop;\n",
            ),
        ],
        types: &["text/x-xkt-src"],
    },
    Scenario {
        name: "alias-as-key",
        desktops: &[],
        apps: &[
            (S1, "xkt-a.desktop", "text/x-xkt-src;", ""),
            (S1, "xkt-b.desktop", "text/x-xkt-src;", ""),
        ],
        files: &[(
            "config/mimeapps.list",
            "[Default Applications]\ntext/x-xkt-alias=xkt-b.desktop;\n",
        )],
        types: &["text/x-xkt-src"],
    },
    Scenario {
        name: "wildcards",
        desktops: &[],
        apps: &[
            (S1, "xkt-images.desktop", "image/*;", ""),
            (S1, "xkt-all.desktop", "*/*;", ""),
        ],
        files: &[],
        types: &["image/x-xkt-png"],
    },
    Scenario {
        name: "implied-parents",
        desktops: &[],
        apps: &[
            (S1, "xkt-text.desktop", "text/plain;", ""),
            (S1, "xkt-data.desktop", "application/octet-stream;", ""),
        ],
        files: &[],
        types: &["text/x-xkt-foo", "image/x-xkt-png"],
    },
];

fn build(scenario: &Scenario) -> Fixture {
    let mut fixture = Fixture::new();
    fixture.desktops = scenario
        .desktops
        .iter()
        .map(|&name| name.to_owned())
        .collect();
    for (dir, id, types, extra) in scenario.apps {
        fixture.app(dir, id, types, extra);
    }
    for (path, text) in scenario.files {
        fixture.write(path, text);
    }
    for folder in fixture.applications() {
        if folder.is_dir() {
            update_desktop_database(&folder);
        }
    }
    fixture
}

/// The listed differences: (scenario, type) → (our answer, reason).
fn listed() -> BTreeMap<(String, String), (String, String)> {
    common::read_differences("gio-differences.txt", 4)
        .into_iter()
        .map(|fields| {
            let [scenario, mime, ours, reason] = <[String; 4]>::try_from(fields).unwrap();
            ((scenario, mime), (ours, reason))
        })
        .collect()
}

#[test]
fn answers_match_gio() {
    let mut listed = listed();
    let mut unexpected = Vec::new();
    let mut stale = Vec::new();
    for scenario in SCENARIOS {
        let fixture = build(scenario);
        for &mime in scenario.types {
            let (ours, gio) = (ours(&fixture, mime), gio(&fixture, mime));
            let key = (scenario.name.to_owned(), mime.to_owned());
            match (listed.remove(&key), ours == gio) {
                (None, false) => {
                    unexpected.push(format!(
                        "{}\t{mime}\t{}\t<reason>\n  gio  {}",
                        scenario.name,
                        ours.summary(),
                        gio.summary()
                    ));
                }
                (Some(_), true) => {
                    stale.push(format!(
                        "{}\t{mime}\tnow the same: {}",
                        scenario.name,
                        ours.summary()
                    ));
                }
                (Some((expected, _)), false) if expected != ours.summary() => {
                    stale.push(format!(
                        "{}\t{mime}\tlisted {expected}, ours now {}",
                        scenario.name,
                        ours.summary()
                    ));
                }
                _ => {}
            }
        }
    }
    for (scenario, mime) in listed.keys() {
        stale.push(format!("{scenario}\t{mime}\tnot a scenario's type"));
    }
    assert!(
        unexpected.is_empty() && stale.is_empty(),
        "{} answers differ and are not listed:\n{}\n\n{} listed differences no longer hold:\n{}",
        unexpected.len(),
        unexpected.join("\n"),
        stale.len(),
        stale.join("\n"),
    );
}
