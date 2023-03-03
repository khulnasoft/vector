use std::{fmt, sync::Arc};

use async_trait::async_trait;
use chrono::Utc;
use futures_util::{
    future::ready,
    stream::{self, BoxStream},
    StreamExt,
};
use tower::Service;
use vector_common::finalization::EventFinalizers;
use vector_core::{
    event::{Event, Metric, MetricValue},
    partition::Partitioner,
    sink::StreamSink,
    stream::{BatcherSettings, DriverResponse},
};

use super::{
    config::DatadogMetricsEndpoint, normalizer::DatadogMetricsNormalizer,
    request_builder::DatadogMetricsRequestBuilder, service::DatadogMetricsRequest,
};
use crate::{
    internal_events::DatadogMetricsEncodingError,
    sinks::util::{
        buffer::metrics::sort::sort_for_compression,
        buffer::metrics::{AggregatedSummarySplitter, MetricSplitter},
        SinkBuilderExt,
    },
};

/// Partitions metrics based on which Datadog API endpoint that they are sent to.
///
/// Generally speaking, all "basic" metrics -- counter, gauge, set, aggregated summary-- are sent to
/// the Series API, while distributions, aggregated histograms, and sketches (hehe) are sent to the
/// Sketches API.
struct DatadogMetricsTypePartitioner;

impl Partitioner for DatadogMetricsTypePartitioner {
    type Item = Metric;
    type Key = (Option<Arc<str>>, DatadogMetricsEndpoint);

    fn partition(&self, item: &Self::Item) -> Self::Key {
        let endpoint = match item.data().value() {
            MetricValue::Counter { .. } => DatadogMetricsEndpoint::Series,
            MetricValue::Gauge { .. } => DatadogMetricsEndpoint::Series,
            MetricValue::Set { .. } => DatadogMetricsEndpoint::Series,
            MetricValue::Distribution { .. } => DatadogMetricsEndpoint::Sketches,
            MetricValue::AggregatedHistogram { .. } => DatadogMetricsEndpoint::Sketches,
            MetricValue::AggregatedSummary { .. } => DatadogMetricsEndpoint::Series,
            MetricValue::Sketch { .. } => DatadogMetricsEndpoint::Sketches,
        };
        (item.metadata().datadog_api_key(), endpoint)
    }
}

pub(crate) struct DatadogMetricsSink<S> {
    service: S,
    request_builder: DatadogMetricsRequestBuilder,
    batch_settings: BatcherSettings,
    protocol: String,
}

impl<S> DatadogMetricsSink<S>
where
    S: Service<DatadogMetricsRequest> + Send,
    S::Error: fmt::Debug + Send + 'static,
    S::Future: Send + 'static,
    S::Response: DriverResponse,
{
    /// Creates a new `DatadogMetricsSink`.
    pub const fn new(
        service: S,
        request_builder: DatadogMetricsRequestBuilder,
        batch_settings: BatcherSettings,
        protocol: String,
    ) -> Self {
        DatadogMetricsSink {
            service,
            request_builder,
            batch_settings,
            protocol,
        }
    }

    async fn run_inner(self: Box<Self>, input: BoxStream<'_, Event>) -> Result<(), ()> {
        let mut splitter: MetricSplitter<AggregatedSummarySplitter> = MetricSplitter::default();

        input
            // Convert `Event` to `Metric` so we don't have to deal with constant conversions.
            .filter_map(|event| ready(event.try_into_metric()))
            // Split aggregated summaries into individual metrics for count, sum, and the quantiles, which lets us
            // ensure that aggregated summaries effectively make it through normalization, as we can't actually
            // normalize them and so they would be dropped during normalization otherwise.
            .flat_map(|metric| stream::iter(splitter.split(metric)))
            // Converts "absolute" metrics to "incremental", and converts distributions and aggregated histograms into
            // sketches so that we can send them in a more DD-native format and thus avoid needing to directly specify
            // what quantiles to generate, etc.
            .normalized_with_default::<DatadogMetricsNormalizer>()
            // We batch metrics by their endpoint: series endpoint for counters, gauge, and sets vs sketch endpoint for
            // distributions, aggregated histograms, and sketches.
            .batched_partitioned(DatadogMetricsTypePartitioner, self.batch_settings)
            // Aggregate counters with identical timestamps, otherwise identical counters (same
            // series and same timestamp, when rounded to whole seconds) will be dropped in a
            // last-write-wins situation when they hit the DD metrics intake.
            .map(|((api_key, endpoint), mut metrics)| {
                //let collapsed_metrics = collapse_counters_by_series_and_timestamp(metrics);
                collapse_counters_by_series_and_timestamp(&mut metrics);
                ((api_key, endpoint), metrics)
            })
            // Sort metrics by name, which significantly improves HTTP compression.
            .map(|((api_key, endpoint), mut metrics)| {
                sort_for_compression(&mut metrics);
                ((api_key, endpoint), metrics)
            })
            // We build our requests "incrementally", which means that for a single batch of metrics, we might generate
            // N requests to send them all, as Datadog has API-level limits on payload size, so we keep adding metrics
            // to a request until we reach the limit, and then create a new request, and so on and so forth, until all
            // metrics have been turned into a request.
            .incremental_request_builder(self.request_builder)
            // This unrolls the vector of request results that our request builder generates.
            .flat_map(stream::iter)
            // Generating requests _can_ fail, so we log and filter out errors here.
            .filter_map(|request| async move {
                match request {
                    Err(e) => {
                        let (error_message, error_code, dropped_events) = e.into_parts();
                        emit!(DatadogMetricsEncodingError {
                            error_message,
                            error_code,
                            dropped_events: dropped_events as usize,
                        });
                        None
                    }
                    Ok(req) => Some(req),
                }
            })
            // Finally, we generate the driver which will take our requests, send them off, and appropriately handle
            // finalization of the events, and logging/metrics, as the requests are responded to.
            .into_driver(self.service)
            .protocol(self.protocol)
            .run()
            .await
    }
}

