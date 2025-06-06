use num::{Num, NumCast};

/// This type is responsible for calculating stats in a rolling fashion.
/// By rolling, it means that there is already some stats calculated
/// which needs to be further aggregated. This is commonly the case when
/// the sampling is done at a higher precision interval (say 1 minute) and
/// then further aggregated (say 1 hour).
///
/// For example the number of lines written per hour is collected as new
/// write requests come in. However, the bucket [`crate::bucket::EventsBucket`]
/// holds `lines` as [`crate::stats::Stats<u64>`], to hold min/max/avg lines
/// written per minute. Then when taking samples per minute to calculate
/// hourly aggregates, [`RollingStats<T>`] is used. To see how it is calculated
/// see the [`RollingStats<T>::update`] method
#[derive(Debug, Default)]
pub(crate) struct RollingStats<T> {
    pub min: T,
    pub max: T,
    pub avg: T,
    pub num_samples: usize,
}

impl<T: Default + Num + Copy + NumCast + PartialOrd> RollingStats<T> {
    /// Update the rolling stats [`Self::min`]/[`Self::max`]/[`Self::avg`] using
    /// reference to an higher precision stats that is passed in. This is usually a
    /// per minute interval stats. One thing to note here is the [`Self::num_samples`]
    /// is updated locally here to calculate the rolling average for usually
    /// an hour for a metric. Refer to [`crate::metrics::Writes`] or
    /// [`crate::metrics::Queries`] to see how it is used
    pub(crate) fn update(&mut self, higher_precision_stats: &Stats<T>) -> Option<()> {
        if self.num_samples == 0 {
            self.min = higher_precision_stats.min;
            self.max = higher_precision_stats.max;
            self.avg = higher_precision_stats.avg;
        } else {
            let (new_min, new_max, new_avg) = rollup_stats(
                self.min,
                self.max,
                self.avg,
                self.num_samples,
                higher_precision_stats.min,
                higher_precision_stats.max,
                higher_precision_stats.avg,
            )?;
            self.min = new_min;
            self.max = new_max;
            self.avg = new_avg;
        }
        self.num_samples += 1;
        Some(())
    }

    pub(crate) fn reset(&mut self) {
        *self = RollingStats::default();
    }
}

/// This is basic stats to keep a tab on min/max/avg for a specific
/// metric
#[derive(Debug, Default)]
pub(crate) struct Stats<T> {
    pub min: T,
    pub max: T,
    pub avg: T,
    pub num_samples: usize,
}

impl<T: Default + Num + Copy + NumCast + PartialOrd> Stats<T> {
    /// Update the [`Self::min`]/[`Self::max`]/[`Self::avg`] from a
    /// new value that is sampled.
    pub(crate) fn update(&mut self, new_val: T) -> Option<()> {
        if self.num_samples == 0 {
            self.min = new_val;
            self.max = new_val;
            self.avg = new_val;
        } else {
            let (new_min, new_max, new_avg) =
                stats(self.min, self.max, self.avg, self.num_samples, new_val)?;
            self.min = new_min;
            self.max = new_max;
            self.avg = new_avg;
        }
        self.num_samples += 1;
        Some(())
    }

    pub(crate) fn reset(&mut self) {
        *self = Stats::default();
    }
}

/// Generic function to calculate min/max/avg from another set of stats.
/// This function works for all types of numbers (unsigned/signed/floats).
/// It calculates min/max/avg by using already calculated min/max/avg for
/// possibly a higher resolution.
///
/// # Example
/// Let's say we're looking at the stats for number of lines written.
/// And we have 1st sample's minimum was 20 and the 3rd sample's
/// minimum was 10. This means in the 1st sample for a whole minute
/// 20 was the minimum number of lines written in a single request and in
/// the 3rd sample (3rd minute) 10 is the minimum number of lines written
/// in a single request. These are already stats at per minute interval, when we
/// calculate the minimum number of lines for the whole hour we compare the samples
/// taken at per minute interval for whole hour. In this case 10 will be the new
/// minimum for the whole hour.
fn rollup_stats<T: Num + Copy + NumCast + PartialOrd>(
    current_min: T,
    current_max: T,
    current_avg: T,
    current_num_samples: usize,

    new_min: T,
    new_max: T,
    new_avg: T,
) -> Option<(T, T, T)> {
    let min = min(current_min, new_min);
    let max = max(current_max, new_max);
    let avg = avg(current_num_samples, current_avg, new_avg)?;
    Some((min, max, avg))
}

/// Generic function to calculate min/max/avg from a new sampled value.
/// This function works for all types of numbers (unsigned/signed/floats).
/// One thing to note here is the average function, it is an incremental average
/// to avoid holding all the samples in memory.
fn stats<T: Num + Copy + NumCast + PartialOrd>(
    current_min: T,
    current_max: T,
    current_avg: T,
    current_num_samples: usize,
    new_value: T,
) -> Option<(T, T, T)> {
    let min = min(current_min, new_value);
    let max = max(current_max, new_value);
    let avg = avg(current_num_samples, current_avg, new_value)?;
    Some((min, max, avg))
}

