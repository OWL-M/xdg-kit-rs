//! Not public API: maps and sets keyed by text that hash each key's text
//! once, for the xdg-kit crates. It may change or go at any time.
//!
//! std's `HashMap` keeps only a few bits of each key's hash, so each time a
//! map grows it hashes every key it holds again: a key of `L` bytes kept
//! while `n` others arrive costs `L log n`. Here each key carries the hash
//! of its text, computed once with the map's random [`Seed`], and the maps
//! hash only that number ([`UseCached`]): growing or shrinking costs a step
//! per entry, whatever the keys' lengths.
//!
//! What a key is, how it is looked up and how its work is counted is its
//! [`Kind`]: a crate names its key types by kinds, each counting on the
//! crate's own work counter ([`Count`]). [`Str`], [`PathKey`], [`Ref`] and
//! [`Numbered`] are the common ones.
//!
//! The text of a key is hashed only when a [`Lookup`] is made (a step per
//! byte, [`Kind::bytes`]); a stored key is made from a lookup, keeping its
//! hash. Two keys compare their hashes, then, if equal, their text (a step
//! per byte compared). A map is searched by a [`Lookup`], which copies
//! nothing: a stored key and a lookup lend the same [`View`] ([`Borrow`]).

use std::borrow::Borrow;
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::hash::{BuildHasher, Hash, Hasher, RandomState};
use std::marker::PhantomData;
use std::path::{Path, PathBuf};

/// Counts steps of work on a crate's counter: a zero-sized type each
/// crate implements with its `work` module.
pub trait Count {
    /// Counts the steps `steps` works out, calling it only where steps are
    /// counted (in tests), so that working them out (measuring a rule
    /// tree, say) costs nothing elsewhere.
    fn count(steps: impl FnOnce() -> usize);
}

/// A kind of key: the type stored ([`Kind::Owned`]), the borrowed form it
/// is hashed, compared and looked up by ([`Kind::View`]), and the counter
/// its work is counted on.
///
/// A view of an owned key and a view of the same text given elsewhere must
/// hash alike and be [`Kind::same`].
pub trait Kind {
    /// What a map stores.
    type Owned;
    /// What a key is hashed, compared and looked up by.
    type View<'a>: Copy + Hash
    where
        Self: 'a;

    /// The view of a stored key.
    fn view<'a>(owned: &'a Self::Owned) -> Self::View<'a>
    where
        Self: 'a;

    /// A view for a shorter time (views are covariant: the body is the
    /// view itself).
    fn shorten<'s, 'l: 's>(view: Self::View<'l>) -> Self::View<'s>
    where
        Self: 'l;

    /// Whether two views are of the same text.
    fn same(a: Self::View<'_>, b: Self::View<'_>) -> bool;

    /// The measure of hashing, copying or comparing a view: its bytes.
    fn bytes(view: Self::View<'_>) -> usize;

    /// Counts the steps `steps` works out on the kind's counter, calling
    /// it only where steps are counted ([`Count::count`]).
    fn count(steps: impl FnOnce() -> usize);
}

/// A kind whose stored key can be copied from a view.
pub trait Copied: Kind {
    /// The stored key of `view`'s text.
    fn to_owned<'a>(view: Self::View<'a>) -> Self::Owned
    where
        Self: 'a;
}

/// The bytes of a value borrowed as a key ([`Ref`]): the measure of
/// hashing or comparing it.
pub trait Measure {
    /// The bytes hashed or compared.
    fn bytes(&self) -> usize;
}

impl Measure for str {
    fn bytes(&self) -> usize {
        self.len()
    }
}

impl Measure for Path {
    fn bytes(&self) -> usize {
        self.as_os_str().len()
    }
}

/// Text, stored as `String`, looked up by `&str`.
pub struct Str<C>(PhantomData<fn() -> C>);

impl<C: Count> Kind for Str<C> {
    type Owned = String;
    type View<'a>
        = &'a str
    where
        Self: 'a;

    fn view<'a>(owned: &'a String) -> &'a str
    where
        Self: 'a,
    {
        owned
    }

    fn shorten<'s, 'l: 's>(view: &'l str) -> &'s str
    where
        Self: 'l,
    {
        view
    }

    fn same(a: &str, b: &str) -> bool {
        a == b
    }

    fn bytes(view: &str) -> usize {
        view.len()
    }

    fn count(steps: impl FnOnce() -> usize) {
        C::count(steps);
    }
}

