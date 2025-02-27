use crate::{
    circuit::{
        metadata::{MetaItem, OperatorMeta},
        operator_traits::{BinaryOperator, Operator, StrictOperator, StrictUnaryOperator},
        Circuit, ExportId, ExportStream, GlobalNodeId, OwnershipPreference, Scope, Stream,
    },
    circuit_cache_key,
    trace::{cursor::Cursor, Batch, BatchReader, Builder, Spine, Trace},
    Timestamp,
};
use size_of::SizeOf;
use std::{borrow::Cow, marker::PhantomData};

circuit_cache_key!(TraceId<B, D>(GlobalNodeId => Stream<B, D>));
circuit_cache_key!(DelayedTraceId<B, D>(GlobalNodeId => Stream<B, D>));
circuit_cache_key!(IntegrateTraceId<B, D>(GlobalNodeId => Stream<B, D>));

// TODO: add infrastructure to compact the trace during slack time.

/// Add `timestamp` to all tuples in the input batch.
///
/// Given an input batch without timing information (`BatchReader::Time = ()`),
/// generate an output batch by adding the same time `timestamp` to
/// each tuple.
///
/// Most DBSP operators output untimed batches.  When such a batch is
/// added to a trace, the current timestamp must be added to it.
// TODO: this can be implemented more efficiently by having a special batch type
// where all updates have the same timestamp, as this is the only kind of
// batch that we ever create directly in DBSP; batches with multiple timestamps
// are only created as a result of merging.  The main complication is that
// we will need to extend the trace implementation to work with batches of
// multiple types.  This shouldn't be too hard and is on the todo list.
fn batch_add_time<BI, TS, BO>(batch: &BI, timestamp: &TS) -> BO
where
    TS: Timestamp,
    BI: BatchReader<Time = ()>,
    BI::Key: Clone,
    BI::Val: Clone,
    BO: Batch<Key = BI::Key, Val = BI::Val, Time = TS, R = BI::R>,
{
    let mut builder = BO::Builder::with_capacity(timestamp.clone(), batch.len());
    let mut cursor = batch.cursor();
    while cursor.key_valid() {
        while cursor.val_valid() {
            let val = cursor.val().clone();
            let w = cursor.weight();
            builder.push((BO::item_from(cursor.key().clone(), val), w.clone()));
            cursor.step_val();
        }
        cursor.step_key();
    }
    builder.done()
}

impl<P, B> Stream<Circuit<P>, B>
where
    P: Clone + 'static,
    B: Clone + 'static,
{
    // TODO: derive timestamp type from the parent circuit.

    /// Record batches in `self` in a trace.
    ///
    /// This operator labels each untimed batch in the stream with the current
    /// timestamp and adds it to a trace.  
    pub fn trace<T>(&self) -> Stream<Circuit<P>, T>
    where
        B: BatchReader<Time = ()>,
        T: Trace<Key = B::Key, Val = B::Val, R = B::R> + Clone,
    {
        self.circuit()
            .cache_get_or_insert_with(TraceId::new(self.origin_node_id().clone()), || {
                let circuit = self.circuit();

                circuit.region("trace", || {
                    let (ExportStream { local, export }, z1feedback) =
                        circuit.add_feedback_with_export(Z1Trace::new(false, circuit.root_scope()));
                    let trace = circuit.add_binary_operator_with_preference(
                        <TraceAppend<T, B>>::new(),
                        (&local, OwnershipPreference::STRONGLY_PREFER_OWNED),
                        (
                            &self.try_sharded_version(),
                            OwnershipPreference::PREFER_OWNED,
                        ),
                    );
                    if self.has_sharded_version() {
                        local.mark_sharded();
                        trace.mark_sharded();
                    }
                    z1feedback.connect_with_preference(
                        &trace,
                        OwnershipPreference::STRONGLY_PREFER_OWNED,
                    );

                    circuit
                        .cache_insert(DelayedTraceId::new(trace.origin_node_id().clone()), local);
                    circuit.cache_insert(ExportId::new(trace.origin_node_id().clone()), export);
                    trace
                })
            })
            .clone()
    }

    // TODO: this method should replace `Stream::integrate()`.
    #[track_caller]
    pub fn integrate_trace(&self) -> Stream<Circuit<P>, Spine<B>>
    where
        B: Batch,
        Spine<B>: SizeOf,
    {
        self.circuit()
            .cache_get_or_insert_with(IntegrateTraceId::new(self.origin_node_id().clone()), || {
                let circuit = self.circuit();

                circuit.region("integrate_trace", || {
                    let (ExportStream { local, export }, z1feedback) =
                        circuit.add_feedback_with_export(Z1Trace::new(true, circuit.root_scope()));

                    let trace = circuit.add_binary_operator_with_preference(
                        UntimedTraceAppend::<Spine<B>>::new(),
                        (&local, OwnershipPreference::STRONGLY_PREFER_OWNED),
                        (
                            &self.try_sharded_version(),
                            OwnershipPreference::PREFER_OWNED,
                        ),
                    );

                    if self.has_sharded_version() {
                        local.mark_sharded();
                        trace.mark_sharded();
                    }

                    z1feedback.connect_with_preference(
                        &trace,
                        OwnershipPreference::STRONGLY_PREFER_OWNED,
                    );

                    circuit
                        .cache_insert(DelayedTraceId::new(trace.origin_node_id().clone()), local);
                    circuit.cache_insert(ExportId::new(trace.origin_node_id().clone()), export);

                    trace
                })
            })
            .clone()
    }
}

