//! Helpers for the tests of FIFOs.

use std::path::Path;
use std::time::Duration;

/// Makes a FIFO at `path`.
pub(crate) fn fifo(path: &Path) {
    rustix::fs::mknodat(
        rustix::fs::CWD,
        path,
        rustix::fs::FileType::Fifo,
        rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
        0,
    )
    .unwrap();
}

/// Runs `f` in a thread and returns its result. If a FIFO blocks the
/// read, the test fails after `timeout`.
pub(crate) fn within<T: Send + 'static>(
    timeout: Duration,
    f: impl FnOnce() -> T + Send + 'static,
) -> T {
    let (send, receive) = std::sync::mpsc::channel();
    std::thread::spawn(move || send.send(f()));
    receive.recv_timeout(timeout).expect("it returns in time")
}