impl<C: Count> Copied for Str<C> {
    fn to_owned<'a>(view: &'a str) -> String
    where
        Self: 'a,
    {
        view.to_owned()
    }
}

/// A path, stored as `PathBuf`, looked up by `&Path`: hashed and compared
/// as `Path` does, by components (`/a//b` is `/a/b`), measured by its
/// bytes.
pub struct PathKey<C>(PhantomData<fn() -> C>);

impl<C: Count> Kind for PathKey<C> {
    type Owned = PathBuf;
    type View<'a>
        = &'a Path
    where
        Self: 'a;

    fn view<'a>(owned: &'a PathBuf) -> &'a Path
    where
        Self: 'a,
    {
        owned
    }

    fn shorten<'s, 'l: 's>(view: &'l Path) -> &'s Path
    where
        Self: 'l,
    {
        view
    }

    fn same(a: &Path, b: &Path) -> bool {
        a == b
    }

    fn bytes(view: &Path) -> usize {
        view.as_os_str().len()
    }

    fn count(steps: impl FnOnce() -> usize) {
        C::count(steps);
    }
}

impl<C: Count> Copied for PathKey<C> {
    fn to_owned<'a>(view: &'a Path) -> PathBuf
    where
        Self: 'a,
    {
        view.to_path_buf()
    }
}

/// A value borrowed for the life of a map, stored as `&'r T`: for sets
/// that note what was seen. Hashed and compared as `T` does, measured by
/// [`Measure`].
pub struct Ref<'r, T: ?Sized, C>(PhantomData<&'r T>, PhantomData<fn() -> C>);

impl<'r, T: ?Sized + Hash + Eq + Measure, C: Count> Kind for Ref<'r, T, C> {
    type Owned = &'r T;
    type View<'a>
        = &'a T
    where
        Self: 'a;

    fn view<'a>(owned: &'a &'r T) -> &'a T
    where
        Self: 'a,
    {
        owned
    }

    fn shorten<'s, 'l: 's>(view: &'l T) -> &'s T
    where
        Self: 'l,
    {
        view
    }

    fn same(a: &T, b: &T) -> bool {
        a == b
    }

    fn bytes(view: &T) -> usize {
        view.bytes()
    }

    fn count(steps: impl FnOnce() -> usize) {
        C::count(steps);
    }
}

/// A number and a key of kind `K`, stored as `(usize, K::Owned)`.
pub struct Numbered<K>(PhantomData<fn() -> K>);

impl<K: Kind> Kind for Numbered<K> {
    type Owned = (usize, K::Owned);
    type View<'a>
        = (usize, K::View<'a>)
    where
        Self: 'a;

    fn view<'a>(owned: &'a (usize, K::Owned)) -> (usize, K::View<'a>)
    where
        Self: 'a,
    {
        (owned.0, K::view(&owned.1))
    }

    fn shorten<'s, 'l: 's>(view: (usize, K::View<'l>)) -> (usize, K::View<'s>)
    where
        Self: 'l,
    {
        (view.0, K::shorten(view.1))
    }

    fn same(a: (usize, K::View<'_>), b: (usize, K::View<'_>)) -> bool {
        a.0 == b.0 && K::same(a.1, b.1)
    }

    fn bytes(view: (usize, K::View<'_>)) -> usize {
        K::bytes(view.1)
    }

    fn count(steps: impl FnOnce() -> usize) {
        K::count(steps);
    }
}

impl<K: Copied> Copied for Numbered<K> {
    fn to_owned<'a>(view: (usize, K::View<'a>)) -> (usize, K::Owned)
    where
        Self: 'a,
    {
        (view.0, K::to_owned(view.1))
    }
}

/// Two keys, of kinds `A` and `B`, stored as a pair.
pub struct Pair<A, B>(PhantomData<fn() -> (A, B)>);

impl<A: Kind, B: Kind> Kind for Pair<A, B> {
    type Owned = (A::Owned, B::Owned);
    type View<'a>
        = (A::View<'a>, B::View<'a>)
    where
        Self: 'a;