#[async_trait]
impl<S> StreamSink<Event> for DatadogMetricsSink<S>
where
    S: Service<DatadogMetricsRequest> + Send,
    S::Error: fmt::Debug + Send + 'static,
    S::Future: Send + 'static,
    S::Response: DriverResponse,
{
    async fn run(self: Box<Self>, input: BoxStream<'_, Event>) -> Result<(), ()> {
        // Rust has issues with lifetimes and generics, which `async_trait` exacerbates, so we write
        // a normal async fn in `DatadogMetricsSink` itself, and then call out to it from this trait
        // implementation, which makes the compiler happy.
        self.run_inner(input).await
    }
}

pub fn collapse_counters_by_series_and_timestamp(metrics: &mut Vec<Metric>) {
    let og_len = metrics.len();

    if og_len < 2 {
        return;
    }

    // NOTE: Astute observers may recognize that this behavior could also be acheived by using
    // `Vec::dedup_by`, but the clincher is that `dedup_by` requires a sorted vector to begin with.
    //
    // This function is designed to collapse duplicate counters even if the metrics are unsorted,
    // which leads to a measurable boost in performance, being nearly 35% faster than `dedup_by`
    // when the inputs are sorted, and up to 50% faster when the inputs are unsorted.
    //
    // These numbers are based on sorting a newtype wrapper around the metric instead of the metric
    // itself, which does involve allocating a string in our tests. _However_, sorting the `Metric`
    // directly is not possible without a customized `PartialOrd` implementation, as some of the
    // nested fields containing `f64` values makes it underivable, and I'm not 100% sure that we
    // could/would want to have a narrowly-focused impl of `PartialOrd` on `Metric` to fit this use
    // case (metric type -> metric name -> metric timestamp, nothing else) vs being able to sort
    // metrics by name first, etc. Then there's the potential issue of the reordering of fields
    // changing the ordering behavior of `Metric`... and it just felt easier to write this tailored
    // algorithm for the use case at hand.
    let mut idx = 0;
    let now_ts = Utc::now().timestamp();

    let mut total_collapsed = 0;
    let mut iter_len = og_len;

    // For each metric, see if it's a counter. If so, we check the rest of the metrics
    // _after_ it to see if they share the same series _and_ timestamp, when converted
    // to a Unix timestamp. If they match, we take that counter's value and merge it
    // with our "current" counter metric, and then drop the secondary one from the
    // vector.
    //
    // For any non-counter, we simply ignore it and leave it as-is.
    while idx < iter_len - 1 {
        let mut accumulated_value = 0.0;
        let mut accumulated_finalizers = EventFinalizers::default();
        let mut n_collapsed = 0;

        match metrics[idx].value() {
            MetricValue::Counter { .. } => {
                let (left, right) = metrics.split_at_mut(idx + 1);

                n_collapsed = collapse_counters(
                    &left[idx],
                    right,
                    &mut idx,
                    total_collapsed,
                    now_ts,
                    &mut accumulated_value,
                    &mut accumulated_finalizers,
                );
            }
            // skip non-counters
            _ => {}
        }

        // If we collapsed any during the accumulator phase, update our original counter.
        if n_collapsed > 0 {
            total_collapsed += n_collapsed;
            iter_len -= n_collapsed;
            let metric = metrics.get_mut(idx).expect("current index must exist");
            match metric.value_mut() {
                MetricValue::Counter { value } => {
                    *value += accumulated_value;
                    metric
                        .metadata_mut()
                        .merge_finalizers(accumulated_finalizers);
                }
                _ => unreachable!("current index must represent a counter"),
            }
        }

        idx += 1;
    }

    if total_collapsed > 0 {
        metrics.truncate(og_len - total_collapsed);
    }
}

