//! Relative paths after the working directory changes. All tests in a
//! binary share the process's working directory, so this binary has only
//! one test.

use std::{env, fs};

use xdg_kit_locale::Locales;
use xdg_kit_mime::{MimeDatabase, MimeType};

/// A `mime` folder in `dir` whose `text/x-foo` matches `*.<extension>` and
/// is described as `comment`.
fn build(dir: &std::path::Path, extension: &str, comment: &str) {
    fs::create_dir_all(dir.join("mime/text")).unwrap();
    fs::write(
        dir.join("mime/globs2"),
        format!("50:text/x-foo:*.{extension}\n"),
    )
    .unwrap();
    fs::write(
        dir.join("mime/text/x-foo.xml"),
        format!(
            r#"<?xml version="1.0"?><mime-type xmlns="http://www.freedesktop.org/standards/shared-mime-info" type="text/x-foo"><comment>{comment}</comment></mime-type>"#
        ),
    )
    .unwrap();
}

#[test]
fn a_relative_folder_keeps_its_files_after_the_folder_changes() {
    let root = tempfile::tempdir().unwrap();
    let a = root.path().join("A");
    let b = root.path().join("B");
    build(&a, "foo", "from A");
    build(&b, "bar", "from B");
    let foo = MimeType::new("text/x-foo").unwrap();
    let best = |db: &MimeDatabase, name: &str| {
        db.type_for_name(name)
            .best()
            .map(|decided| decided.mime.to_string())
    };

    env::set_current_dir(&a).unwrap();
    let here = env::current_dir().unwrap();
    let db = MimeDatabase::from_dirs(["mime"]);
    assert_eq!(db.dirs(), [here.join("mime").as_path()]);

    env::set_current_dir(&b).unwrap();
    // The description is read lazily, from A.
    assert_eq!(
        db.description(&foo, &Locales::untranslated()).as_deref(),
        Some("from A")
    );
    // Nothing changed in A's folder; a forced reread keeps A's rules.
    assert!(db.refresh().is_none());
    fs::write(a.join("mime/globs2"), "50:text/x-foo:*.foo\n# touched\n").unwrap();
    let db = db.refresh().expect("A's globs changed");
    assert_eq!(best(&db, "x.foo").as_deref(), Some("text/x-foo"));
    assert_eq!(best(&db, "x.bar"), None);
    assert_eq!(db.dirs(), [here.join("mime").as_path()]);
    env::set_current_dir(root.path()).unwrap();
}
