//! Implementation using ordered keys and exponential search.

mod consumer;
mod tests;

pub use consumer::{OrderedLayerConsumer, OrderedLayerValues};

use crate::{
    algebra::{AddAssignByRef, AddByRef, NegByRef},
    trace::layers::{
        advance, column_layer::ColumnLayer, Builder, Cursor, MergeBuilder, OrdOffset, Trie,
        TupleBuilder,
    },
    utils::{assume, cast_uninit_vec},
    DBData, NumEntries,
};
use size_of::SizeOf;
use std::{
    cmp::{min, Ordering},
    fmt::{Debug, Display, Formatter},
    marker::PhantomData,
    mem::MaybeUninit,
    ops::{Add, AddAssign, Neg},
};
use textwrap::indent;

/// A level of the trie, with keys and offsets into a lower layer.
///
/// In this representation, the values for `keys[i]` are found at `vals[offs[i]
/// .. offs[i+1]]`.
// False positive from clippy
#[allow(unknown_lints, clippy::derive_partial_eq_without_eq)]
#[derive(Debug, SizeOf, PartialEq, Eq, Clone)]
pub struct OrderedLayer<K, L, O = usize> {
    /// The keys of the layer.
    pub(crate) keys: Vec<K>,
    /// The offsets associate with each key.
    ///
    /// The bounds for `keys[i]` are `(offs[i], offs[i+1]`). The offset array is
    /// guaranteed to be one element longer than the keys array, ensuring
    /// that these accesses do not panic.
    pub(crate) offs: Vec<O>,
    /// The ranges of values associated with the keys.
    pub(crate) vals: L,
}

impl<K, L, O> OrderedLayer<K, L, O> {
    /// Break down an `OrderedLayer` into its constituent parts
    pub fn into_parts(self) -> (Vec<K>, Vec<O>, L) {
        (self.keys, self.offs, self.vals)
    }

    /// Create a new `OrderedLayer` from its component parts
    ///
    /// # Safety
    ///
    /// - `keys` must have a length of `offs.len() + 1` and every offset within
    ///   `offs` must be a valid value index into `vals`.
    /// - Every key's offset range must also be a valid range within `vals`
    /// - Every value range must be non-empty
    pub unsafe fn from_parts(keys: Vec<K>, offs: Vec<O>, vals: L) -> Self {
        // TODO: Maybe validate indices into `vals` when debug assertions are enabled
        // TODO: Maybe validate that value ranges are all valid
        debug_assert_eq!(keys.len() + 1, offs.len());

        Self { keys, offs, vals }
    }

    /// Assume the invariants of the current builder
    ///
    /// # Safety
    ///
    /// Requires that `offs` has a length of `keys + 1`
    unsafe fn assume_invariants(&self) {
        assume(self.offs.len() == self.keys.len() + 1)
    }
}

impl<K, V, R, O> OrderedLayer<K, ColumnLayer<V, R>, O> {
    /// Turns the current `OrderedLayer<K, ColumnLayer<V, R>, O>` into a
    /// layer of [`MaybeUninit`] values
    fn into_uninit(
        self,
    ) -> OrderedLayer<MaybeUninit<K>, ColumnLayer<MaybeUninit<V>, MaybeUninit<R>>, O> {
        unsafe {
            self.assume_invariants();
            self.vals.assume_invariants();
        }

        OrderedLayer {
            keys: cast_uninit_vec(self.keys),
            offs: self.offs,
            vals: self.vals.into_uninit(),
        }
    }
}

impl<K, L, O> NumEntries for OrderedLayer<K, L, O>
where
    K: Ord + Clone,
    L: Trie,
    O: OrdOffset,
{
    const CONST_NUM_ENTRIES: Option<usize> = None;

    fn num_entries_shallow(&self) -> usize {
        self.keys()
    }

    fn num_entries_deep(&self) -> usize {
        self.tuples()
    }
}

