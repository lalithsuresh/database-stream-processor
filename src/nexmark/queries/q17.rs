use super::NexmarkStream;
use crate::{
    nexmark::{model::Event, queries::OrdinalDate},
    operator::{FilterMap, Max, Min},
    Circuit, OrdIndexedZSet, OrdZSet, Stream,
};
use arcstr::ArcStr;
use std::time::{Duration, SystemTime};
use time::{
    format_description::well_known::{iso8601, iso8601::FormattedComponents, Iso8601},
    Date, OffsetDateTime,
};

///
/// Query 17: Auction Statistics Report (Not in original suite)
///
/// How many bids on an auction made a day and what is the price?
/// Illustrates an unbounded group aggregation.
///
/// ```sql
/// CREATE TABLE discard_sink (
///   auction BIGINT,
///   `day` VARCHAR,
///   total_bids BIGINT,
///   rank1_bids BIGINT,
///   rank2_bids BIGINT,
///   rank3_bids BIGINT,
///   min_price BIGINT,
///   max_price BIGINT,
///   avg_price BIGINT,
///   sum_price BIGINT
/// ) WITH (
///   'connector' = 'blackhole'
/// );
///
/// INSERT INTO discard_sink
/// SELECT
///      auction,
///      DATE_FORMAT(dateTime, 'yyyy-MM-dd') as `day`,
///      count(*) AS total_bids,
///      count(*) filter (where price < 10000) AS rank1_bids,
///      count(*) filter (where price >= 10000 and price < 1000000) AS rank2_bids,
///      count(*) filter (where price >= 1000000) AS rank3_bids,
///      min(price) AS min_price,
///      max(price) AS max_price,
///      avg(price) AS avg_price,
///      sum(price) AS sum_price
/// FROM bid
/// GROUP BY auction, DATE_FORMAT(dateTime, 'yyyy-MM-dd');
/// ```

type Q17Output = (
    u64,
    ArcStr,
    isize,
    isize,
    isize,
    isize,
    usize,
    usize,
    isize,
    isize,
);

type Q17Stream = Stream<Circuit<()>, OrdZSet<Q17Output, isize>>;