// Now go through each metric _after_ the current one to see if it matches the
// current metric: is a counter, with the same name and timestamp. If it is, we
// accumulate its value and then remove it.
//
// Otherwise, we skip it.
fn collapse_counters(
    curr_metric: &Metric,
    right: &mut [Metric],
    idx: &mut usize,
    dead_end: usize,
    now_ts: i64,
    accumulated_value: &mut f64,
    accumulated_finalizers: &mut EventFinalizers,
) -> usize {
    let mut counter_epoch = curr_metric.data().epoch();

    if counter_epoch == 0 {
        counter_epoch = now_ts;
    }

    let curr_series = curr_metric.series();

    let mut is_disjoint = false;
    let mut n_collapsed = 0;
    let mut right_idx = 0;

    let mut right_end = right.len() - dead_end;

    while right_idx < right_end {
        let inner_metric = &mut right[right_idx];
        let mut should_advance = true;

        let value = inner_metric.value();
        let data = inner_metric.data();

        let mut other_counter_epoch = data.epoch();
        if other_counter_epoch == 0 {
            other_counter_epoch = now_ts;
        }

        // Order of comparison matters here. Compare to the timespamps first as the series
        // comparison is much more expensive.
        if counter_epoch == other_counter_epoch && curr_series == inner_metric.series() {
            // Collapse this counter by accumulating its value, and its
            // finalizers, and removing it from the original vector of metrics.

            // always true
            if let MetricValue::Counter { value } = value {
                *accumulated_value += value;
            }

            accumulated_finalizers.merge(inner_metric.metadata_mut().take_finalizers());

            right[right_idx] = right[right_end - 1].clone();
            right_end -= 1;
            should_advance = false;
            n_collapsed += 1;
        } else {
            // We hit a counter that _doesn't_ match, but we can't just skip
            // it because we also need to evaulate it against all the
            // counters that come after it, so we only increment the index
            // for this inner loop.
            //
            // As well, we mark ourselves to stop incrementing the outer
            // index if we find more counters to accumulate, because we've
            // hit a disjoint counter here. While we may be continuing to
            // shrink the count of remaining metrics from accumulating,
            // we have to ensure this counter we just visited is visited by
            // the outer loop.
            is_disjoint = true;
        }

        if should_advance {
            right_idx += 1;

            if !is_disjoint {
                *idx += 1;
            }
        }
    }

    n_collapsed
}

#[cfg(test)]
mod tests {
    use std::fs::File;

    use chrono::{DateTime, Duration, Utc};
    use proptest::prelude::*;
    use vector_core::event::{Metric, MetricKind, MetricValue};

    use super::collapse_counters_by_series_and_timestamp;

    fn arb_collapsible_metrics() -> impl Strategy<Value = Vec<Metric>> {
        let ts = Utc::now();

        any::<Vec<(u16, MetricValue)>>().prop_map(move |values| {
            values
                .into_iter()
                .map(|(id, value)| {
                    let name = format!("{}-{}", value.as_name(), id);
                    Metric::new(name, MetricKind::Incremental, value).with_timestamp(Some(ts))
                })
                .collect()
        })
    }

    fn create_counter(name: &str, value: f64) -> Metric {
        Metric::new(
            name,
            MetricKind::Incremental,
            MetricValue::Counter { value },
        )
    }

    fn create_gauge(name: &str, value: f64) -> Metric {
        Metric::new(name, MetricKind::Incremental, MetricValue::Gauge { value })
    }

    #[test]
    fn collapse_no_metrics() {
        let mut actual = Vec::new();
        let expected = actual.clone();
        collapse_counters_by_series_and_timestamp(&mut actual);

        assert_eq!(expected, actual);
    }

    #[test]
    fn collapse_single_metric() {
        let mut actual = vec![create_counter("basic", 42.0)];
        let expected = actual.clone();
        collapse_counters_by_series_and_timestamp(&mut actual);

        assert_eq!(expected, actual);
    }

