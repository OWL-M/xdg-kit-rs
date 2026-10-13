//! The Desktop Menu spec's registries (version 1.1, xdg-specs `1e7f4ce`,
//! `menu/menu-spec.xml`): categories ("Registered Categories") and desktop
//! names ("Registered `OnlyShowIn` Environments"), used by `validate`. A test
//! checks these tables against the pinned spec, allowing one correction
//! (`Qt` for the spec's `QT`, see [`ADDITIONAL_CATEGORIES`]). Names used by
//! desktops and compositors in `XDG_CURRENT_DESKTOP` but absent from the spec
//! are listed separately in [`KNOWN_DESKTOPS`]. The test checks that the spec
//! registers none of them.

/// Main categories, each with the category it requires besides itself
/// ("Desktop entry must include ... as well").
pub(crate) const MAIN_CATEGORIES: &[(&str, Option<&str>)] = &[
    ("AudioVideo", None),
    ("Audio", Some("AudioVideo")),
    ("Video", Some("AudioVideo")),
    ("Development", None),
    ("Education", None),
    ("HealthFitness", None),
    ("Game", None),
    ("Graphics", None),
    ("Network", None),
    ("Office", None),
    ("Science", None),
    ("Settings", None),
    ("System", None),
    ("Utility", None),
];