    fn view<'a>(owned: &'a (A::Owned, B::Owned)) -> (A::View<'a>, B::View<'a>)
    where
        Self: 'a,
    {
        (A::view(&owned.0), B::view(&owned.1))
    }

    fn shorten<'s, 'l: 's>(view: (A::View<'l>, B::View<'l>)) -> (A::View<'s>, B::View<'s>)
    where
        Self: 'l,
    {
        (A::shorten(view.0), B::shorten(view.1))
    }

    fn same(a: (A::View<'_>, B::View<'_>), b: (A::View<'_>, B::View<'_>)) -> bool {
        A::same(a.0, b.0) && B::same(a.1, b.1)
    }

    fn bytes(view: (A::View<'_>, B::View<'_>)) -> usize {
        A::bytes(view.0) + B::bytes(view.1)
    }

    fn count(steps: impl FnOnce() -> usize) {
        A::count(steps);
    }
}

impl<A: Copied, B: Copied> Copied for Pair<A, B> {
    fn to_owned<'a>(view: (A::View<'a>, B::View<'a>)) -> (A::Owned, B::Owned)
    where
        Self: 'a,
    {
        (A::to_owned(view.0), B::to_owned(view.1))
    }
}

/// A stored key: its text and the hash its map's [`Seed`] gave it.
pub struct Hashed<K: Kind> {
    hash: u64,
    owned: K::Owned,
}

impl<K: Kind> Hashed<K> {
    /// The key as stored.
    pub fn owned(&self) -> &K::Owned {
        &self.owned
    }

    /// The key as stored, taken out.
    pub fn into_owned(self) -> K::Owned {
        self.owned
    }

    /// Its measure ([`Kind::bytes`]).
    pub fn bytes(&self) -> usize {
        K::bytes(K::view(&self.owned))
    }
}

impl<K: Kind> Clone for Hashed<K>
where
    K::Owned: Clone,
{
    fn clone(&self) -> Self {
        Self {
            hash: self.hash,
            owned: self.owned.clone(),
        }
    }
}

/// Its text only: the hash, random from run to run, is left out.
impl<K: Kind> fmt::Debug for Hashed<K>
where
    K::Owned: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.owned.fmt(f)
    }
}

/// A borrowed key and its hash, made by a [`Seed`]: what a map is searched
/// with, and what a stored key is made from.
pub struct Lookup<'a, K: Kind + 'a> {
    hash: u64,
    view: K::View<'a>,
}

impl<'a, K: Kind + 'a> Clone for Lookup<'a, K> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<'a, K: Kind + 'a> Copy for Lookup<'a, K> {}

impl<'a, K: Kind + 'a> Lookup<'a, K> {
    /// A lookup of `view` with the hash `hash`, for tests that need two
    /// texts of one hash.
    #[must_use]
    pub fn with_hash(hash: u64, view: K::View<'a>) -> Self {
        Self { hash, view }
    }

    /// What a map is searched with.
    #[must_use]
    pub fn view(&self) -> &(dyn View<K> + 'a) {
        self
    }

    /// The text looked up.
    #[must_use]
    pub fn text(&self) -> K::View<'a> {
        self.view
    }

    /// Its hash.
    #[must_use]
    pub fn hash(&self) -> u64 {
        self.hash
    }

    /// Its measure ([`Kind::bytes`]).
    #[must_use]
    pub fn bytes(&self) -> usize {
        K::bytes(self.view)
    }

    /// The stored key for this lookup: the text copied (a step per byte),
    /// the hash kept.
    #[must_use]
    pub fn to_owned(&self) -> Hashed<K>
    where
        K: Copied,
    {
        K::count(|| self.bytes());
        Hashed {
            hash: self.hash,
            owned: K::to_owned(self.view),
        }
    }
}

/// A key of a map keyed by [`Hashed`] text, stored or looked up: its hash
/// and its text. The two forms hash and compare alike.
pub trait View<K: Kind> {
    /// The hash.
    fn cached(&self) -> u64;
    /// The text.
    fn text(&self) -> K::View<'_>;
}

impl<K: Kind> View<K> for Hashed<K> {
    fn cached(&self) -> u64 {
        self.hash
    }

    fn text(&self) -> K::View<'_> {
        K::view(&self.owned)
    }
}