impl<K, L, O> NegByRef for OrderedLayer<K, L, O>
where
    K: Ord + Clone,
    L: Trie + NegByRef,
    O: OrdOffset,
{
    fn neg_by_ref(&self) -> Self {
        Self {
            keys: self.keys.clone(),
            offs: self.offs.clone(),
            // We assume that offsets in `vals` don't change after negation;
            // otherwise `self.offs` will be invalid.
            vals: self.vals.neg_by_ref(),
        }
    }
}

impl<K, L, O> Neg for OrderedLayer<K, L, O>
where
    K: Ord + Clone,
    L: Trie + Neg<Output = L>,
    O: OrdOffset,
{
    type Output = Self;

    fn neg(self) -> Self {
        Self {
            keys: self.keys,
            offs: self.offs,
            // We assume that offsets in `vals` don't change after negation;
            // otherwise `self.offs` will be invalid.
            vals: self.vals.neg(),
        }
    }
}

// TODO: by-value merge
impl<K, L, O> Add<Self> for OrderedLayer<K, L, O>
where
    K: Ord + Clone,
    L: Trie,
    O: OrdOffset,
{
    type Output = Self;

    // TODO: In-place merge
    fn add(self, rhs: Self) -> Self::Output {
        if self.is_empty() {
            rhs
        } else if rhs.is_empty() {
            self
        } else {
            self.merge(&rhs)
        }
    }
}

impl<K, L, O> AddAssign<Self> for OrderedLayer<K, L, O>
where
    K: Ord + Clone,
    L: Trie,
    O: OrdOffset,
{
    // TODO: In-place merge
    fn add_assign(&mut self, rhs: Self) {
        if self.is_empty() {
            *self = rhs;
        } else if !rhs.is_empty() {
            *self = self.merge(&rhs);
        }
    }
}

impl<K, L, O> AddAssignByRef for OrderedLayer<K, L, O>
where
    K: Ord + Clone,
    L: Trie,
    O: OrdOffset,
{
    fn add_assign_by_ref(&mut self, other: &Self) {
        if !other.is_empty() {
            *self = self.merge(other);
        }
    }
}

impl<K, L, O> AddByRef for OrderedLayer<K, L, O>
where
    K: Ord + Clone,
    L: Trie,
    O: OrdOffset,
{
    fn add_by_ref(&self, rhs: &Self) -> Self {
        self.merge(rhs)
    }
}

impl<K, L, O> Trie for OrderedLayer<K, L, O>
where
    K: Ord + Clone,
    L: Trie,
    O: OrdOffset,
{
    type Item = (K, L::Item);
    type Cursor<'s> = OrderedCursor<'s, K, O, L> where K: 's, O: 's, L: 's;
    type MergeBuilder = OrderedBuilder<K, L::MergeBuilder, O>;
    type TupleBuilder = UnorderedBuilder<K, L::TupleBuilder, O>;

    fn keys(&self) -> usize {
        unsafe { self.assume_invariants() }
        self.keys.len()
    }

    fn tuples(&self) -> usize {
        unsafe { self.assume_invariants() }
        self.vals.tuples()
    }

    fn cursor_from(&self, lower: usize, upper: usize) -> Self::Cursor<'_> {
        unsafe { self.assume_invariants() }

        if lower < upper {
            let child_lower = self.offs[lower];
            let child_upper = self.offs[lower + 1];

            OrderedCursor {
                bounds: (lower, upper),
                storage: self,
                child: self
                    .vals
                    .cursor_from(child_lower.into_usize(), child_upper.into_usize()),
                pos: lower,
            }
        } else {
            OrderedCursor {
                bounds: (0, 0),
                storage: self,
                child: self.vals.cursor_from(0, 0),
                pos: 0,
            }
        }
    }
}

impl<K, L, O> Default for OrderedLayer<K, L, O>
where
    O: OrdOffset,
    L: Default,
{
    fn default() -> Self {
        Self {
            keys: Vec::new(),
            // `offs.len()` **must** be `keys.len() + 1`
            offs: vec![O::zero()],
            vals: L::default(),
        }
    }
}