/// Additional categories, each with its related categories: any one of
/// the alternatives, each a set of categories, should be present too. The
/// spec's KDE and `LXQt` rows say `QT`, a typo for the registered `Qt`, as
/// `desktop-file-validate` reads them; here they say `Qt`.
pub(crate) const ADDITIONAL_CATEGORIES: &[(&str, &[&[&str]])] = &[
    ("Building", &[&["Development"]]),
    ("Debugger", &[&["Development"]]),
    ("IDE", &[&["Development"]]),
    ("GUIDesigner", &[&["Development"]]),
    ("Profiling", &[&["Development"]]),
    ("RevisionControl", &[&["Development"]]),
    ("Translation", &[&["Development"]]),
    ("Calendar", &[&["Office"]]),
    ("ContactManagement", &[&["Office"]]),
    (
        "Database",
        &[&["Office"], &["Development"], &["AudioVideo"]],
    ),
    ("Dictionary", &[&["Office"], &["TextTools"]]),
    ("Chart", &[&["Office"]]),
    ("Email", &[&["Office"], &["Network"]]),
    ("Finance", &[&["Office"]]),
    ("FlowChart", &[&["Office"]]),
    ("PDA", &[&["Office"]]),
    ("ProjectManagement", &[&["Office"], &["Development"]]),
    ("Presentation", &[&["Office"]]),
    ("Spreadsheet", &[&["Office"]]),
    ("WordProcessor", &[&["Office"]]),
    ("2DGraphics", &[&["Graphics"]]),
    ("VectorGraphics", &[&["Graphics", "2DGraphics"]]),
    ("RasterGraphics", &[&["Graphics", "2DGraphics"]]),
    ("3DGraphics", &[&["Graphics"]]),
    ("Scanning", &[&["Graphics"]]),
    ("OCR", &[&["Graphics", "Scanning"]]),
    ("Photography", &[&["Graphics"], &["Office"]]),
    ("Publishing", &[&["Graphics"], &["Office"]]),
    ("Viewer", &[&["Graphics"], &["Office"]]),
    ("TextTools", &[&["Utility"]]),
    ("DesktopSettings", &[&["Settings"]]),
    ("HardwareSettings", &[&["Settings"]]),
    ("Printing", &[&["HardwareSettings", "Settings"]]),
    ("PackageManager", &[&["Settings"]]),
    ("Dialup", &[&["Network"]]),
    ("InstantMessaging", &[&["Network"]]),
    ("Chat", &[&["Network"]]),
    ("IRCClient", &[&["Network"]]),
    ("Feed", &[&["Network"]]),
    ("FileTransfer", &[&["Network"]]),
    ("HamRadio", &[&["Network"], &["Audio"]]),
    ("News", &[&["Network"]]),
    ("P2P", &[&["Network"]]),
    ("RemoteAccess", &[&["Network"]]),
    ("Telephony", &[&["Network"]]),
    ("TelephonyTools", &[&["Utility"]]),
    ("VideoConference", &[&["Network"]]),
    ("WebBrowser", &[&["Network"]]),
    ("WebDevelopment", &[&["Network"], &["Development"]]),
    ("Midi", &[&["AudioVideo", "Audio"]]),
    ("Mixer", &[&["AudioVideo", "Audio"]]),
    ("Sequencer", &[&["AudioVideo", "Audio"]]),
    ("Tuner", &[&["AudioVideo", "Audio"]]),
    ("TV", &[&["AudioVideo", "Video"]]),
    (
        "AudioVideoEditing",
        &[&["Audio"], &["Video"], &["AudioVideo"]],
    ),
    ("Player", &[&["Audio"], &["Video"], &["AudioVideo"]]),
    ("Recorder", &[&["Audio"], &["Video"], &["AudioVideo"]]),
    ("DiscBurning", &[&["AudioVideo"]]),
    ("ActionGame", &[&["Game"]]),
    ("AdventureGame", &[&["Game"]]),
    ("ArcadeGame", &[&["Game"]]),
    ("BoardGame", &[&["Game"]]),
    ("BlocksGame", &[&["Game"]]),
    ("CardGame", &[&["Game"]]),
    ("KidsGame", &[&["Game"]]),
    ("LogicGame", &[&["Game"]]),
    ("RolePlaying", &[&["Game"]]),
    ("Shooter", &[&["Game"]]),
    ("Simulation", &[&["Game"]]),
    ("SportsGame", &[&["Game"]]),
    ("StrategyGame", &[&["Game"]]),
    ("LauncherStore", &[&["Game"]]),
    ("GameTool", &[&["Game"]]),
    ("Exercise", &[&["HealthFitness"]]),
    ("Medication", &[&["HealthFitness"]]),
    ("Mindfulness", &[&["HealthFitness"]]),
    ("Nutrition", &[&["HealthFitness"]]),
    ("CareProvider", &[&["HealthFitness"]]),
    ("Sleep", &[&["HealthFitness"]]),
    ("Art", &[&["Education"], &["Science"]]),
    ("Construction", &[&["Education"], &["Science"]]),
    ("Music", &[&["AudioVideo"], &["Education"]]),
    ("Languages", &[&["Education"], &["Science"]]),
    ("ArtificialIntelligence", &[&["Education"], &["Science"]]),
    ("Astronomy", &[&["Education"], &["Science"]]),
    ("Biology", &[&["Education"], &["Science"]]),
    ("Chemistry", &[&["Education"], &["Science"]]),
    ("ComputerScience", &[&["Education"], &["Science"]]),
    ("DataVisualization", &[&["Education"], &["Science"]]),
    ("Economy", &[&["Education"], &["Science"]]),
    ("Electricity", &[&["Education"], &["Science"]]),
    ("Geography", &[&["Education"], &["Science"]]),
    ("Geology", &[&["Education"], &["Science"]]),
    ("Geoscience", &[&["Education"], &["Science"]]),
    ("History", &[&["Education"], &["Science"]]),
    ("Humanities", &[&["Education"], &["Science"]]),
    ("ImageProcessing", &[&["Education"], &["Science"]]),
    ("Literature", &[&["Education"], &["Science"]]),
    ("Maps", &[&["Education"], &["Science"], &["Utility"]]),
    ("Math", &[&["Education"], &["Science"]]),
    (
        "NumericalAnalysis",
        &[&["Education", "Math"], &["Science", "Math"]],
    ),
    ("MedicalSoftware", &[&["Education"], &["Science"]]),
    ("Physics", &[&["Education"], &["Science"]]),
    ("Robotics", &[&["Education"], &["Science"]]),
    (
        "Spirituality",
        &[&["Education"], &["Science"], &["Utility"]],
    ),
    ("Sports", &[&["Education"], &["Science"]]),
    (
        "ParallelComputing",
        &[
            &["Education", "ComputerScience"],
            &["Science", "ComputerScience"],
        ],
    ),
    ("Amusement", &[]),
    ("Archiving", &[&["Utility"]]),
    ("Compression", &[&["Utility", "Archiving"]]),
    ("Electronics", &[]),
    ("Emulator", &[&["System"], &["Game"]]),
    ("Engineering", &[]),
    ("FileTools", &[&["Utility"], &["System"]]),
    ("FileManager", &[&["System", "FileTools"]]),
    ("TerminalEmulator", &[&["System"]]),
    ("Filesystem", &[&["System"]]),
    ("Monitor", &[&["System"], &["Network"]]),
    ("Security", &[&["Settings"], &["System"]]),
    ("Accessibility", &[&["Settings"], &["Utility"]]),
    ("Calculator", &[&["Utility"]]),
    ("Clock", &[&["Utility"]]),
    ("TextEditor", &[&["Utility"]]),
    ("Documentation", &[]),
    ("Adult", &[]),
    ("Core", &[]),
    ("KDE", &[&["Qt"]]),
    ("COSMIC", &[]),
    ("GNOME", &[&["GTK"]]),
    ("LXQt", &[&["Qt"]]),
    ("XFCE", &[&["GTK"]]),
    ("DDE", &[&["Qt"]]),
    ("GTK", &[]),
    ("Qt", &[]),
    ("Motif", &[]),
    ("Java", &[]),
    ("ConsoleOnly", &[]),
];