impl<'a, K: Kind + 'a> View<K> for Lookup<'a, K> {
    fn cached(&self) -> u64 {
        self.hash
    }

    fn text(&self) -> K::View<'_> {
        K::shorten(self.view)
    }
}

impl<K: Kind> Hash for dyn View<K> + '_ {
    fn hash<H: Hasher>(&self, state: &mut H) {
        state.write_u64(self.cached());
    }
}

/// Hashes first; texts only when the hashes match, a step per byte
/// compared.
impl<K: Kind> PartialEq for dyn View<K> + '_ {
    fn eq(&self, other: &Self) -> bool {
        self.cached() == other.cached() && {
            let (text, others) = (self.text(), other.text());
            K::count(|| K::bytes(text).min(K::bytes(others)));
            K::same(text, others)
        }
    }
}

impl<K: Kind> Eq for dyn View<K> + '_ {}

/// The stored key lends its view, so that a map keyed by it is searched
/// by a [`Lookup`].
impl<'a, K: Kind + 'a> Borrow<dyn View<K> + 'a> for Hashed<K> {
    fn borrow(&self) -> &(dyn View<K> + 'a) {
        self
    }
}

/// As its view hashes.
impl<K: Kind> Hash for Hashed<K> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        state.write_u64(self.hash);
    }
}

/// As its view compares.
impl<K: Kind> PartialEq for Hashed<K> {
    fn eq(&self, other: &Self) -> bool {
        let (view, other): (&dyn View<K>, &dyn View<K>) = (self, other);
        view == other
    }
}

impl<K: Kind> Eq for Hashed<K> {}

/// A map's random hash seed: names that collide cannot be chosen in
/// advance. A clone hashes as the original does, so a map's clone finds
/// the keys it copied.
#[derive(Debug, Clone, Default)]
pub struct Seed(RandomState);

impl Seed {
    /// A lookup of `view`: its text hashed, a step per byte. The only place
    /// a key's text is hashed.
    #[must_use]
    pub fn lookup<'a, K: Kind + 'a>(&self, view: K::View<'a>) -> Lookup<'a, K> {
        K::count(|| K::bytes(view));
        Lookup {
            hash: self.0.hash_one(view),
            view,
        }
    }

    /// The stored key of `owned`, hashed as a lookup of its view is.
    #[must_use]
    pub fn key<K: Kind>(&self, owned: K::Owned) -> Hashed<K> {
        let hash = self.lookup::<K>(K::view(&owned)).hash;
        Hashed { hash, owned }
    }
}

/// Builds the hashers of maps keyed by [`Hashed`] text: each takes the
/// key's hash as it is.
#[derive(Debug, Clone, Copy, Default)]
pub struct UseCached;

impl BuildHasher for UseCached {
    type Hasher = Cached;

    fn build_hasher(&self) -> Cached {
        Cached(0)
    }
}

/// Takes the single number a [`Hashed`] key's view writes as the hash.
/// Writing anything else is a bug, caught by a debug assertion; outside
/// debug builds it is folded into the hash.
#[derive(Debug)]
pub struct Cached(u64);

#[cfg(any(test, feature = "__count-work"))]
thread_local! {
    /// The bytes written to [`Cached`] hashers on this thread.
    #[allow(
        clippy::missing_const_for_thread_local,
        reason = "already const; clippy flags it on targets without native thread locals (Android, OpenBSD, illumos)"
    )]
    static FED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// The bytes written to the hashers of maps keyed by [`Hashed`] text on
/// this thread so far: eight per key hashed, whatever its text.
#[cfg(any(test, feature = "__count-work"))]
#[must_use]
pub fn fed() -> usize {
    FED.with(std::cell::Cell::get)
}

impl Hasher for Cached {
    fn finish(&self) -> u64 {
        self.0
    }

    fn write(&mut self, bytes: &[u8]) {
        debug_assert!(
            false,
            "only a cached hash is written to the hasher of a map keyed by text"
        );
        for &byte in bytes {
            self.0 = self.0.rotate_left(8) ^ u64::from(byte);
        }
    }

