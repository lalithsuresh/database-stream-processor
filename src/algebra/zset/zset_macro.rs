/// Create an indexed Z-set with specified elements.
///
/// This macro is used in unit tests to create reference inputs and outputs.
/// It generates an indexed Z-set of type
/// [`OrdIndexedZSet`](crate::trace::ord::OrdIndexedZSet)s.
#[macro_export]
macro_rules! indexed_zset {
    ( $($key:expr => { $($value:expr => $weight:expr),* }),* $(,)?) => {{
        let mut batcher = <<$crate::trace::ord::OrdIndexedZSet<_, _, _> as $crate::trace::Batch>::Batcher as $crate::trace::Batcher<_, _, _, _>>::new_batcher(());
        let mut batch = ::std::vec![ $( $( (($key, $value), $weight) ),* ),* ];
        $crate::trace::Batcher::push_batch(&mut batcher, &mut batch);
        $crate::trace::Batcher::seal(batcher)
    }};
}

/// Create a Z-set with specified elements.
///
/// This macro is used in unit tests to create reference inputs and outputs.
/// It generates a Z-set of type [`OrdZSet`](crate::trace::ord::OrdZSet)s.
#[macro_export]
macro_rules! zset {
    ( $( $key:expr => $weight:expr ),* $(,)?) => {{
        let mut batcher = <<$crate::trace::ord::OrdZSet<_, _> as $crate::trace::Batch>::Batcher as $crate::trace::Batcher<_, _, _, _>>::new_batcher(());

        let mut batch = ::std::vec![ $( ($key, $weight) ),* ];
        $crate::trace::Batcher::push_batch(&mut batcher, &mut batch);
        $crate::trace::Batcher::seal(batcher)
    }};
}

/// Create a Z-set with specified elements all with weight 1.
///
/// This macro is used in unit tests to create reference inputs and outputs.
/// It generates a Z-set of type [`OrdZSet`](crate::trace::ord::OrdZSet)s.
#[macro_export]
macro_rules! zset_set {
    ( $( $key:expr ),* $(,)?) => {{
        let mut batcher = <<$crate::trace::ord::OrdZSet<_, _> as $crate::trace::Batch>::Batcher as $crate::trace::Batcher<_, _, _, _>>::new_batcher(());

        let mut batch = ::std::vec![ $( ($key, 1) ),* ];
        $crate::trace::Batcher::push_batch(&mut batcher, &mut batch);
        $crate::trace::Batcher::seal(batcher)
    }};
}
