//! Counts work in tests to check that it grows linearly with the input.
//! A step is an item visited (a line, record or list item) or a byte
//! examined. Counting does nothing outside tests.
//!
//! Counters detect only the work they measure. Every operation on a tested
//! path whose cost grows with the input must count its steps. This includes
//! each line, database record, list item and repeat check visited by a loop,
//! and each byte scanned in a value, line or record. Library calls such as
//! `position` and `from_utf8` count the bytes they may scan, just as explicit
//! loops do.
//!
//! Copying input (`to_owned`, `clone`, `join`, pushing a byte) counts one
//! step per byte. Hashing and comparing a set or map key for insertion or
//! lookup counts two; copying the key as well counts three. Also count each
//! byte compared with other input and each byte of a message containing
//! input. Comparisons of two [`MimeType`](crate::MimeType)s count the bytes
//! compared, like other input comparisons. Comparisons with fixed names
//! (literals such as known element or media type names) are bounded by the
//! literal's length and are not counted. File I/O is not counted either.
//! New loops over input on a tested path must count their steps too.
//!
//! The test-only `assert_linear` runs each test at two input sizes.
//! Doubling the size may increase the work by at most 2.5 times: linear
//! work roughly doubles, while quadratic work roughly quadruples. Each
//! count must also stay within a bound based on items and bytes, set at
//! about twice the analysed cost.
//!
//! Tests also track memory where it can grow faster than the input, as in
//! `xml`'s entity loop analysis. Each buffer records its size for as long
//! as it is held ([`hold`]); tests read the peak through the test-only
//! `peak`. This measures only buffers that record their size.

#[cfg(test)]
thread_local! {
    #[allow(
        clippy::missing_const_for_thread_local,
        reason = "already const; clippy flags it on targets without native thread locals (Android, OpenBSD, illumos)"
    )]
    static HELD: std::cell::Cell<(usize, usize)> = const { std::cell::Cell::new((0, 0)) };
}

/// Bytes noted as held until it is dropped ([`hold`]); nothing outside
/// tests.
#[must_use]
pub(crate) struct Held(#[cfg(test)] usize);

/// Notes, in tests, `bytes` held until the result is dropped.
#[inline]
pub(crate) fn hold(bytes: impl FnOnce() -> usize) -> Held {
    #[cfg(test)]
    {
        let bytes = bytes();
        HELD.with(|held| {
            let (now, most) = held.get();
            held.set((now + bytes, most.max(now + bytes)));
        });
        Held(bytes)
    }
    #[cfg(not(test))]
    {
        let _ = bytes;
        Held()
    }
}

/// The bytes of a buffer of `capacity` items of `T`, for [`hold`].
pub(crate) const fn size<T>(capacity: usize) -> usize {
    capacity * std::mem::size_of::<T>()
}

#[cfg(test)]
impl Drop for Held {
    fn drop(&mut self) {
        HELD.with(|held| {
            let (now, most) = held.get();
            held.set((now - self.0, most));
        });
    }
}

/// Returns `f`'s result and the most bytes noted as held at once while it
/// ran ([`hold`]), above what was held before.
#[cfg(test)]
pub(crate) fn peak<T>(f: impl FnOnce() -> T) -> (T, usize) {
    let (before, most) = HELD.with(std::cell::Cell::get);
    HELD.with(|held| held.set((before, before)));
    let value = f();
    let (now, top) = HELD.with(std::cell::Cell::get);
    HELD.with(|held| held.set((now, most.max(top))));
    (value, top - before)
}

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

/// Evaluates `steps` and counts its result in tests only. Use when
/// calculating the count requires its own traversal, such as counting
/// bytes in a nested rule.
#[inline]
pub(crate) fn count_by(steps: impl FnOnce() -> usize) {
    #[cfg(test)]
    count(steps());
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

#[cfg(test)]
mod tests {
    use super::{assert_linear, count, steps};

    #[test]
    fn linear_work_passes_and_quadratic_work_does_not() {
        let linear = |n: usize| {
            let ((), done) = steps(|| (0..n).for_each(|_| count(3)));
            (done, n)
        };
        assert_linear(100, linear, |items, bytes| 3 * items + bytes);
        let quadratic = |n: usize| {
            let ((), done) = steps(|| (0..n * n).for_each(|_| count(1)));
            (done, n)
        };
        let caught = std::panic::catch_unwind(|| assert_linear(100, quadratic, |_, _| usize::MAX));
        assert!(caught.is_err(), "quadratic work passed");
    }
}