    fn write_u64(&mut self, hash: u64) {
        #[cfg(any(test, feature = "__count-work"))]
        FED.with(|fed| fed.set(fed.get() + 8));
        // The hash itself when it is the first thing written.
        self.0 = self.0.rotate_left(32) ^ hash;
    }
}

/// A map keyed by [`Hashed`] text, without its seed (for several maps that
/// share one).
pub type ByText<K, V> = HashMap<Hashed<K>, V, UseCached>;

/// A map keyed by text of kind `K`, with its own [`Seed`].
pub struct Map<K: Kind, V> {
    seed: Seed,
    map: ByText<K, V>,
}

impl<K: Kind, V> Default for Map<K, V> {
    fn default() -> Self {
        Self {
            seed: Seed::default(),
            map: HashMap::default(),
        }
    }
}

impl<K: Kind, V: Clone> Clone for Map<K, V>
where
    K::Owned: Clone,
{
    fn clone(&self) -> Self {
        Self {
            seed: self.seed.clone(),
            map: self.map.clone(),
        }
    }
}

/// The same keys with equal values. Each map hashes with its own seed, so
/// each key is looked up in the other by its text.
impl<K: Kind, V: PartialEq> PartialEq for Map<K, V> {
    fn eq(&self, other: &Self) -> bool {
        self.len() == other.len()
            && self
                .iter()
                .all(|(key, value)| other.get(K::view(key)) == Some(value))
    }
}

impl<K: Kind, V: fmt::Debug> fmt::Debug for Map<K, V>
where
    K::Owned: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_map().entries(self.map.iter()).finish()
    }
}