impl<K, L, O> Display for OrderedLayer<K, L, O>
where
    K: DBData,
    L: Trie,
    for<'a> L::Cursor<'a>: Clone + Display,
    O: OrdOffset,
{
    fn fmt(&self, f: &mut Formatter<'_>) -> Result<(), std::fmt::Error> {
        self.cursor().fmt(f)
    }
}

/// Assembles a layer of this
#[derive(SizeOf)]
pub struct OrderedBuilder<K, L, O = usize> {
    /// Keys
    pub keys: Vec<K>,
    /// Offsets
    pub offs: Vec<O>,
    /// The next layer down
    pub vals: L,
}

impl<K, L, O> OrderedBuilder<K, L, O> {
    /// Performs one step of merging.
    pub fn merge_step(
        &mut self,
        (trie1, lower1, upper1): (&<Self as Builder>::Trie, &mut usize, usize),
        (trie2, lower2, upper2): (&<Self as Builder>::Trie, &mut usize, usize),
    ) where
        K: Ord + Clone,
        L: MergeBuilder,
        O: OrdOffset,
    {
        match trie1.keys[*lower1].cmp(&trie2.keys[*lower2]) {
            Ordering::Less => {
                // determine how far we can advance lower1 until we reach/pass lower2
                let step = 1 + advance(&trie1.keys[(1 + *lower1)..upper1], |x| {
                    x < &trie2.keys[*lower2]
                });
                let step = min(step, 1_000);
                self.copy_range(trie1, *lower1, *lower1 + step);
                *lower1 += step;
            }

            Ordering::Equal => {
                let lower = self.vals.boundary();
                // record vals_length so we can tell if anything was pushed.
                let upper = self.vals.push_merge(
                    trie1.vals.cursor_from(
                        trie1.offs[*lower1].into_usize(),
                        trie1.offs[*lower1 + 1].into_usize(),
                    ),
                    trie2.vals.cursor_from(
                        trie2.offs[*lower2].into_usize(),
                        trie2.offs[*lower2 + 1].into_usize(),
                    ),
                );
                if upper > lower {
                    self.keys.push(trie1.keys[*lower1].clone());
                    self.offs.push(O::from_usize(upper));
                }

                *lower1 += 1;
                *lower2 += 1;
            }

            Ordering::Greater => {
                // determine how far we can advance lower2 until we reach/pass lower1
                let step = 1 + advance(&trie2.keys[(1 + *lower2)..upper2], |x| {
                    x < &trie1.keys[*lower1]
                });
                let step = min(step, 1_000);
                self.copy_range(trie2, *lower2, *lower2 + step);
                *lower2 += step;
            }
        }
    }

    /// Push a key and all of its associated values to the current builder
    ///
    /// Can be more efficient than repeatedly calling `.push_tuple()` because it
    /// doesn't require an owned key for every value+diff pair which can allow
    /// eliding unnecessary clones
    pub fn with_key<F>(&mut self, key: K, with: F)
    where
        K: Eq,
        L: TupleBuilder,
        O: OrdOffset,
        F: for<'a> FnOnce(OrderedBuilderVals<'a, K, L, O>),
    {
        let mut pushes = 0;
        let vals = OrderedBuilderVals {
            builder: self,
            pushes: &mut pushes,
        };
        with(vals);

        // If the user's closure actually added any elements, push the key
        if pushes != 0
            && (self.keys.is_empty()
                || !self.offs[self.keys.len()].is_zero()
                || self.keys[self.keys.len() - 1] != key)
        {
            if !self.keys.is_empty() && self.offs[self.keys.len()].is_zero() {
                self.offs[self.keys.len()] = O::from_usize(self.vals.boundary());
            }
            self.keys.push(key);
            self.offs.push(O::zero());
        }
    }
}

pub struct OrderedBuilderVals<'a, K, L, O> {
    builder: &'a mut OrderedBuilder<K, L, O>,
    pushes: &'a mut usize,
}