/// Reserved categories: only for desktop-specific entries, which need
/// `OnlyShowIn`.
pub(crate) const RESERVED_CATEGORIES: &[&str] = &["Screensaver", "TrayIcon", "Applet", "Shell"];

/// Registered desktop names for `OnlyShowIn` and `NotShowIn`.
pub(crate) const DESKTOPS: &[&str] = &[
    "COSMIC",
    "GNOME",
    "GNOME-Classic",
    "GNOME-Flashback",
    "KDE",
    "LXDE",
    "LXQt",
    "MATE",
    "Razor",
    "ROX",
    "TDE",
    "Unity",
    "XFCE",
    "EDE",
    "Cinnamon",
    "Pantheon",
    "Phosh",
    "DDE",
    "Endless",
    "Old",
];

/// Desktop names absent from the menu spec but used by desktops and
/// compositors in `XDG_CURRENT_DESKTOP`, either set in code or supplied by
/// `DesktopNames` in their session files. Validation accepts these names in
/// desktop entries. `Budgie` and `Enlightenment` also appear in
/// `desktop-file-validate`'s list. The values used by sway, Wayfire, labwc
/// and mango share the `wlroots` component. Wayfire's session file supplies
/// `Wayfire`; Wayfire itself sets `wayfire` when the variable is unset.
pub(crate) const KNOWN_DESKTOPS: &[&str] = &[
    "Budgie",
    "Enlightenment",
    "Hyprland",
    "sway",
    "niri",
    "Wayfire",
    "wayfire",
    "labwc",
    "gamescope",
    "jay",
    "mango",
    "pinnacle",
    "Miriway",
    "wlroots",
];

#[cfg(test)]
mod tests {
    use super::*;

    /// The pinned xdg-specs the dev shell points to.
    fn menu_spec() -> String {
        let dir = std::env::var_os("XDG_KIT_XDG_SPECS_SRC").expect(
            "XDG_KIT_XDG_SPECS_SRC is not set: run the tests in the dev shell (nix develop)",
        );
        std::fs::read_to_string(std::path::Path::new(&dir).join("menu/menu-spec.xml")).unwrap()
    }

