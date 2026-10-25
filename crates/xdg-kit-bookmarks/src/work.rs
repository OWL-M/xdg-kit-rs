//! Counts work in tests to check that it grows linearly with the input.
//! A step is an item visited (an XML event, a bookmark, a list item) or a
//! byte examined. Counting does nothing outside tests.
//!
//! The counts can only show what they are told: on a path a counted test
//! runs, every step that grows with the input counts, each item a loop
//! visits included (each event, each entry, each repeat check), and each
//! byte a scan of a value looks at, whether the loop is written here or is
//! a library call such as `position` or `from_utf8` (counted by the bytes
//! it may walk). So does each byte of input copied (`to_owned`, `clone`,
//! a byte pushed), hashed (a set or map key inserted or looked up: hashed
//! and compared, two steps per byte) or compared with other input (maps
//! and sets keyed by text are [`xdg_kit_basedir::__hashed`]'s, which count
//! their hashing, comparing and copying on this counter ([`Counted`]) and
//! grow a step per entry moved, whatever the keys' length), and each byte
//! of a message that input is written into. Comparing with a fixed name
//! (a literal such as a known element name) is bounded by that name and
//! not counted; nor is file I/O. A new loop over input on such a path must
//! count its steps too.
//!
//! A counted test runs at two sizes with `assert_linear` (test-only): the
//! work at twice the size may be at most 2.5 times the work at the first
//! (linear work about doubles, quadratic work about quadruples), and each
//! count stays within a bound in items and bytes of about twice the
//! analysed cost.

#[cfg(test)]
thread_local! {
    #[allow(
        clippy::missing_const_for_thread_local,
        reason = "already const; clippy flags it on targets without native thread locals (Android, OpenBSD, illumos)"
    )]
    static STEPS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Counts `steps` steps on this thread.
#[inline]
pub(crate) fn count(steps: usize) {
    #[cfg(test)]
    STEPS.with(|done| done.set(done.get() + steps));
    #[cfg(not(test))]
    let _ = steps;
}

/// Counts the work of the shared maps keyed by text
/// ([`xdg_kit_basedir::__hashed`]) on this crate's counter.
pub(crate) struct Counted;

impl xdg_kit_basedir::__hashed::Count for Counted {
    fn count(steps: impl FnOnce() -> usize) {
        #[cfg(test)]
        count(steps());
        #[cfg(not(test))]
        let _ = steps;
    }
}

/// Returns `f`'s result and the number of steps it counted on this thread.
#[cfg(test)]
pub(crate) fn steps<T>(f: impl FnOnce() -> T) -> (T, usize) {
    let before = STEPS.with(std::cell::Cell::get);
    let value = f();
    (value, STEPS.with(std::cell::Cell::get) - before)
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
