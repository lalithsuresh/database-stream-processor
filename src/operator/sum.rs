//! N-ary plus operator.

use crate::{
    algebra::{AddAssignByRef, AddByRef, HasZero},
    circuit::{
        operator_traits::{NaryOperator, Operator},
        Circuit, OwnershipPreference, Scope, Stream,
    },
    NumEntries,
};
use std::{
    borrow::Cow,
    cmp::Reverse,
    iter::once,
    mem::{take, ManuallyDrop},
    ops::Add,
};

impl<P, D> Stream<Circuit<P>, D>
where
    P: Clone + 'static,
    D: Add<Output = D> + AddByRef + AddAssignByRef + Clone + HasZero + NumEntries + 'static,
{
    /// Apply the [`Sum`] operator to `self` and all streams in `streams`.
    pub fn sum<'a, I>(&'a self, streams: I) -> Stream<Circuit<P>, D>
    where
        I: IntoIterator<Item = &'a Self>,
    {
        self.circuit()
            .add_nary_operator(Sum::new(), once(self).chain(streams))
    }
}

/// Operator that computes the sum of values across all its input streams at
/// each timestamp.
pub struct Sum<D>
where
    D: Clone + 'static,
{
    // Vector of input values.  Keep it here to reuse the allocation across
    // operator invocations.
    inputs: Vec<Cow<'static, D>>,
}

impl<D> Sum<D>
where
    D: Clone + 'static,
{
    pub fn new() -> Self {
        Self { inputs: Vec::new() }
    }
}

impl<D> Default for Sum<D>
where
    D: Clone + 'static,
{
    fn default() -> Self {
        Self::new()
    }
}

impl<D> Operator for Sum<D>
where
    D: Clone + 'static,
{
    fn name(&self) -> Cow<'static, str> {
        Cow::from("Sum")
    }

    fn fixedpoint(&self, _scope: Scope) -> bool {
        true
    }
}

impl<D> NaryOperator<D, D> for Sum<D>
where
    D: Add<Output = D> + AddAssignByRef + Clone + HasZero + NumEntries + 'static,
{
    fn eval<'a, Iter>(&mut self, inputs: Iter) -> D
    where
        Iter: Iterator<Item = Cow<'a, D>>,
    {
        let mut input_vec: Vec<Cow<'a, _>> = unsafe {
            let mut buffer = ManuallyDrop::new(take(&mut self.inputs));
            let (ptr, len, cap) = (buffer.as_mut_ptr(), buffer.len(), buffer.capacity());
            Vec::from_raw_parts(ptr, len, cap)
        };

        assert!(input_vec.is_empty());

        for input in inputs {
            input_vec.push(input);
        }

        input_vec.sort_by_key(|x| Reverse(x.num_entries_shallow()));

        let mut res = D::zero();
        for input in input_vec.drain(..) {
            if res.is_zero() {
                res = match input {
                    Cow::Borrowed(v) => v.clone(),
                    Cow::Owned(v) => v,
                };
            } else {
                match input {
                    Cow::Borrowed(v) => res.add_assign_by_ref(v),
                    Cow::Owned(v) => res = res + v,
                }
            }
        }

        self.inputs = unsafe {
            assert!(input_vec.is_empty());
            let mut buffer = ManuallyDrop::new(input_vec);
            let (ptr, len, cap) = (buffer.as_mut_ptr(), buffer.len(), buffer.capacity());
            Vec::from_raw_parts(ptr.cast(), len, cap)
        };

        res
    }

    fn input_preference(&self) -> OwnershipPreference {
        OwnershipPreference::PREFER_OWNED
    }
}

#[cfg(test)]
mod test {
    use crate::{
        algebra::HasZero,
        circuit::OwnershipPreference,
        operator::{Generator, Inspect},
        trace::{ord::OrdZSet, Batch},
        zset, Circuit,
    };

    #[test]
    fn zset_sum() {
        let build_circuit = |circuit: &Circuit<()>| {
            let mut s = <OrdZSet<_, _> as HasZero>::zero();
            let source1 = circuit.add_source(Generator::new(move || {
                let res = s.clone();
                s = s.merge(&zset! { 5 => 1, 6 => 2 });
                res
            }));
            let mut s = <OrdZSet<_, _> as HasZero>::zero();
            let source2 = circuit.add_source(Generator::new(move || {
                let res = s.clone();
                s = s.merge(&zset! { 5 => -1 });
                res
            }));
            let mut s = <OrdZSet<_, _> as HasZero>::zero();
            let source3 = circuit.add_source(Generator::new(move || {
                let res = s.clone();
                s = s.merge(&zset! { 6 => -1 });
                res
            }));

            // Supply `source3` twice to test the handling of aliases.
            source3
                .sum(&[source2.clone(), source1.clone(), source3.clone()])
                .inspect(|s| assert_eq!(s, &<OrdZSet<_, _> as HasZero>::zero()));
            (source1, source2, source3)
        };

        // Allow `Sum` to consume all streams by value.
        let circuit = Circuit::build(move |circuit| {
            build_circuit(circuit);
        })
        .unwrap()
        .0;

        for _ in 0..100 {
            circuit.step().unwrap();
        }

        // Only consume source2, source3 by value.
        let circuit = Circuit::build(move |circuit| {
            let (source1, _source2, _source3) = build_circuit(circuit);
            circuit.add_unary_operator_with_preference(
                Inspect::new(|_| {}),
                &source1,
                OwnershipPreference::STRONGLY_PREFER_OWNED,
            );
        })
        .unwrap()
        .0;

        for _ in 0..100 {
            circuit.step().unwrap();
        }

        // Consume all streams by reference.
        let circuit = Circuit::build(move |circuit| {
            let (source1, source2, source3) = build_circuit(circuit);
            circuit.add_unary_operator_with_preference(
                Inspect::new(|_| {}),
                &source1,
                OwnershipPreference::STRONGLY_PREFER_OWNED,
            );
            circuit.add_unary_operator_with_preference(
                Inspect::new(|_| {}),
                &source2,
                OwnershipPreference::STRONGLY_PREFER_OWNED,
            );
            circuit.add_unary_operator_with_preference(
                Inspect::new(|_| {}),
                &source3,
                OwnershipPreference::STRONGLY_PREFER_OWNED,
            );
        })
        .unwrap()
        .0;

        for _ in 0..100 {
            circuit.step().unwrap();
        }
    }
}
