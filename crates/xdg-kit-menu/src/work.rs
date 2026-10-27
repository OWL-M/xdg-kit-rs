//! Counts work in tests to check that it grows linearly with the input.
//! A step is an item visited (a directory, a listed name, a candidate
//! file) or a byte examined. Counting does nothing outside tests.
//!
//! The steps are counted on the key file's counter
//! (`xdg_kit_desktop_entry::__count_work`), so a counted test sees this
//! crate's work and the parsing of each entry together. Every step that
//! grows with the input counts: each item a loop visits, and each byte of
//! input scanned, copied, compared or sorted against another. Comparing
//! with a fixed name (such as the `.desktop` suffix) is bounded by that
//! name and not counted; nor is file I/O.
//!
//! A counted test runs at two sizes with `assert_linear` (test-only): the
//! work at twice the size may be at most 2.5 times the work at the first
//! (linear work about doubles, quadratic work about quadruples), and each
//! count stays within a bound in items and bytes of about twice the
//! analysed cost.

/// Counts `steps` steps on this thread, on the key file's counter.
#[inline]
pub(crate) fn count(steps: usize) {
    #[cfg(test)]
    xdg_kit_desktop_entry::__count_work::count(steps);
    #[cfg(not(test))]
    let _ = steps;
}

/// What `f` returns, and the steps it counted on this thread, here and in
/// the key file.
#[cfg(test)]
pub(crate) fn steps<T>(f: impl FnOnce() -> T) -> (T, usize) {
    let before = xdg_kit_desktop_entry::__count_work::total();
    let value = f();
    (value, xdg_kit_desktop_entry::__count_work::total() - before)
}

/// Runs `run` at `n` items and at `2 * n`; it returns the steps counted
/// and the bytes of input. Asserts that the work at `2 * n` is at most
/// 2.5 times that at `n` (linear work about doubles, quadratic work about
/// quadruples), then that each count is within `bound` of its items and
/// bytes.
#[cfg(test)]
#[track_caller]
pub(crate) fn assert_linear(
    n: usize,
    run: impl Fn(usize) -> (usize, usize),
    bound: impl Fn(usize, usize) -> usize,
) {
    let (single, single_bytes) = run(n);
    let (doubled, doubled_bytes) = run(2 * n);
    let (single_bound, doubled_bound) = (bound(n, single_bytes), bound(2 * n, doubled_bytes));
    assert!(
        2 * doubled <= 5 * single,
        "{doubled} steps for {} items against {single} for {n}: more than 2.5 times \
         (bounds {doubled_bound}, {single_bound})",
        2 * n
    );
    for (items, steps, bytes, bound) in [
        (n, single, single_bytes, single_bound),
        (2 * n, doubled, doubled_bytes, doubled_bound),
    ] {
        assert!(
            steps <= bound,
            "{steps} steps for {items} items, {bytes} bytes: more than the bound {bound} \
             (n = {n})"
        );
    }
}