    #[test]
    fn collapse_identical_metrics_gauge() {
        let mut actual = vec![create_gauge("basic", 42.0), create_gauge("basic", 42.0)];
        let expected = actual.clone();
        collapse_counters_by_series_and_timestamp(&mut actual);

        assert_eq!(expected, actual);

        let gauge_value = 41.0;
        let mut actual = vec![
            create_gauge("basic", gauge_value),
            create_gauge("basic", gauge_value),
            create_gauge("basic", gauge_value),
            create_gauge("basic", gauge_value),
            create_gauge("basic", gauge_value),
            create_gauge("basic", gauge_value),
            create_gauge("basic", gauge_value),
        ];
        let expected = actual.clone();
        collapse_counters_by_series_and_timestamp(&mut actual);

        assert_eq!(expected, actual);
    }

    #[test]
    fn collapse_identical_metrics_counter_og() {
        let counter_value = 42.0;

        let mut actual = vec![
            create_counter("basic", counter_value),
            create_counter("basic", counter_value),
            create_counter("basic", counter_value),
            create_counter("basic", counter_value),
            create_counter("basic", counter_value),
            create_counter("basic", counter_value),
            create_counter("basic", counter_value),
        ];

        let expected_counter_value = actual.len() as f64 * counter_value;
        let expected = vec![create_counter("basic", expected_counter_value)];
        collapse_counters_by_series_and_timestamp(&mut actual);

        assert_eq!(expected, actual);
    }

    /// Tests collapse_counters_by_series_and_timestamp() using 1,000,000 identical metrics with no
    /// timestamp
    #[test]
    fn collapse_identical_metrics_counter_large_no_timestamp() {
        let counter_value = 42.0;

        let mut actual = vec![];

        // need to have enough runtime to trigger the sampling
        for _ in 0..1_000_000 {
            actual.push(create_counter("basic", counter_value));
        }

        let expected_counter_value = actual.len() as f64 * counter_value;
        let expected = vec![create_counter("basic", expected_counter_value)];

        let guard = pprof::ProfilerGuardBuilder::default()
            .frequency(100)
            .blocklist(&["libc", "libgcc", "pthread", "vdso"])
            .build()
            .unwrap();

        collapse_counters_by_series_and_timestamp(&mut actual);

        if let Ok(report) = guard.report().build() {
            //println!("report: {:?}", &report);

            let file = File::create("flamegraph.svg").unwrap();
            let mut options = pprof::flamegraph::Options::default();
            report.flamegraph_with_options(file, &mut options).unwrap();
        };

        assert_eq!(expected, actual);
    }

    /// Tests collapse_counters_by_series_and_timestamp() using 1,000,000 identical metrics with no
    /// timestamp
    #[test]
    fn collapse_identical_metrics_counter_large_with_timestamp() {
        let counter_value = 42.0;

        let mut actual = vec![];

        let now_ts = Some(Utc::now());

        // need to have enough runtime to trigger the sampling
        for _ in 0..1_000_000 {
            actual.push(create_counter("basic", counter_value).with_timestamp(now_ts));
        }

        let expected_counter_value = actual.len() as f64 * counter_value;
        let expected = vec![create_counter("basic", expected_counter_value).with_timestamp(now_ts)];

        let guard = pprof::ProfilerGuardBuilder::default()
            .frequency(100)
            .blocklist(&["libc", "libgcc", "pthread", "vdso"])
            .build()
            .unwrap();

        collapse_counters_by_series_and_timestamp(&mut actual);

        if let Ok(report) = guard.report().build() {
            //println!("report: {:?}", &report);

            let file = File::create("flamegraph.svg").unwrap();
            let mut options = pprof::flamegraph::Options::default();
            report.flamegraph_with_options(file, &mut options).unwrap();
        };

        assert_eq!(expected, actual);
    }

    /// Tests collapse_counters_by_series_and_timestamp() using 10,000 metrics where every 10th
    /// metric is a collapsible one and the remaining have unique, existing timestamps.
    #[test]
    fn collapse_identical_metrics_counter_mix() {
        let counter_value = 42.0;

        let mut actual = vec![];

        let mut j = 0;
        let mut f = 0;

        let now = Utc::now();

        for _ in 0..10_000 {
            if j == 10 {
                actual.push(create_counter("basic", counter_value));
                j = 0;
            } else {
                actual.push(
                    create_counter("basic", counter_value)
                        .with_timestamp(Some(now + Duration::seconds(f))),
                );
            }

            j = j + 1;
            f = f + 1;
        }

        let expected_counter_value = actual.len() as f64 * counter_value;
        let _expected = vec![create_counter("basic", expected_counter_value)];

        let guard = pprof::ProfilerGuardBuilder::default()
            .frequency(100)
            .blocklist(&["libc", "libgcc", "pthread", "vdso"])
            .build()
            .unwrap();

        collapse_counters_by_series_and_timestamp(&mut actual);

        if let Ok(report) = guard.report().build() {
            println!("report: {:?}", &report);

            let file = File::create("flamegraph.svg").unwrap();
            let mut options = pprof::flamegraph::Options::default();
            report.flamegraph_with_options(file, &mut options).unwrap();
        };
    }

