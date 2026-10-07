//! Shared by the integration tests.

use std::path::Path;
use std::process::Command;

/// Runs `update-mime-database` on the `mime` folder `mime` under the data
/// folder `data_dir` (which keeps it from warning that the folder is not on
/// the search path). The dev shell provides the tool.
pub fn update_mime_database(mime: &Path, data_dir: &Path) {
    let status = Command::new("update-mime-database")
        .arg(mime)
        .env("XDG_DATA_DIRS", data_dir)
        .env("XDG_DATA_HOME", data_dir)
        .status()
        .expect(
            "update-mime-database is not on PATH: run the tests in the dev shell (nix develop)",
        );
    assert!(status.success(), "update-mime-database failed");
}