impl<P, T> Stream<Circuit<P>, T>
where
    P: Clone + 'static,
    T: Trace + 'static,
{
    pub fn delay_trace(&self) -> Stream<Circuit<P>, T> {
        // The delayed trace should be automatically created while the real trace is
        // created via `.trace()` or a similar function
        // FIXME: Create a trace if it doesn't exist
        self.circuit()
            .cache_get_or_insert_with(DelayedTraceId::new(self.origin_node_id().clone()), || {
                panic!("called `.delay_trace()` on a stream without a previously created trace")
            })
            .clone()
    }
}

pub struct UntimedTraceAppend<T>
where
    T: Trace,
{
    _phantom: PhantomData<T>,
}

impl<T> UntimedTraceAppend<T>
where
    T: Trace,
{
    pub fn new() -> Self {
        Self {
            _phantom: PhantomData,
        }
    }
}

impl<T> Default for UntimedTraceAppend<T>
where
    T: Trace,
{
    fn default() -> Self {
        Self::new()
    }
}

impl<T> Operator for UntimedTraceAppend<T>
where
    T: Trace + 'static,
{
    fn name(&self) -> Cow<'static, str> {
        Cow::from("UntimedTraceAppend")
    }
    fn fixedpoint(&self, _scope: Scope) -> bool {
        true
    }
}

impl<T> BinaryOperator<T, T::Batch, T> for UntimedTraceAppend<T>
where
    T: Trace + 'static,
{
    fn eval(&mut self, _trace: &T, _batch: &T::Batch) -> T {
        // Refuse to accept trace by reference.  This should not happen in a correctly
        // constructed circuit.
        panic!("UntimedTraceAppend::eval(): cannot accept trace by reference")
    }

    fn eval_owned_and_ref(&mut self, mut trace: T, batch: &T::Batch) -> T {
        trace.insert(batch.clone());
        trace
    }

    fn eval_ref_and_owned(&mut self, _trace: &T, _batch: T::Batch) -> T {
        // Refuse to accept trace by reference.  This should not happen in a correctly
        // constructed circuit.
        panic!("UntimedTraceAppend::eval_ref_and_owned(): cannot accept trace by reference")
    }

    fn eval_owned(&mut self, mut trace: T, batch: T::Batch) -> T {
        trace.insert(batch);
        trace
    }

    fn input_preference(&self) -> (OwnershipPreference, OwnershipPreference) {
        (
            OwnershipPreference::PREFER_OWNED,
            OwnershipPreference::PREFER_OWNED,
        )
    }
}

pub struct TraceAppend<T, B>
where
    T: Trace,
{
    time: T::Time,
    _phantom: PhantomData<B>,
}

impl<T, B> TraceAppend<T, B>
where
    T: Trace,
{
    pub fn new() -> Self {
        Self {
            time: T::Time::clock_start(),
            _phantom: PhantomData,
        }
    }
}

impl<T, B> Default for TraceAppend<T, B>
where
    T: Trace,
{
    fn default() -> Self {
        Self::new()
    }
}

impl<T, B> Operator for TraceAppend<T, B>
where
    T: Trace + 'static,
    B: 'static,
{
    fn name(&self) -> Cow<'static, str> {
        Cow::from("TraceAppend")
    }
    fn clock_end(&mut self, scope: Scope) {
        self.time = self.time.advance(scope + 1);
    }
    fn fixedpoint(&self, _scope: Scope) -> bool {
        true
    }
}