impl<'a, K, L, O> OrderedBuilderVals<'a, K, L, O>
where
    L: TupleBuilder,
{
    pub fn push(&mut self, value: <L as TupleBuilder>::Item) {
        *self.pushes += 1;
        self.builder.vals.push_tuple(value);
    }
}

impl<K, L, O> Builder for OrderedBuilder<K, L, O>
where
    K: Ord + Clone,
    L: Builder,
    O: OrdOffset,
{
    type Trie = OrderedLayer<K, L::Trie, O>;

    fn boundary(&mut self) -> usize {
        self.offs[self.keys.len()] = O::from_usize(self.vals.boundary());
        self.keys.len()
    }

    fn done(mut self) -> Self::Trie {
        if !self.keys.is_empty() && self.offs[self.keys.len()].is_zero() {
            self.offs[self.keys.len()] = O::from_usize(self.vals.boundary());
        }

        OrderedLayer {
            keys: self.keys,
            offs: self.offs,
            vals: self.vals.done(),
        }
    }
}

impl<K, L, O> MergeBuilder for OrderedBuilder<K, L, O>
where
    K: Ord + Clone,
    L: MergeBuilder,
    O: OrdOffset,
{
    fn with_capacity(other1: &Self::Trie, other2: &Self::Trie) -> Self {
        let mut offs = Vec::with_capacity(other1.keys() + other2.keys() + 1);
        offs.push(O::zero());

        Self {
            keys: Vec::with_capacity(other1.keys() + other2.keys()),
            offs,
            vals: L::with_capacity(&other1.vals, &other2.vals),
        }
    }

    fn with_key_capacity(capacity: usize) -> Self {
        let mut offs = Vec::with_capacity(capacity + 1);
        offs.push(O::zero());

        Self {
            keys: Vec::with_capacity(capacity),
            offs,
            vals: L::with_key_capacity(capacity),
        }
    }

    fn reserve(&mut self, additional: usize) {
        self.keys.reserve(additional);
        self.offs.reserve(additional);
        self.vals.reserve(additional);
    }

    fn copy_range(&mut self, other: &Self::Trie, lower: usize, upper: usize) {
        assert!(lower < upper && lower < other.offs.len() && upper < other.offs.len());

        let other_basis = other.offs[lower];
        let self_basis = self.offs.last().copied().unwrap_or_else(|| O::zero());

        self.keys.extend_from_slice(&other.keys[lower..upper]);
        for index in lower..upper {
            self.offs
                .push((other.offs[index + 1] + self_basis) - other_basis);
        }

        self.vals.copy_range(
            &other.vals,
            other_basis.into_usize(),
            other.offs[upper].into_usize(),
        );
    }

    fn push_merge<'a>(
        &'a mut self,
        cursor1: <Self::Trie as Trie>::Cursor<'a>,
        cursor2: <Self::Trie as Trie>::Cursor<'a>,
    ) -> usize {
        let (mut lower1, upper1) = cursor1.bounds;
        let (mut lower2, upper2) = cursor2.bounds;

        let capacity = (upper1 - lower1) + (upper2 - lower2);
        self.keys.reserve(capacity);
        self.offs.reserve(capacity);

        // while both mergees are still active
        while lower1 < upper1 && lower2 < upper2 {
            self.merge_step(
                (cursor1.storage, &mut lower1, upper1),
                (cursor2.storage, &mut lower2, upper2),
            );
        }

        if lower1 < upper1 {
            self.copy_range(cursor1.storage, lower1, upper1);
        }
        if lower2 < upper2 {
            self.copy_range(cursor2.storage, lower2, upper2);
        }

        self.keys.len()
    }
}

