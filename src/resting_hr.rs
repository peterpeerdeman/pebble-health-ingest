//! Server-side resting heart rate estimate, computed from `pebble_minute`
//! history instead of trusting the watch's own naive daily-minimum query
//! (`Health.metric.query({metric:"heart rate", aggregation:"min"})` in
//! src/embeddedjs/main.js).
//!
//! Calibrated against ~20 days of paired Fitbit data (its own reported
//! `restingHeartRate` alongside its raw intraday heart rate, both already in
//! this InfluxDB): the daily minimum and low-percentile approaches (5th,
//! 10th, 25th percentile of heart rate during sleep) all undershot Fitbit's
//! number by 7-9 bpm, consistent with those being thrown off by PPG sensor
//! noise/artifacts, which read artificially *low*. The median of heart rate
//! samples taken while still was the closest (~5.5 bpm mean error) and, being
//! a median rather than an extreme, is inherently robust to exactly that kind
//! of noise. This is not an attempt to reproduce Fitbit's number exactly -
//! that needs its proprietary algorithm and its specific sensor's noise
//! profile, neither of which is available here - just to get a
//! physiologically reasonable, comparably-scaled value from what Pebble
//! actually gives us.
//!
//! "Still" is approximated as `vmc` (accelerometer vector magnitude count)
//! at or below a threshold, since Pebble has no sleep-stage data to weight by
//! the way Fitbit's algorithm reportedly does.

#[derive(Clone, Copy, Debug, Default)]
pub struct MinuteSample {
    pub hr: Option<i64>,
    pub vmc: Option<i64>,
    pub steps: Option<i64>,
}

/// `None` if fewer than `min_samples` qualifying (still, HR-bearing) minutes
/// are available - better to omit the field than publish a noisy estimate
/// from a handful of points.
pub fn compute(samples: &[MinuteSample], vmc_max: i64, min_samples: usize) -> Option<i64> {
    let mut still_hr: Vec<i64> = samples
        .iter()
        .filter(|s| s.steps.unwrap_or(0) == 0)
        .filter(|s| s.vmc.is_some_and(|v| v <= vmc_max))
        .filter_map(|s| s.hr)
        .filter(|&hr| hr > 0)
        .collect();

    if still_hr.len() < min_samples {
        return None;
    }

    still_hr.sort_unstable();
    Some(median(&still_hr))
}

/// Integer median, rounding down on an even count (matches Fitbit's own
/// values, which are always whole bpm).
fn median(sorted: &[i64]) -> i64 {
    let n = sorted.len();
    if n % 2 == 1 {
        sorted[n / 2]
    } else {
        (sorted[n / 2 - 1] + sorted[n / 2]) / 2
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(hr: Option<i64>, vmc: Option<i64>, steps: Option<i64>) -> MinuteSample {
        MinuteSample { hr, vmc, steps }
    }

    #[test]
    fn none_when_too_few_qualifying_samples() {
        let samples: Vec<_> = (0..5)
            .map(|_| sample(Some(50), Some(10), Some(0)))
            .collect();
        assert_eq!(compute(&samples, 40, 30), None);
    }

    #[test]
    fn none_on_empty_input() {
        assert_eq!(compute(&[], 40, 1), None);
    }

    #[test]
    fn filters_out_high_motion_minutes() {
        let mut samples = vec![sample(Some(45), Some(10), Some(0)); 30];
        // A burst of movement with a much higher (exercising) heart rate -
        // must not pull the estimate up.
        samples.extend(vec![sample(Some(120), Some(500), Some(80)); 30]);
        assert_eq!(compute(&samples, 40, 30), Some(45));
    }

    #[test]
    fn filters_out_minutes_with_steps_even_if_vmc_is_low() {
        // vmc can lag a brief movement; steps is the stricter, cheaper signal.
        let mut samples = vec![sample(Some(45), Some(10), Some(0)); 30];
        samples.extend(vec![sample(Some(90), Some(10), Some(3)); 30]);
        assert_eq!(compute(&samples, 40, 30), Some(45));
    }

    #[test]
    fn ignores_minutes_without_an_hr_sample() {
        // HR is only sampled every ~10 min in the background, so most still
        // minutes have no reading at all - those must not count toward
        // min_samples or be treated as zero.
        let mut samples = vec![sample(None, Some(10), Some(0)); 300];
        samples.extend(vec![sample(Some(48), Some(10), Some(0)); 30]);
        assert_eq!(compute(&samples, 40, 30), Some(48));
    }

    #[test]
    fn takes_the_median_not_the_minimum() {
        let mut samples: Vec<_> = (40..=49)
            .flat_map(|hr| vec![sample(Some(hr), Some(5), Some(0)); 3])
            .collect();
        // 30 samples, hr 40..49 repeated 3x each, sorted median sits between 44/45.
        samples.sort_by_key(|s| s.hr);
        assert_eq!(compute(&samples, 40, 30), Some(44));
    }

    #[test]
    fn median_rounds_down_on_even_count() {
        assert_eq!(median(&[40, 41]), 40);
        assert_eq!(median(&[40, 41, 42]), 41);
        assert_eq!(median(&[10, 20, 30, 40]), 25);
    }
}