impl<T, B> BinaryOperator<T, B, T> for TraceAppend<T, B>
where
    B: BatchReader<Time = ()>,
    T: Trace<Key = B::Key, Val = B::Val, R = B::R>,
{
    fn eval(&mut self, _trace: &T, _batch: &B) -> T {
        // Refuse to accept trace by reference.  This should not happen in a correctly
        // constructed circuit.
        unimplemented!()
    }

    fn eval_owned_and_ref(&mut self, mut trace: T, batch: &B) -> T {
        // TODO: extend `trace` type to feed untimed batches directly
        // (adding fixed timestamp on the fly).
        trace.insert(batch_add_time(batch, &self.time));
        self.time = self.time.advance(0);
        trace
    }

    fn eval_ref_and_owned(&mut self, _trace: &T, _batch: B) -> T {
        // Refuse to accept trace by reference.  This should not happen in a correctly
        // constructed circuit.
        unimplemented!()
    }

    fn eval_owned(&mut self, mut trace: T, batch: B) -> T {
        trace.insert(batch_add_time(&batch, &self.time));
        self.time = self.time.advance(0);
        trace
    }

    fn input_preference(&self) -> (OwnershipPreference, OwnershipPreference) {
        (
            OwnershipPreference::PREFER_OWNED,
            OwnershipPreference::PREFER_OWNED,
        )
    }
}

pub struct Z1Trace<T: Trace> {
    time: T::Time,
    trace: Option<T>,
    // `dirty[scope]` is `true` iff at least one non-empty update was added to the trace
    // since the previous clock cycle at level `scope`.
    dirty: Vec<bool>,
    root_scope: Scope,
    reset_on_clock_start: bool,
}

impl<T> Z1Trace<T>
where
    T: Trace,
{
    pub fn new(reset_on_clock_start: bool, root_scope: Scope) -> Self {
        Self {
            time: T::Time::clock_start(),
            trace: None,
            dirty: vec![false; root_scope as usize + 1],
            root_scope,
            reset_on_clock_start,
        }
    }
}

impl<T> Operator for Z1Trace<T>
where
    T: Trace,
{
    fn name(&self) -> Cow<'static, str> {
        Cow::from("Z1 (trace)")
    }

    fn clock_start(&mut self, scope: Scope) {
        self.dirty[scope as usize] = false;

        if scope == 0 && self.trace.is_none() {
            // TODO: use T::with_effort with configurable effort?
            self.trace = Some(T::new(None));
        }
    }

    fn clock_end(&mut self, scope: Scope) {
        if scope + 1 == self.root_scope && !self.reset_on_clock_start {
            if let Some(tr) = self.trace.as_mut() {
                tr.recede_to(&self.time.epoch_end(self.root_scope).recede(self.root_scope));
            }
        }
        self.time.advance(scope + 1);
    }

    fn metadata(&self, meta: &mut OperatorMeta) {
        let total_size = self
            .trace
            .as_ref()
            .map(|trace| trace.num_entries_deep())
            .unwrap_or(0);

        let bytes = self
            .trace
            .as_ref()
            .map(|trace| trace.size_of())
            .unwrap_or_default();

        meta.extend(metadata! {
            "total size" => total_size,
            "allocated bytes" => MetaItem::bytes(bytes.total_bytes()),
            "used bytes" => MetaItem::bytes(bytes.used_bytes()),
            "allocations" => bytes.distinct_allocations(),
            "shared bytes" => MetaItem::bytes(bytes.shared_bytes()),
        });
    }

    fn fixedpoint(&self, scope: Scope) -> bool {
        !self.dirty[scope as usize]
    }
}

impl<T> StrictOperator<T> for Z1Trace<T>
where
    T: Trace,
{
    fn get_output(&mut self) -> T {
        let mut result = self.trace.take().unwrap();
        result.clear_dirty_flag();
        result
    }

    fn get_final_output(&mut self) -> T {
        if self.reset_on_clock_start {
            self.get_output()
        } else {
            T::new(None)
        }
    }
}

impl<T> StrictUnaryOperator<T, T> for Z1Trace<T>
where
    T: Trace,
{
    fn eval_strict(&mut self, _i: &T) {
        unimplemented!()
    }

    fn eval_strict_owned(&mut self, i: T) {
        self.time = self.time.advance(0);

        let dirty = i.dirty();
        self.trace = Some(i);

        self.dirty[0] = dirty;
        for d in self.dirty[1..].iter_mut() {
            *d = *d || dirty;
        }
    }

    fn input_preference(&self) -> OwnershipPreference {
        OwnershipPreference::PREFER_OWNED
    }
}
