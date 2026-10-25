//! A loaded bookmark file must keep addressing the file it read when the
//! working directory changes afterwards. The working directory is
//! process-global, so this binary holds exactly one test and nothing
//! else runs beside it.

use std::fs;

use xdg_kit_bookmarks::{BookmarkFile, Opened};

fn opened() -> Opened {
    Opened {
        name: "viewer".to_owned(),
        exec: "'viewer %u'".to_owned(),
        mime: xdg_kit_mime::MimeType::new("text/plain").unwrap(),
        groups: Vec::new(),
        private: false,
        when: std::time::SystemTime::UNIX_EPOCH,
    }
}

#[test]
fn a_loaded_file_survives_a_working_directory_change() {
    let here = tempfile::tempdir().unwrap();
    let elsewhere = tempfile::tempdir().unwrap();
    fs::write(here.path().join("recently-used.xbel"), "<xbel>\n</xbel>\n").unwrap();
    fs::write(
        elsewhere.path().join("recently-used.xbel"),
        "<xbel><bookmark href=\"file:///decoy\"/></xbel>",
    )
    .unwrap();
    std::env::set_current_dir(here.path()).unwrap();
    let mut file = BookmarkFile::read("recently-used.xbel").unwrap();
    std::env::set_current_dir(elsewhere.path()).unwrap();

    // Refresh keeps watching the file that was read, not the decoy the
    // relative name now points at.
    assert!(file.refresh().is_none(), "the read file is unchanged");

    // A write lands in the file that was read.
    file.open("file:///mine", &opened()).unwrap();
    let _ = file.write().unwrap();
    let written = BookmarkFile::read(here.path().join("recently-used.xbel")).unwrap();
    assert!(written.get("file:///mine").is_some());
    let decoy = BookmarkFile::read(elsewhere.path().join("recently-used.xbel")).unwrap();
    assert!(
        decoy.get("file:///mine").is_none(),
        "the decoy is untouched"
    );
    assert!(decoy.get("file:///decoy").is_some());
}