    /// The text of the `<entry>` cells of each `<row>` in the part of `xml`
    /// from `id` to `end`, header row left out; tags dropped, white space
    /// collapsed.
    fn rows(xml: &str, id: &str, end: &str) -> Vec<Vec<String>> {
        let start = xml.find(&format!("id=\"{id}\"")).unwrap();
        let part = &xml[start..start + xml[start..].find(end).unwrap()];
        let cells = |row: &str| -> Vec<String> {
            row.split("<entry>")
                .skip(1)
                .map(|cell| {
                    let cell = &cell[..cell.find("</entry>").unwrap()];
                    let mut text = String::new();
                    let mut in_tag = false;
                    for c in cell.chars() {
                        match c {
                            '<' => in_tag = true,
                            '>' => in_tag = false,
                            _ if !in_tag => text.push(c),
                            _ => {}
                        }
                    }
                    text.replace("&amp;", "&")
                        .split_whitespace()
                        .collect::<Vec<_>>()
                        .join(" ")
                })
                .collect()
        };
        part.split("<row>")
            .skip(2)
            .map(|row| cells(&row[..row.find("</row>").unwrap()]))
            .collect()
    }

    #[test]
    fn the_tables_match_the_pinned_menu_spec() {
        let xml = menu_spec();
        let main: Vec<(String, Option<String>)> = rows(&xml, "main-category-registry", "</sect1>")
            .into_iter()
            .map(|row| {
                let required = row.get(2).and_then(|note| {
                    note.strip_prefix("Desktop entry must include ")
                        .and_then(|rest| rest.strip_suffix(" as well"))
                        .map(str::to_owned)
                });
                (row[0].clone(), required)
            })
            .collect();
        let ours: Vec<(String, Option<String>)> = MAIN_CATEGORIES
            .iter()
            .map(|(name, required)| ((*name).to_owned(), required.map(str::to_owned)))
            .collect();
        assert_eq!(ours, main);

        // The one correction allowed: the spec's KDE and LXQt rows relate
        // them to `QT`, a typo upstream for the registered `Qt`.
        let corrected = |name: &str, related: &str| match (name, related) {
            ("KDE" | "LXQt", "QT") => "Qt".to_owned(),
            _ => related.to_owned(),
        };
        let additional: Vec<(String, String)> =
            rows(&xml, "additional-category-registry", "</sect1>")
                .into_iter()
                .map(|row| (row[0].clone(), corrected(&row[0], &row[2])))
                .collect();
        let typos = rows(&xml, "additional-category-registry", "</sect1>")
            .into_iter()
            .filter(|row| row[2] == "QT")
            .map(|row| row[0].clone())
            .collect::<Vec<_>>();
        assert_eq!(typos, ["KDE", "LXQt"], "the typo the correction is for");
        let ours: Vec<(String, String)> = ADDITIONAL_CATEGORIES
            .iter()
            .map(|(name, related)| {
                let related: Vec<String> = related.iter().map(|set| set.join(";")).collect();
                ((*name).to_owned(), related.join(" or "))
            })
            .collect();
        assert_eq!(ours, additional);

        let reserved: Vec<String> = rows(&xml, "reserved-category-registry", "</sect1>")
            .into_iter()
            .map(|row| row[0].clone())
            .collect();
        assert_eq!(RESERVED_CATEGORIES, reserved);

        let desktops: Vec<String> = rows(&xml, "onlyshowin-registry", "</appendix>")
            .into_iter()
            .map(|row| row[0].clone())
            .collect();
        let registered: Vec<&&str> = KNOWN_DESKTOPS
            .iter()
            .filter(|name| desktops.iter().any(|desktop| desktop == **name))
            .collect();
        assert!(
            registered.is_empty(),
            "registered by the menu spec, so not for KNOWN_DESKTOPS: {registered:?}"
        );
        assert_eq!(DESKTOPS, desktops);
    }

    #[test]
    fn known_desktops_are_distinct_and_not_extensions() {
        let mut seen = std::collections::HashSet::new();
        for name in KNOWN_DESKTOPS {
            assert!(seen.insert(name), "{name} twice");
            assert!(!name.starts_with("X-"), "{name} is valid as an extension");
        }
    }
}
