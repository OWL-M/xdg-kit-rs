//! Counts work in tests to check that it grows linearly with the input.
//! A step is an item visited (a name, a locale or a key) or a byte
//! examined. Counting does nothing outside tests.
//!
//! Counters detect only the work they measure. Every operation on a tested
//! path whose cost grows with the input must count its steps. This includes
//! each item visited by a loop (each name and repeat check) and each byte
//! scanned in a name. Library calls such as `split_once` count the bytes
//! they may scan, just as explicit loops do.
//!
//! Count each input byte copied (`to_owned`, `clone`, `format!`), hashed or
//! compared with other input. Set and map insertions and lookups count
//! hashing and comparison at two steps per byte. Comparisons with fixed
//! names such as `C` or `POSIX` are bounded by the literal's length and
//! are not counted. New loops over input on a tested path must count their
//! steps too.
//!
//! The test-only `assert_linear` runs each test at two input sizes.
//! Doubling the size may increase the work by at most 2.5 times: linear
//! work roughly doubles, while quadratic work roughly quadruples. Each
//! count must also stay within a bound based on items and bytes, set at
//! about twice the analysed cost.

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

/// Returns `f`'s result and the number of steps it counted on this thread.
#[cfg(test)]
pub(crate) fn steps<T>(f: impl FnOnce() -> T) -> (T, usize) {
    let before = STEPS.with(std::cell::Cell::get);
    let value = f();
    (value, STEPS.with(std::cell::Cell::get) - before)
}

/// Runs `run` at `n` items and at `2 * n`; it returns the steps
/// counted and the bytes of input. Asserts that the work at `2 * n` is
/// at most 2.5 times that at `n` (linear work about doubles, quadratic
/// work about quadruples), then that each count is within `bound` of
/// its items and bytes.
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