    /// Tests collapse_counters_by_series_and_timestamp() using 10,000 metrics where every even
    /// metric is a collapsible one and the remaining have unique, existing timestamps.
    #[test]
    fn collapse_identical_metrics_counter_even() {
        let counter_value = 1.0;

        let mut actual = vec![];
        let mut expected = vec![];

        let now = Utc::now();

        let n = 10_000;

        expected.push(create_counter("basic", counter_value * (n as f64 / 2.0)));

        for i in 0..n {
            if i % 2 == 0 {
                actual.push(create_counter("basic", counter_value));
            } else {
                actual.push(
                    create_counter("basic", counter_value)
                        .with_timestamp(Some(now + Duration::seconds(i))),
                );
                expected.push(
                    create_counter("basic", counter_value)
                        .with_timestamp(Some(now + Duration::seconds(i))),
                );
            }
        }

        collapse_counters_by_series_and_timestamp(&mut actual);

        let expected_value = if let MetricValue::Counter { value } = expected[0].value() {
            *value
        } else {
            1.0
        };

        let actual_value = if let MetricValue::Counter { value } = actual[0].value() {
            *value
        } else {
            0.0
        };

        assert_eq!(expected_value, actual_value);
        assert_eq!(expected.len(), actual.len());
    }

    /// Tests collapse_counters_by_series_and_timestamp() using 10,000 metrics where every no
    /// metric is a collapsible- all have unique, existing timestamps.
    #[test]
    fn collapse_identical_metrics_counter_large_all_unique() {
        let counter_value = 1.0;

        let mut actual = vec![];

        let now = Utc::now();

        let n = 10_000;

        for i in 0..n {
            actual.push(
                create_counter("basic", counter_value)
                    .with_timestamp(Some(now + Duration::seconds(i))),
            );
        }

        let guard = pprof::ProfilerGuardBuilder::default()
            .frequency(100)
            .blocklist(&["libc", "libgcc", "pthread", "vdso"])
            .build()
            .unwrap();

        let expected = actual.clone();

        collapse_counters_by_series_and_timestamp(&mut actual);

        if let Ok(report) = guard.report().build() {
            //println!("report: {:?}", &report);

            let file = File::create("flamegraph.svg").unwrap();
            let mut options = pprof::flamegraph::Options::default();
            //options.image_width = Some(2500);
            report.flamegraph_with_options(file, &mut options).unwrap();
        };

        assert_eq!(expected, actual);
    }

    #[derive(Eq, Ord, PartialEq, PartialOrd)]
    struct MetricCollapseSort {
        metric_type: &'static str,
        metric_name: String,
        metric_ts: Option<DateTime<Utc>>,
    }

    impl MetricCollapseSort {
        fn from_metric(metric: &Metric) -> Self {
            Self {
                metric_type: metric.value().as_name(),
                metric_name: metric.name().to_string(),
                metric_ts: metric.timestamp(),
            }
        }
    }

    fn collapse_dedup_fn(left: &mut Metric, right: &mut Metric) -> bool {
        let series_eq = left.series() == right.series();
        let timestamp_eq = left.timestamp() == right.timestamp();
        if !series_eq || !timestamp_eq {
            return false;
        }

        match (left.value_mut(), right.value_mut()) {
            (
                MetricValue::Counter { value: left_value },
                MetricValue::Counter { value: right_value },
            ) => {
                // NOTE: The docs for `dedup_by` specify that if `left`/`right` are equal, then
                // `left` is the element that gets removed.
                *right_value += *left_value;
                true
            }
            // Only counters can be equivalent for the purpose of this test.
            _ => false,
        }
    }

    proptest! {
        #[test]
        fn test_counter_collapse(mut actual in arb_collapsible_metrics()) {
            let mut expected_output = actual.clone();
            expected_output.sort_by_cached_key(MetricCollapseSort::from_metric);
            expected_output.dedup_by(collapse_dedup_fn);

            collapse_counters_by_series_and_timestamp(&mut actual);
            actual.sort_by_cached_key(MetricCollapseSort::from_metric);

            prop_assert_eq!(expected_output, actual);
        }
    }
}