impl<K, L, O> TupleBuilder for OrderedBuilder<K, L, O>
where
    K: Ord + Clone,
    L: TupleBuilder,
    O: OrdOffset,
{
    type Item = (K, L::Item);

    fn new() -> Self {
        Self {
            keys: Vec::new(),
            offs: vec![O::zero()],
            vals: L::new(),
        }
    }

    fn with_capacity(cap: usize) -> Self {
        let mut offs = Vec::with_capacity(cap + 1);
        offs.push(O::zero());

        Self {
            keys: Vec::with_capacity(cap),
            offs,
            vals: L::with_capacity(cap),
        }
    }

    fn reserve_tuples(&mut self, additional: usize) {
        self.keys.reserve(additional);
        self.offs.reserve(additional);
        self.vals.reserve_tuples(additional);
    }

    fn tuples(&self) -> usize {
        self.vals.tuples()
    }

    fn push_tuple(&mut self, (key, val): (K, L::Item)) {
        // if first element, prior element finish, or different element, need to push
        // and maybe punctuate.
        if self.keys.is_empty()
            || !self.offs[self.keys.len()].is_zero()
            || self.keys[self.keys.len() - 1] != key
        {
            if !self.keys.is_empty() && self.offs[self.keys.len()].is_zero() {
                self.offs[self.keys.len()] = O::from_usize(self.vals.boundary());
            }
            self.keys.push(key);
            self.offs.push(O::zero()); // <-- indicates "unfinished".
        }
        self.vals.push_tuple(val);
    }
}

pub struct UnorderedBuilder<K, L, O = usize>
where
    K: Ord,
    L: TupleBuilder,
    O: OrdOffset,
{
    pub vals: Vec<(K, L::Item)>,
    _phantom: PhantomData<O>,
}

impl<K, L, O> Builder for UnorderedBuilder<K, L, O>
where
    K: Ord + Clone,
    L: TupleBuilder,
    O: OrdOffset,
{
    type Trie = OrderedLayer<K, L::Trie, O>;

    fn boundary(&mut self) -> usize {
        self.vals.len()
    }

    fn done(mut self) -> Self::Trie {
        // Don't use `sort_unstable_by_key` to avoid cloning the key.
        self.vals
            .sort_unstable_by(|(k1, _), (k2, _)| K::cmp(k1, k2));
        let mut builder = <OrderedBuilder<K, L, O> as TupleBuilder>::with_capacity(self.vals.len());

        for (k, v) in self.vals.into_iter() {
            builder.push_tuple((k, v));
        }
        builder.done()
    }
}

impl<K, L, O> TupleBuilder for UnorderedBuilder<K, L, O>
where
    K: Ord + Clone,
    L: TupleBuilder,
    O: OrdOffset,
{
    type Item = (K, L::Item);

    fn new() -> Self {
        Self {
            vals: Vec::new(),
            _phantom: PhantomData,
        }
    }

    fn with_capacity(capacity: usize) -> Self {
        Self {
            vals: Vec::with_capacity(capacity),
            _phantom: PhantomData,
        }
    }

    fn reserve_tuples(&mut self, additional: usize) {
        self.vals.reserve(additional);
    }

    fn tuples(&self) -> usize {
        self.vals.len()
    }

    fn push_tuple(&mut self, kv: Self::Item) {
        self.vals.push(kv);
    }

    fn extend_tuples<I>(&mut self, tuples: I)
    where
        I: IntoIterator<Item = Self::Item>,
    {
        self.vals.extend(tuples);
    }
}

/// A cursor with a child cursor that is updated as we move.
#[derive(Debug)]
pub struct OrderedCursor<'s, K, O, L>
where
    L: Trie,
{
    storage: &'s OrderedLayer<K, L, O>,
    pos: usize,
    bounds: (usize, usize),
    /// The cursor for the trie layer below this one.
    pub child: L::Cursor<'s>,
}