impl<K: Kind, V> Map<K, V> {
    /// An empty map.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A map with room for `capacity` keys.
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            seed: Seed::default(),
            map: HashMap::with_capacity_and_hasher(capacity, UseCached),
        }
    }

    /// A lookup of `view`, hashed with this map's seed.
    #[must_use]
    pub fn lookup<'a>(&self, view: K::View<'a>) -> Lookup<'a, K>
    where
        K: 'a,
    {
        self.seed.lookup(view)
    }

    /// The value of `view`'s key.
    #[must_use]
    pub fn get<'a>(&self, view: K::View<'a>) -> Option<&V>
    where
        K: 'a,
    {
        self.map.get(self.lookup(view).view())
    }

    /// The value of `view`'s key, to change.
    pub fn get_mut<'a>(&mut self, view: K::View<'a>) -> Option<&mut V>
    where
        K: 'a,
    {
        let lookup = self.lookup(view);
        self.map.get_mut(lookup.view())
    }

    /// Whether `view`'s key is present.
    #[must_use]
    pub fn contains_key<'a>(&self, view: K::View<'a>) -> bool
    where
        K: 'a,
    {
        self.map.contains_key(self.lookup(view).view())
    }

    /// Takes out `view`'s key's value.
    pub fn remove<'a>(&mut self, view: K::View<'a>) -> Option<V>
    where
        K: 'a,
    {
        let lookup = self.lookup(view);
        self.map.remove(lookup.view())
    }

    /// Sets the value of `view`'s key, copying the key only if it is new;
    /// returns the value replaced.
    pub fn insert<'a>(&mut self, view: K::View<'a>, value: V) -> Option<V>
    where
        K: Copied + 'a,
    {
        let lookup = self.lookup(view);
        if let Some(old) = self.map.get_mut(lookup.view()) {
            return Some(std::mem::replace(old, value));
        }
        self.map.insert(lookup.to_owned(), value);
        None
    }

    /// Sets the value of the key `owned`; returns the value replaced.
    pub fn insert_owned(&mut self, owned: K::Owned, value: V) -> Option<V> {
        let key = self.seed.key(owned);
        self.map.insert(key, value)
    }

    /// The value of `view`'s key, made by `make` if it is absent (the key
    /// then copied).
    pub fn get_or_insert_with<'a>(&mut self, view: K::View<'a>, make: impl FnOnce() -> V) -> &mut V
    where
        K: Copied + 'a,
    {
        let lookup = self.lookup(view);
        if !self.map.contains_key(lookup.view()) {
            self.map.insert(lookup.to_owned(), make());
        }
        self.map
            .get_mut(lookup.view())
            .expect("the key was just made present")
    }

    /// The value of the key `owned`, made by `make` if it is absent (`owned`
    /// then stored, else dropped).
    pub fn get_or_insert_owned(&mut self, owned: K::Owned, make: impl FnOnce() -> V) -> &mut V {
        let key = self.seed.key(owned);
        self.map.entry(key).or_insert_with(make)
    }

    /// The value of the key `lookup` (made by [`Map::lookup`]) looks up.
    #[must_use]
    pub fn get_at(&self, lookup: &Lookup<'_, K>) -> Option<&V> {
        self.map.get(lookup.view())
    }

    /// The value of the key `lookup` looks up, to change.
    pub fn get_mut_at(&mut self, lookup: &Lookup<'_, K>) -> Option<&mut V> {
        self.map.get_mut(lookup.view())
    }

    /// Whether the key `lookup` looks up is present.
    #[must_use]
    pub fn contains_at(&self, lookup: &Lookup<'_, K>) -> bool {
        self.map.contains_key(lookup.view())
    }

    /// Takes out the value of the key `lookup` looks up.
    pub fn remove_at(&mut self, lookup: &Lookup<'_, K>) -> Option<V> {
        self.map.remove(lookup.view())
    }

    /// Adds the key `lookup` looks up, which must be absent, copying its
    /// text.
    pub fn insert_at(&mut self, lookup: &Lookup<'_, K>, value: V)
    where
        K: Copied,
    {
        let old = self.map.insert(lookup.to_owned(), value);
        debug_assert!(old.is_none(), "a key added twice");
    }

    /// Adds `owned`, the text `lookup` looks up, already copied (as when
    /// its memory is asked for first): it keeps the lookup's hash. The key
    /// must be absent.
    pub fn insert_owned_at(&mut self, lookup: &Lookup<'_, K>, owned: K::Owned, value: V) {
        debug_assert!(
            K::same(K::view(&owned), lookup.text()),
            "a key stored for another text's lookup"
        );
        let key = Hashed {
            hash: lookup.hash(),
            owned,
        };
        let old = self.map.insert(key, value);
        debug_assert!(old.is_none(), "a key added twice");
    }

    /// The value of the key `lookup` looks up, made by `make` if it is
    /// absent (its text then copied).
    pub fn get_or_insert_at(&mut self, lookup: &Lookup<'_, K>, make: impl FnOnce() -> V) -> &mut V
    where
        K: Copied,
    {
        if !self.map.contains_key(lookup.view()) {
            self.map.insert(lookup.to_owned(), make());
        }
        self.map
            .get_mut(lookup.view())
            .expect("the key was just made present")
    }

    /// Asks for room for `additional` more keys.
    ///
    /// # Errors
    ///
    /// If the memory is refused.
    pub fn try_reserve(
        &mut self,
        additional: usize,
    ) -> Result<(), std::collections::TryReserveError> {
        self.map.try_reserve(additional)
    }

    /// The keys it has room for.
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.map.capacity()
    }

    /// The number of keys.
    #[must_use]
    pub fn len(&self) -> usize {
        self.map.len()
    }

    /// Whether there are no keys.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// The keys, as stored, and their values, in no order.
    pub fn iter(&self) -> impl Iterator<Item = (&K::Owned, &V)> {
        self.map.iter().map(|(key, value)| (key.owned(), value))
    }

    /// The values, in no order.
    pub fn values(&self) -> impl Iterator<Item = &V> {
        self.map.values()
    }

    /// The values, to change, in no order.
    pub fn values_mut(&mut self) -> impl Iterator<Item = &mut V> {
        self.map.values_mut()
    }

    /// Takes out every key.
    pub fn clear(&mut self) {
        self.map.clear();
    }
}

/// A set of text of kind `K`, with its own [`Seed`].
pub struct Set<K: Kind> {
    seed: Seed,
    set: HashSet<Hashed<K>, UseCached>,
}

impl<K: Kind> Default for Set<K> {
    fn default() -> Self {
        Self {
            seed: Seed::default(),
            set: HashSet::default(),
        }
    }
}

impl<K: Kind> Clone for Set<K>
where
    K::Owned: Clone,
{
    fn clone(&self) -> Self {
        Self {
            seed: self.seed.clone(),
            set: self.set.clone(),
        }
    }
}