pub fn q17(input: NexmarkStream) -> Q17Stream {
    let iso8601_day_format = &Iso8601::<
        {
            iso8601::Config::DEFAULT
                .set_formatted_components(FormattedComponents::Date)
                .encode()
        },
    >;

    let bids_indexed = input.flat_map_index(|event| match event {
        Event::Bid(b) => {
            let date_time = SystemTime::UNIX_EPOCH + Duration::from_millis(b.date_time);

            let date_time = <SystemTime as Into<OffsetDateTime>>::into(date_time);
            let day = date_time.date().to_ordinal_date();

            Some(((b.auction, day), b.price))
        }
        _ => None,
    });

    let count_total_bids: Stream<_, OrdIndexedZSet<(u64, OrdinalDate), isize, _>> =
        bids_indexed.aggregate_linear::<(), _, _>(|_, _| -> isize { 1 });
    let count_rank1_bids = bids_indexed
        .filter(|(_auction_day, price)| *price < 10_000)
        .aggregate_linear::<(), _, _>(|_, _| -> isize { 1 });
    let count_rank2_bids = bids_indexed
        .filter(|(_auction_day, price)| *price >= 10_000 && *price < 1_000_000)
        .aggregate_linear::<(), _, _>(|_, _| -> isize { 1 });
    let count_rank3_bids = bids_indexed
        .filter(|(_auction_day, price)| *price >= 1_000_000)
        .aggregate_linear::<(), _, _>(|_, _| -> isize { 1 });
    let min_price = bids_indexed.aggregate::<(), _>(Min);
    let max_price = bids_indexed.aggregate::<(), _>(Max);
    let sum_price =
        bids_indexed.aggregate_linear::<(), _, _>(|_, price| -> isize { *price as isize });

    // Another outer-join abomination to put all aggregates into single stream.
    count_total_bids
        .outer_join_default(&count_rank1_bids, |auction_day, total_bids, count_rank1| {
            (*auction_day, (*total_bids, *count_rank1))
        })
        .index()
        .outer_join_default(
            &count_rank2_bids,
            |auction_day, (total_bids, count_rank1), count_rank2| {
                (*auction_day, (*total_bids, *count_rank1, *count_rank2))
            },
        )
        .index()
        .outer_join_default(
            &count_rank3_bids,
            |auction_day, (total_bids, count_rank1, count_rank2), count_rank3| {
                (
                    *auction_day,
                    (*total_bids, *count_rank1, *count_rank2, *count_rank3),
                )
            },
        )
        .index()
        .outer_join_default(
            &min_price,
            |auction_day, (total_bids, count_rank1, count_rank2, count_rank3), min_price| {
                (
                    *auction_day,
                    (
                        *total_bids,
                        *count_rank1,
                        *count_rank2,
                        *count_rank3,
                        *min_price,
                    ),
                )
            },
        )
        .index()
        .outer_join_default(
            &max_price,
            |auction_day,
             (total_bids, count_rank1, count_rank2, count_rank3, min_price),
             max_price| {
                (
                    *auction_day,
                    (
                        *total_bids,
                        *count_rank1,
                        *count_rank2,
                        *count_rank3,
                        *min_price,
                        *max_price,
                    ),
                )
            },
        )
        .index()
        .outer_join_default(
            &sum_price,
            |(auction, day),
             (total_bids, count_rank1, count_rank2, count_rank3, min_price, max_price),
             sum_price| {
                (
                    *auction,
                    Date::from_ordinal_date(day.0, day.1)
                        .unwrap()
                        .format(iso8601_day_format)
                        .unwrap()
                        .into(),
                    *total_bids,
                    *count_rank1,
                    *count_rank2,
                    *count_rank3,
                    *min_price,
                    *max_price,
                    (*sum_price / *total_bids),
                    *sum_price,
                )
            },
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        nexmark::{generator::tests::make_bid, model::Bid},
        zset,
    };
    use rstest::rstest;

    #[rstest]
    #[case::single_auction_single_batch(
        vec![vec![
            (1, 0, 100),
            (1, 10_000, 700),
            (1, 20_000, 400),
        ]],
        vec![zset! {
            (
            1,
            String::from("1970-01-01").into(),
            3,
            3,
            0,
            0,
            100,
            700,
            400,
            1_200,
        ) => 1}]
    )]
    #[case::multiple_auctions_single_batch(
        vec![vec![
            (1, 0, 100),
            (2, 5_000, 500),
            (1, 10_000, 700),
            (2, 15_000, 1_500),
            (1, 20_000, 400),
            (2, 25_000, 3_000),
        ]],
        vec![zset! {
            (
            1,
            String::from("1970-01-01").into(),
            3,
            3,
            0,
            0,
            100,
            700,
            400,
            1_200,
        ) => 1, (
            2,
            String::from("1970-01-01").into(),
            3,
            3,
            0,
            0,
            500,
            3_000,
            1_666,
            5_000,
        ) => 1}]
    )]
    #[case::multiple_auctions_multiple_batches(
        vec![vec![
            (1, 0, 100),
        ], vec![
            // This batch has an extra bid from the first day (updating one aggregate)
            // and more bids for a second day.
            (1, 1_000*60*60*24 - 1, 10_100),
            (2, 1_000*60*60*24*2, 1_000_000),
            (2, 1_000*60*60*24*2 + 1_000, 2_000_000),
        ]],
        vec![zset! {
            (
            1,
            String::from("1970-01-01").into(),
            1,
            1,
            0,
            0,
            100,
            100,
            100,
            100,
        ) => 1}, zset! {
            (
            1,
            String::from("1970-01-01").into(),
            1,
            1,
            0,
            0,
            100,
            100,
            100,
            100,
        ) => -1, (
            1,
            String::from("1970-01-01").into(),
            2,
            1,
            1,
            0,
            100,
            10_100,
            5_100,
            10_200,
        ) => 1, (
            2,
            String::from("1970-01-03").into(),
            2,
            0,
            0,
            2,
            1_000_000,
            2_000_000,
            1_500_000,
            3_000_000,
        ) => 1
        }]
    )]
    fn test_q17(
        #[case] input_bid_batches: Vec<Vec<(u64, u64, usize)>>,
        #[case] expected_zsets: Vec<OrdZSet<Q17Output, isize>>,
    ) {
        let input_vecs = input_bid_batches.into_iter().map(|batch| {
            batch
                .into_iter()
                .map(|(auction, date_time, price)| {
                    (
                        Event::Bid(Bid {
                            auction,
                            date_time,
                            price,
                            ..make_bid()
                        }),
                        1,
                    )
                })
                .collect()
        });

        let (circuit, mut input_handle) = Circuit::build(move |circuit| {
            let (stream, input_handle) = circuit.add_input_zset::<Event, isize>();

            let output = q17(stream);

            let mut expected_output = expected_zsets.into_iter();
            output.inspect(move |batch| assert_eq!(batch, &expected_output.next().unwrap()));

            input_handle
        })
        .unwrap();

        for mut vec in input_vecs {
            input_handle.append(&mut vec);
            circuit.step().unwrap();
        }
    }
}