impl<'s, K, O, L> Clone for OrderedCursor<'s, K, O, L>
where
    K: Ord,
    L: Trie,
    O: OrdOffset,
    L::Cursor<'s>: Clone,
{
    fn clone(&self) -> Self {
        Self {
            storage: self.storage,
            pos: self.pos,
            bounds: self.bounds,
            child: self.child.clone(),
        }
    }

    fn clone_from(&mut self, source: &Self) {
        self.storage.clone_from(&source.storage);
        self.pos.clone_from(&source.pos);
        self.bounds.clone_from(&source.bounds);
        self.child.clone_from(&source.child);
    }
}

impl<'s, K, L, O> OrderedCursor<'s, K, O, L>
where
    K: Ord,
    L: Trie,
    O: OrdOffset,
{
    pub fn seek_with<P>(&mut self, predicate: P)
    where
        P: Fn(&K) -> bool,
    {
        self.pos += advance(&self.storage.keys[self.pos..self.bounds.1], predicate);

        if self.valid() {
            self.child.reposition(
                self.storage.offs[self.pos].into_usize(),
                self.storage.offs[self.pos + 1].into_usize(),
            );
        }
    }
}

impl<'s, K, L, O> Cursor<'s> for OrderedCursor<'s, K, O, L>
where
    K: Ord,
    L: Trie,
    O: OrdOffset,
{
    type Key<'k> = &'k K
    where
        Self: 'k;
    type ValueStorage = L;

    #[inline]
    fn keys(&self) -> usize {
        self.bounds.1 - self.bounds.0
    }

    #[inline]
    fn key(&self) -> Self::Key<'s> {
        &self.storage.keys[self.pos]
    }

    fn values(&self) -> L::Cursor<'s> {
        if self.valid() {
            self.storage.vals.cursor_from(
                self.storage.offs[self.pos].into_usize(),
                self.storage.offs[self.pos + 1].into_usize(),
            )
        } else {
            self.storage.vals.cursor_from(0, 0)
        }
    }

    fn step(&mut self) {
        self.pos += 1;

        if self.valid() {
            self.child.reposition(
                self.storage.offs[self.pos].into_usize(),
                self.storage.offs[self.pos + 1].into_usize(),
            );
        } else {
            self.pos = self.bounds.1;
        }
    }

    fn seek<'a>(&mut self, key: Self::Key<'a>)
    where
        's: 'a,
    {
        self.pos += advance(&self.storage.keys[self.pos..self.bounds.1], |k| k < key);

        if self.valid() {
            self.child.reposition(
                self.storage.offs[self.pos].into_usize(),
                self.storage.offs[self.pos + 1].into_usize(),
            );
        }
    }

    fn last_key(&mut self) -> Option<Self::Key<'s>> {
        // Cursor not empty?
        if self.bounds.1 > self.bounds.0 {
            Some(&self.storage.keys[self.bounds.1 - 1])
        } else {
            None
        }
    }

    fn valid(&self) -> bool {
        self.pos < self.bounds.1
    }

    fn rewind(&mut self) {
        self.pos = self.bounds.0;

        if self.valid() {
            self.child.reposition(
                self.storage.offs[self.pos].into_usize(),
                self.storage.offs[self.pos + 1].into_usize(),
            );
        }
    }

    fn reposition(&mut self, lower: usize, upper: usize) {
        self.pos = lower;
        self.bounds = (lower, upper);

        if self.valid() {
            self.child.reposition(
                self.storage.offs[self.pos].into_usize(),
                self.storage.offs[self.pos + 1].into_usize(),
            );
        }
    }
}

impl<'a, K, L, O> Display for OrderedCursor<'a, K, O, L>
where
    K: DBData,
    L: Trie,
    L::Cursor<'a>: Clone + Display,
    O: OrdOffset,
{
    fn fmt(&self, f: &mut Formatter<'_>) -> Result<(), std::fmt::Error> {
        let mut cursor: OrderedCursor<'_, K, O, L> = self.clone();

        while cursor.valid() {
            let key = cursor.key();
            writeln!(f, "{key:?}:")?;
            let val_str = cursor.values().to_string();

            f.write_str(&indent(&val_str, "    "))?;
            cursor.step();
        }

        Ok(())
    }
}