impl<K: Kind> fmt::Debug for Set<K>
where
    K::Owned: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_set().entries(self.set.iter()).finish()
    }
}

impl<K: Kind> Set<K> {
    /// An empty set.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds `view`'s text, copying it only if it is new; whether it was
    /// new.
    pub fn insert<'a>(&mut self, view: K::View<'a>) -> bool
    where
        K: Copied + 'a,
    {
        let lookup = self.seed.lookup(view);
        if self.set.contains(lookup.view()) {
            return false;
        }
        self.set.insert(lookup.to_owned())
    }

    /// Adds `owned`; whether it was new.
    pub fn insert_owned(&mut self, owned: K::Owned) -> bool {
        let key = self.seed.key(owned);
        self.set.insert(key)
    }

    /// Whether `view`'s text is present.
    #[must_use]
    pub fn contains<'a>(&self, view: K::View<'a>) -> bool
    where
        K: 'a,
    {
        self.set.contains(self.seed.lookup(view).view())
    }

    /// Takes out `view`'s text; whether it was present.
    pub fn remove<'a>(&mut self, view: K::View<'a>) -> bool
    where
        K: 'a,
    {
        let lookup = self.seed.lookup(view);
        self.set.remove(lookup.view())
    }

    /// A set with room for `capacity` texts.
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            seed: Seed::default(),
            set: HashSet::with_capacity_and_hasher(capacity, UseCached),
        }
    }

    /// The number of texts.
    #[must_use]
    pub fn len(&self) -> usize {
        self.set.len()
    }

    /// Whether there are none.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.set.is_empty()
    }

    /// The texts, as stored, in no order.
    pub fn iter(&self) -> impl Iterator<Item = &K::Owned> {
        self.set.iter().map(Hashed::owned)
    }

    /// Takes out every text.
    pub fn clear(&mut self) {
        self.set.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::work;

    struct Counted;

    impl Count for Counted {
        fn count(steps: impl FnOnce() -> usize) {
            work::count(steps());
        }
    }

    type Text = Str<Counted>;

    #[test]
    fn a_stored_key_and_a_lookup_of_its_text_agree() {
        let seed = Seed::default();
        let owned = String::from("Desktop Entry");
        let key = seed.lookup::<Text>("Desktop Entry").to_owned();
        let same = seed.lookup::<Text>(&owned);
        let other = seed.lookup::<Text>("Desktop Action");
        let hash = |view: &dyn View<Text>| UseCached.hash_one(view);
        assert_eq!(UseCached.hash_one(&key), hash(same.view()));
        assert!(Borrow::<dyn View<Text>>::borrow(&key) == same.view());
        assert_eq!(key, same.to_owned());
        assert!(Borrow::<dyn View<Text>>::borrow(&key) != other.view());
        assert_eq!(seed.key::<Text>("Desktop Entry".into()), key);
        // A clone of the seed hashes as the seed does.
        assert_eq!(seed.clone().lookup::<Text>(&owned).hash(), same.hash());
        let map: ByText<Text, usize> = HashMap::from_iter([(key, 0)]);
        assert!(map.contains_key(same.view()));
        assert!(!map.contains_key(other.view()));
    }

    #[test]
    fn texts_of_one_hash_stay_apart_and_are_found_by_their_text() {
        // Equal hashes do not make texts equal: each is kept, and found by
        // comparing its text, with a step per byte compared.
        let lookup = |text: &'static str| Lookup::<Text>::with_hash(7, text);
        let mut map: ByText<Text, usize> = HashMap::default();
        for (at, text) in ["first", "second", "third"].into_iter().enumerate() {
            assert!(map.insert(lookup(text).to_owned(), at).is_none());
        }
        assert_eq!(map.len(), 3);
        for (at, text) in ["first", "second", "third"].into_iter().enumerate() {
            assert_eq!(map.get(lookup(text).view()), Some(&at));
        }
        assert_eq!(map.get(lookup("fourth").view()), None);
        let lone: ByText<Text, usize> = HashMap::from_iter([(lookup("first").to_owned(), 0)]);
        let (found, steps) = work::steps(|| lone.contains_key(lookup("first").view()));
        assert!(found);
        assert_eq!(steps, "first".len());
    }

    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "only a cached hash is written")]
    fn the_maps_hasher_takes_only_a_cached_hash() {
        UseCached.build_hasher().write(b"text");
    }

    #[test]
    fn growing_a_map_beside_a_long_key_never_hashes_its_text() {
        // A key of 4 MiB kept while 2^16 short ones arrive: the map grows
        // sixteen times, and its hasher is fed eight bytes per key moved,
        // never the text. Hashing the long key's text once is the only
        // step that counts its bytes.
        let long = "x".repeat(4 << 20);
        let shorts: Vec<String> = (0..1 << 16).map(|n| n.to_string()).collect();
        let short_bytes: usize = shorts.iter().map(String::len).sum();
        let before = fed();
        let (map, steps) = work::steps(|| {
            let mut map = Map::<Text, usize>::new();
            map.insert(&long, 0);
            for (at, short) in shorts.iter().enumerate() {
                map.insert(short, at);
            }
            map
        });
        let fed = fed() - before;
        assert_eq!(map.len(), 1 + shorts.len());
        assert!(fed.is_multiple_of(8), "{fed} bytes");
        assert!(
            fed < long.len(),
            "{fed} bytes fed beside a key of {} bytes",
            long.len()
        );
        // Hashed and copied once each: the long key, and each short one.
        assert_eq!(steps, 2 * (long.len() + short_bytes));
        assert_eq!(map.get(&long), Some(&0));
    }

    #[test]
    fn keys_are_measured_only_where_work_is_counted() {
        // Outside tests nothing is counted, so nothing is measured: a rule
        // tree, say, is not walked for a count that goes nowhere.
        struct Silent;

        impl Count for Silent {
            fn count(_: impl FnOnce() -> usize) {}
        }

        #[derive(Hash, PartialEq, Eq)]
        struct Unmeasured(u8);

        impl Measure for Unmeasured {
            fn bytes(&self) -> usize {
                panic!("measured where nothing is counted")
            }
        }

        let items = [Unmeasured(1), Unmeasured(1), Unmeasured(2)];
        let mut set = Set::<Ref<'_, Unmeasured, Silent>>::new();
        assert!(set.insert_owned(&items[0]));
        // Equal hashes: compared, still unmeasured.
        assert!(!set.insert_owned(&items[1]));
        assert!(set.insert_owned(&items[2]));
        assert!(set.contains(&items[1]));
        let mut map = Map::<Str<Silent>, usize>::new();
        assert_eq!(map.insert("a", 1), None);
        assert_eq!(map.insert("a", 2), Some(1));
    }

    #[test]
    fn maps_and_sets_find_what_they_hold() {
        let mut map = Map::<Text, usize>::new();
        assert_eq!(map.insert("a", 1), None);
        assert_eq!(map.insert("a", 2), Some(1));
        *map.get_or_insert_with("b", || 0) += 5;
        assert_eq!(map.get("b"), Some(&5));
        assert!(map.contains_key("a"));
        assert_eq!(map.remove("a"), Some(2));
        assert!(!map.contains_key("a"));
        assert_eq!(map.insert_owned("c".into(), 3), None);
        assert_eq!(map.get("c"), Some(&3));
        let copy = map.clone();
        assert_eq!(copy.get("c"), Some(&3));
        assert_eq!(copy.len(), 2);

        let mut set = Set::<Text>::new();
        assert!(set.insert("a"));
        assert!(!set.insert("a"));
        assert!(set.insert_owned("b".into()));
        assert!(set.contains("b"));
        assert!(set.remove("b"));
        assert!(!set.contains("b"));

        let paths = ["/a//b", "/a/b"];
        let mut seen = Set::<PathKey<Counted>>::new();
        assert!(seen.insert(Path::new(paths[0])));
        assert!(!seen.insert(Path::new(paths[1])), "compared as `Path` does");

        let names = [String::from("x"), String::from("x")];
        let mut borrowed = Set::<Ref<'_, str, Counted>>::new();
        assert!(borrowed.insert_owned(&names[0]));
        assert!(!borrowed.insert_owned(&names[1]));

        let mut numbered = Set::<Numbered<Text>>::new();
        assert!(numbered.insert((1, "x")));
        assert!(numbered.insert((2, "x")));
        assert!(!numbered.insert((1, "x")));
    }
}