/// Average function that returns average based on the type
/// provided. u64 for example will return avg as u64. This probably
/// is fine as we don't really need it to be a precise average.
/// For example, memory consumed measured in MB can be rounded as u64
fn avg<T: Num + Copy + NumCast + PartialOrd>(
    current_num_samples: usize,
    current_avg: T,
    new_value: T,
) -> Option<T> {
    // NB: num::cast(current_num_samples).unwrap() should have been enough,
    //     given we always reset metrics. However, if we decide to not reset
    //     metrics without retrying then it is better to bubble up the `Option`
    //     to indicate this cast did not work
    let new_num_samples = num::cast(current_num_samples.wrapping_add(1))?;
    let zero = num::cast(0).unwrap();
    if new_num_samples == zero {
        return None;
    }

    // To avoid overflows,
    //     use this idea: https://math.stackexchange.com/questions/106700/incremental-averaging/1836447#1836447
    // formula:
    //     (current_avg) + ((new_value - current_avg) / new_num_items)
    //
    // Special case (new_value < current_avg) formula:
    //     (current_avg) - ((current_avg - new_value) / new_num_items)
    if new_value < current_avg {
        let partial = current_avg.sub(new_value);
        let new_avg = current_avg - (partial.div(new_num_samples));
        Some(new_avg)
    } else {
        let partial = new_value.sub(current_avg);
        let new_avg = current_avg + (partial.div(new_num_samples));
        Some(new_avg)
    }
}

fn min<T: Num + PartialOrd + Copy>(current_min: T, new_value: T) -> T {
    if new_value < current_min {
        return new_value;
    };
    current_min
}

fn max<T: Num + PartialOrd + Copy>(current_max: T, new_value: T) -> T {
    if new_value > current_max {
        return new_value;
    };
    current_max
}

#[cfg(test)]
mod tests {
    use observability_deps::tracing::info;
    use proptest::prelude::*;

    use super::*;

    #[test]
    fn min_float_test() {
        assert_eq!(1.0, min(1.0, 2.0));
    }

    #[test]
    fn min_num_test() {
        assert_eq!(1, min(1, 2));
    }

    #[test]
    fn max_num_test() {
        assert_eq!(2, max(1, 2));
    }

    #[test]
    fn max_float_test() {
        assert_eq!(2.0, max(1.0, 2.0));
    }

    #[test]
    fn avg_num_test() {
        assert_eq!(Some(2), avg(3, 2, 4));
    }

    #[test_log::test(test)]
    fn avg_float_test() {
        let avg_floats = avg(3, 2.0, 4.0);
        info!(avg = ?avg_floats, "average float");
        assert_eq!(Some(2.5), avg_floats);
    }

    #[test_log::test(test)]
    fn avg_float_test_max() {
        let avg_floats = avg(usize::MAX, 2.0, 4.0);
        info!(avg = ?avg_floats, "average float");
        assert_eq!(None, avg_floats);
    }

    #[test_log::test(test)]
    fn avg_num_test_max() {
        let avg_nums = avg(usize::MAX, 2u64, 4);
        assert_eq!(None, avg_nums);
    }

    #[test_log::test(test)]
    fn stats_test() {
        let stats = stats(2.0, 135.5, 25.5, 37, 25.0);
        assert!(stats.is_some());
        let (min, max, avg) = stats.unwrap();
        info!(min = ?min, max = ?max, avg = ?avg, "stats >>");
        assert_eq!((2.0, 135.5, 25.486842105263158), (min, max, avg));
    }

    #[test_log::test(test)]
    fn rollup_stats_test() {
        let stats = rollup_stats(2.0, 135.5, 25.5, 37, 25.0, 150.0, 32.0);
        assert!(stats.is_some());
        let (min, max, avg) = stats.unwrap();
        info!(min = ?min, max = ?max, avg = ?avg, "stats >>");

        assert_eq!((2.0, 150.0, 25.67105263157895), (min, max, avg));
    }

    #[test_log::test(test)]
    fn avg_test_new_value_lower() {
        let rolling_avg = avg(2, 110, 20u64);
        assert!(rolling_avg.is_some());
        assert_eq!(80, rolling_avg.unwrap());

        let rolling_avg = avg(3, 80, 22u64);
        assert!(rolling_avg.is_some());
        assert_eq!(66, rolling_avg.unwrap());
    }

    #[test_log::test(test)]
    fn avg_test() {
        let avg = avg(0, 4339, 0u64);
        assert!(avg.is_some());
    }

    proptest! {
        #[test_log::test(test)]
        fn prop_test_stats_no_panic_u64(
            min in 0u64..10000,
            max in 0u64..10000,
            curr_avg in 0u64..10000,
            num_samples in 0usize..10000,
            new_value in 0u64..100000,
        ) {
            stats(min, max, curr_avg, num_samples, new_value);
        }

        #[test]
        fn prop_test_stats_no_panic_f32(
            min in 0.0f32..10000.0,
            max in 0.0f32..10000.0,
            curr_avg in 0.0f32..10000.0,
            num_samples in 0usize..10000,
            new_value in 0.0f32..100000.0,
        ) {
            stats(min, max, curr_avg, num_samples, new_value);
        }

        #[test]
        fn prop_test_stats_no_panic_f64(
            min in 0.0f64..10000.0,
            max in 0.0f64..10000.0,
            curr_avg in 0.0f64..10000.0,
            num_samples in 0usize..10000,
            new_value in 0.0f64..100000.0,
        ) {
            stats(min, max, curr_avg, num_samples, new_value);
        }
    }
}
