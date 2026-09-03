//! What a series of timings says, and whether two series differ.
//!
//! [Issue #179](https://github.com/myuon/cove/issues/179) records that this
//! repository could not write the sentence a refactor most wants to write —
//! "no statistically meaningful regression" — because `cove-bench` reported
//! `{min, mean, max}` and nothing else. Three numbers are not a claim; they
//! are three numbers a reader eyeballs against a band they remember.
//!
//! # Why the median and the quartiles
//!
//! A wall-time series has a floor and no ceiling. The machine cannot run the
//! benchmark faster than the machine runs it, and it can always run it slower
//! — a descheduled turn, a migration between cores, a neighbour waking up. So
//! the failure mode of a benchmark timing is a sample that is too *large*, and
//! the mean is the statistic that moves furthest when one arrives. The median
//! does not move at all until half the samples are affected, which is the
//! property that makes it the right summary of a series taken on a machine
//! that is not perfectly quiet.
//!
//! That is the argument from the shape of the failure, and it is the one to
//! rely on. The argument from skew is weaker than it looks, and it is recorded
//! here because it was checked rather than assumed. The only distributions
//! this repository has written down are the nine rows of the
//! calling-convention matrix, taken at nine samples a row on a quiet machine
//! and reported as `{median, min, max}`. That table measured the backend
//! ADR 0034 deleted and went with it; `docs/VM_ARCHITECTURE.md` restates the
//! counts below, and the table itself is in git history at commit `6e90085`.
//! If those series were reliably right-skewed then `max - median` would
//! exceed `median - min` on most of them. It exceeds it on three rows, falls
//! short on five, and ties on one. So "benchmark timings are right-skewed" is
//! not a claim this data supports and is not what justifies the median here.
//! What justifies it is that one bad sample must not be able to move the
//! number a decision is made on — and that argument does not need the skew,
//! because a statistic robust to an outlier in either direction is what a
//! decision wants either way.
//!
//! Two caveats on that check, because it is weaker than a real one. Three
//! order statistics are not a distribution, and nine samples put both
//! extremes deep in the tails where they are noisiest. A stronger check is
//! now possible and was not before: `wall_ns.samples` records every timing, so
//! whoever next takes a run on a quiet machine can look at the actual shape
//! rather than at its extremes.
//!
//! The mean, the minimum and the maximum are all still reported. The mean
//! because [ADR 0012](../../../docs/adr/0012-performance-gate-and-native-backend.md)
//! says wall time is reported as `{min, mean, max}` and a reader of that
//! format must keep finding it; the extremes because they are the cheapest
//! way to see that a series went wrong, and a summary that hides the one
//! run that took four times as long is worse than no summary.
//!
//! # Why a comparison, and not just a spread
//!
//! A spread on one run says how noisy that run was. It does not say whether
//! this build is slower than that build, which is the actual question, and
//! [issue #126](https://github.com/myuon/cove/issues/126) is the reason it
//! has to be asked against a *fixed commit* rather than against the parent:
//! three changes each individually inside the noise summed to 19%. A
//! comparison against whatever ran last cannot see that, and a comparison
//! against a recorded baseline can.
//!
//! So [`Comparison`] takes the baseline's samples and this run's samples and
//! answers with the shift between their medians and an interval around it.
//! The verdict is read off the interval: an interval that excludes zero is a
//! difference that cleared the noise, and one that contains zero is not — and
//! in that case the interval's own width is the honest bound on what the
//! change could have cost, which is the number to quote instead of claiming
//! there was no effect.

use std::collections::BTreeMap;

/// How many samples a side needs before a comparison will call anything.
///
/// Six, and the reason is exact rather than a rule of thumb. The
/// distribution-free interval for a median is a pair of order statistics, and
/// the widest one available is the whole range: `[min, max]` fails to cover
/// the true median only when every sample falls on the same side of it, which
/// for `n` samples has probability `2 * (1/2)^n`. Asking for 95% confidence
/// therefore asks for `2^(n-1) >= 20`, which first holds at `n = 6`. Below
/// six samples there is no 95% statement to be made about one median, let
/// alone about the difference of two, so a comparison that made one would be
/// inventing it.
///
/// This is a floor and not a recommendation. `docs/VM_ARCHITECTURE.md` takes
/// its tables at fifteen.
pub const MIN_SAMPLES: usize = 6;

/// How many resamples the interval below is built from.
///
/// Ten thousand is enough that the 2.5th and 97.5th percentiles of the
/// resampled statistic are stable to well under a tenth of a percent, and it
/// costs microseconds at the sample counts this harness produces.
const RESAMPLES: usize = 10_000;

/// The confidence the reported interval carries.
pub const CONFIDENCE: f64 = 0.95;

/// The seed the resampling starts from.
///
/// Fixed, so that the same two series always produce the same interval and
/// the same verdict. A tool that answered differently on a rerun of
/// identical data would be asking to be rerun until it agreed.
const SEED: u64 = 0x0000_0C0F_FEE5_EED0;

/// A series of samples and the order statistics read off it.
///
/// Holds the samples twice: sorted, because everything below is an order
/// statistic, and in the order the run took them, because that is what the
/// report carries and a baseline is only useful if the samples themselves
/// survive into it — a summary cannot be compared against a later run with
/// anything better than arithmetic on summaries.
///
/// The two copies exist for a reason a sorted one alone cannot serve.
/// [Issue #205](https://github.com/myuon/cove/issues/205) asks whether a
/// run-to-run disagreement is drift *within* a suite or *between* two of
/// them, and a sorted array cannot answer it: whether the slow samples were
/// the first three or the last three is exactly the information sorting
/// throws away. Nothing in this file reads the run order — every statistic
/// below is an order statistic and the bootstrap resamples with replacement
/// — so keeping it costs one vector and buys a question that could not be
/// asked before.
pub struct Stats {
    sorted: Vec<u64>,
    /// The same samples, in the order the run took them. Reported; not read.
    taken: Vec<u64>,
    mean: u64,
}

impl Stats {
    /// Summarizes `samples`, which must not be empty.
    pub fn of(samples: &[u64]) -> Stats {
        assert!(!samples.is_empty(), "a series has at least one sample");
        let taken = samples.to_vec();
        let mut sorted = taken.clone();
        sorted.sort_unstable();
        let sum: u128 = sorted.iter().map(|&n| u128::from(n)).sum();
        let mean = (sum / sorted.len() as u128) as u64;
        Stats {
            sorted,
            taken,
            mean,
        }
    }

    /// The samples, in ascending order.
    pub fn samples(&self) -> &[u64] {
        &self.sorted
    }

    pub fn min(&self) -> u64 {
        self.sorted[0]
    }

    pub fn max(&self) -> u64 {
        self.sorted[self.sorted.len() - 1]
    }

    /// The arithmetic mean, truncated. Kept because ADR 0012 names it.
    pub fn mean(&self) -> u64 {
        self.mean
    }

    pub fn median(&self) -> f64 {
        quantile(&self.sorted, 0.5)
    }

    pub fn p25(&self) -> f64 {
        quantile(&self.sorted, 0.25)
    }

    pub fn p75(&self) -> f64 {
        quantile(&self.sorted, 0.75)
    }

    /// The interquartile range: the width of the middle half of the series.
    ///
    /// This is the spread to read. Unlike `max - min` it does not grow just
    /// because the series got longer and so had more chances to catch one
    /// bad sample.
    pub fn iqr(&self) -> f64 {
        self.p75() - self.p25()
    }

    /// The summary, as a JSON object.
    ///
    /// `min`, `mean` and `max` are first and keep their names, because ADR
    /// 0012 describes this object as `{min, mean, max}` and a reader written
    /// against that description must keep working.
    pub fn to_json(&self) -> String {
        format!(
            "{{\"min\":{},\"mean\":{},\"max\":{},\"p25\":{:.1},\"median\":{:.1},\"p75\":{:.1},\"iqr\":{:.1}}}",
            self.min(),
            self.mean(),
            self.max(),
            self.p25(),
            self.median(),
            self.p75(),
            self.iqr(),
        )
    }

    /// The summary with every sample beside it.
    ///
    /// What makes a recorded run a baseline rather than a memory of one: a
    /// comparison needs the samples, not a summary of them, because the
    /// interval it reports is built by resampling them.
    ///
    /// **In the order the run took them**, which is strictly more than a
    /// sorted array says and costs a reader who wanted the sorted one a
    /// `sort`. Nothing that compares two runs is affected — every statistic
    /// here is an order statistic and [`Comparison::of`] sorts what it is
    /// given — and what it buys is that a reader can see *when* in a series a
    /// slow sample arrived, which is the difference between a machine that
    /// drifted and a benchmark that is noisy.
    pub fn to_json_with_samples(&self) -> String {
        let mut json = self.to_json();
        json.pop();
        let samples: Vec<String> = self.taken.iter().map(u64::to_string).collect();
        json.push_str(&format!(",\"samples\":[{}]}}", samples.join(",")));
        json
    }
}

/// The `p`th quantile of an ascending series, by linear interpolation
/// between the two order statistics that bracket it.
///
/// This is the definition NumPy and R use by default (R's type 7), chosen
/// because it is the one a reader who reaches for another tool to check this
/// one will get from it. At `p = 0.5` it is the ordinary median: the middle
/// sample of an odd series, the average of the two middle samples of an even
/// one.
pub fn quantile(sorted: &[u64], p: f64) -> f64 {
    assert!(!sorted.is_empty(), "a series has at least one sample");
    let h = (sorted.len() - 1) as f64 * p;
    let lower = h.floor() as usize;
    let upper = h.ceil() as usize;
    let below = sorted[lower] as f64;
    let above = sorted[upper] as f64;
    below + (h - lower as f64) * (above - below)
}

/// What a comparison concluded.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Verdict {
    /// One side has fewer than [`MIN_SAMPLES`] samples, so no interval is
    /// available and nothing is claimed. The shift is still reported, as a
    /// number to look at rather than a number to act on.
    Underpowered,
    /// The interval contains zero. The difference, whatever it is, is not
    /// separable from the noise of these two runs.
    InsideTheNoise,
    /// The interval lies entirely above zero: this run is slower.
    Regression,
    /// The interval lies entirely below zero: this run is faster.
    Improvement,
}

impl Verdict {
    pub fn as_str(self) -> &'static str {
        match self {
            Verdict::Underpowered => "underpowered",
            Verdict::InsideTheNoise => "inside the noise",
            Verdict::Regression => "regression",
            Verdict::Improvement => "improvement",
        }
    }
}

/// This run's median against a baseline's, and how sure of the difference the
/// two series allow anyone to be.
#[derive(Clone, Copy, Debug)]
pub struct Comparison {
    pub baseline_median_ns: f64,
    pub median_ns: f64,
    /// The shift between the two medians, as a percentage of the baseline's.
    /// Positive is slower.
    pub delta_pct: f64,
    /// The interval around `delta_pct`, at [`CONFIDENCE`].
    pub low_pct: f64,
    pub high_pct: f64,
    pub verdict: Verdict,
}

impl Comparison {
    /// Compares two series of wall times.
    ///
    /// The interval is a percentile bootstrap on the relative shift between
    /// the medians: resample each side with replacement, take the two
    /// medians, and record `(current - baseline) / baseline`. The 2.5th and
    /// 97.5th percentiles of ten thousand such records are the interval.
    ///
    /// A bootstrap rather than a `t` test because nothing here is normal and
    /// the statistic is a median; a percentile bootstrap rather than a rank
    /// test because what a refactor needs is not only "is there a
    /// difference" but "how large could it be", and the interval answers
    /// both while a `p` value answers only the first.
    pub fn of(baseline: &[u64], current: &[u64]) -> Comparison {
        let mut base_sorted = baseline.to_vec();
        base_sorted.sort_unstable();
        let mut current_sorted = current.to_vec();
        current_sorted.sort_unstable();

        let baseline_median_ns = quantile(&base_sorted, 0.5);
        let median_ns = quantile(&current_sorted, 0.5);
        let delta_pct = if baseline_median_ns > 0.0 {
            100.0 * (median_ns - baseline_median_ns) / baseline_median_ns
        } else {
            0.0
        };

        // A zero baseline median has no relative shift to report, and a
        // series shorter than the floor has no interval; both are
        // `Underpowered`, which is this type's way of saying "the number
        // beside this is not a claim".
        if baseline.len() < MIN_SAMPLES || current.len() < MIN_SAMPLES || baseline_median_ns <= 0.0
        {
            return Comparison {
                baseline_median_ns,
                median_ns,
                delta_pct,
                low_pct: f64::NAN,
                high_pct: f64::NAN,
                verdict: Verdict::Underpowered,
            };
        }

        let mut shifts = Vec::with_capacity(RESAMPLES);
        let mut rng = Rng::new(SEED);
        let mut base_draw = vec![0u64; base_sorted.len()];
        let mut current_draw = vec![0u64; current_sorted.len()];
        for _ in 0..RESAMPLES {
            for slot in base_draw.iter_mut() {
                *slot = base_sorted[rng.below(base_sorted.len())];
            }
            for slot in current_draw.iter_mut() {
                *slot = current_sorted[rng.below(current_sorted.len())];
            }
            base_draw.sort_unstable();
            current_draw.sort_unstable();
            let b = quantile(&base_draw, 0.5);
            let c = quantile(&current_draw, 0.5);
            shifts.push(if b > 0.0 { 100.0 * (c - b) / b } else { 0.0 });
        }
        shifts.sort_by(|a, b| a.partial_cmp(b).expect("no sample is NaN"));

        let tail = (1.0 - CONFIDENCE) / 2.0;
        let low_pct = percentile_f64(&shifts, tail);
        let high_pct = percentile_f64(&shifts, 1.0 - tail);
        let verdict = if low_pct > 0.0 {
            Verdict::Regression
        } else if high_pct < 0.0 {
            Verdict::Improvement
        } else {
            Verdict::InsideTheNoise
        };

        Comparison {
            baseline_median_ns,
            median_ns,
            delta_pct,
            low_pct,
            high_pct,
            verdict,
        }
    }

    /// The comparison as one line of the harness's JSON output.
    ///
    /// `kind` is `comparison` rather than the kind of the row compared, so
    /// that a reader filtering on `kind` keeps finding exactly the rows it
    /// was finding; `of` says which kind this line is about.
    pub fn to_json(self, benchmark: &str, of: &str, backend: &str) -> String {
        let interval = if self.verdict == Verdict::Underpowered {
            "\"ci_low_pct\":null,\"ci_high_pct\":null".to_string()
        } else {
            format!(
                "\"ci_low_pct\":{:.2},\"ci_high_pct\":{:.2}",
                self.low_pct, self.high_pct
            )
        };
        format!(
            "{{\"benchmark\":\"{}\",\"kind\":\"comparison\",\"of\":\"{}\",\"backend\":\"{}\",\"baseline_median_ns\":{:.1},\"median_ns\":{:.1},\"delta_pct\":{:.2},{},\"confidence\":{},\"verdict\":\"{}\"}}",
            benchmark,
            of,
            backend,
            self.baseline_median_ns,
            self.median_ns,
            self.delta_pct,
            interval,
            CONFIDENCE,
            self.verdict.as_str(),
        )
    }
}

/// The `p`th percentile of an ascending series of `f64`, by the same
/// interpolation [`quantile`] uses.
fn percentile_f64(sorted: &[f64], p: f64) -> f64 {
    let h = (sorted.len() - 1) as f64 * p;
    let lower = h.floor() as usize;
    let upper = h.ceil() as usize;
    sorted[lower] + (h - lower as f64) * (sorted[upper] - sorted[lower])
}

/// SplitMix64, which is four lines and needs no dependency.
///
/// The resampling wants a stream that is the same every time and does not
/// want cryptographic quality; this is the standard seeding generator for
/// exactly that job.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Rng {
        Rng(seed)
    }

    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// A uniform index below `n`. The modulo bias is under one part in 2^58
    /// for the sample counts this harness produces.
    fn below(&mut self, n: usize) -> usize {
        (self.next_u64() % n as u64) as usize
    }
}

// ------------------------------------------------------------ the baseline

/// Which row of a report a comparison is about.
///
/// The benchmark, the kind of measurement, and the backend, which is exactly
/// what ADR 0019 requires every number here to carry — two rows that agree on
/// all three are the same measurement taken twice, and no two rows of one
/// report agree on all three.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct RowKey {
    pub benchmark: String,
    pub kind: String,
    pub backend: String,
}

/// A previous run of this harness, read back as the samples it recorded.
pub struct Baseline {
    rows: BTreeMap<RowKey, Vec<u64>>,
}

impl Baseline {
    /// Reads a baseline from this harness's own JSONL output.
    ///
    /// This parses the format this file writes and nothing else. It is not a
    /// JSON parser and does not pretend to be one: it looks for the three
    /// string fields that name a row and for the sample array inside
    /// `wall_ns`, and it skips any line that does not carry all four. Lines
    /// that legitimately do not — an `unsupported` refusal, a
    /// `trace_overhead` ratio, a `comparison` from an earlier run — are
    /// therefore skipped rather than treated as errors.
    ///
    /// A baseline written by an older build, before `samples` was reported,
    /// parses as empty. That is the wanted behaviour: comparing against
    /// summaries would mean inventing the spread that made them.
    pub fn parse(text: &str) -> Result<Baseline, String> {
        let mut rows = BTreeMap::new();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let (Some(benchmark), Some(kind), Some(backend)) = (
                string_field(line, "benchmark"),
                string_field(line, "kind"),
                string_field(line, "backend"),
            ) else {
                continue;
            };
            let Some(samples) = wall_samples(line) else {
                continue;
            };
            if samples.is_empty() {
                continue;
            }
            rows.insert(
                RowKey {
                    benchmark,
                    kind,
                    backend,
                },
                samples,
            );
        }
        if rows.is_empty() {
            return Err(
                "no rows with a `wall_ns.samples` array; a baseline must be the JSON output \
of a `cove-bench` run new enough to record its samples"
                    .to_string(),
            );
        }
        Ok(Baseline { rows })
    }

    /// How many rows the baseline carries.
    pub fn len(&self) -> usize {
        self.rows.len()
    }

    /// The samples recorded for one row, if the baseline has it.
    ///
    /// A row the baseline does not have is not an error: benchmarks are added
    /// and the VM learns to run ones it previously refused, and a baseline
    /// taken before either is still a baseline for every row it does have.
    pub fn samples(&self, benchmark: &str, kind: &str, backend: &str) -> Option<&[u64]> {
        self.rows
            .get(&RowKey {
                benchmark: benchmark.to_string(),
                kind: kind.to_string(),
                backend: backend.to_string(),
            })
            .map(Vec::as_slice)
    }
}

/// The value of `"<name>":"..."` in a line this harness wrote.
fn string_field(line: &str, name: &str) -> Option<String> {
    let needle = format!("\"{name}\":\"");
    let start = line.find(&needle)? + needle.len();
    let rest = &line[start..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

/// The `samples` array inside this line's `wall_ns` object.
///
/// Anchored on `wall_ns` so that a future field carrying samples of its own
/// cannot be read as if it were the wall time.
fn wall_samples(line: &str) -> Option<Vec<u64>> {
    let start = line.find("\"wall_ns\":{")?;
    let rest = &line[start..];
    let end = rest.find('}')?;
    let object = &rest[..end];
    let needle = "\"samples\":[";
    let list_start = object.find(needle)? + needle.len();
    let list = &object[list_start..];
    let list_end = list.find(']')?;
    let list = &list[..list_end];
    if list.trim().is_empty() {
        return Some(Vec::new());
    }
    list.split(',')
        .map(|item| item.trim().parse::<u64>().ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A deterministic series with a known median and a known spread.
    ///
    /// Every test below runs on synthetic data, and that is deliberate: the
    /// question these tests ask is whether the estimator recovers an answer
    /// that is known in advance, and a real timing series has no known
    /// answer to recover. A machine under load cannot make any of these
    /// fail.
    fn series(centre: f64, spread_pct: f64, n: usize, seed: u64) -> Vec<u64> {
        let mut rng = Rng::new(seed);
        (0..n)
            .map(|_| {
                // Uniform on [-spread, +spread] about the centre.
                let unit = (rng.next_u64() >> 11) as f64 / (1u64 << 53) as f64;
                let jitter = (unit * 2.0 - 1.0) * spread_pct / 100.0;
                (centre * (1.0 + jitter)).round() as u64
            })
            .collect()
    }

    #[test]
    fn quantiles_are_the_textbook_ones() {
        let odd: Vec<u64> = (1..=9).collect();
        assert_eq!(quantile(&odd, 0.5), 5.0);
        assert_eq!(quantile(&odd, 0.25), 3.0);
        assert_eq!(quantile(&odd, 0.75), 7.0);
        assert_eq!(quantile(&odd, 0.0), 1.0);
        assert_eq!(quantile(&odd, 1.0), 9.0);

        let even: Vec<u64> = vec![1, 2, 3, 4];
        assert_eq!(quantile(&even, 0.5), 2.5);
        assert_eq!(quantile(&even, 0.25), 1.75);
        assert_eq!(quantile(&even, 0.75), 3.25);

        let one = vec![42];
        assert_eq!(quantile(&one, 0.5), 42.0);
        assert_eq!(quantile(&one, 0.25), 42.0);
    }

    #[test]
    fn the_median_ignores_the_slow_run_and_the_mean_does_not() {
        // Nine samples at 100, and one run that took ten times as long --
        // the shape of the failure a wall-time series actually has.
        let clean: Vec<u64> = vec![100; 9];
        let polluted: Vec<u64> = vec![100, 100, 100, 100, 100, 100, 100, 100, 100, 1000];

        assert_eq!(Stats::of(&clean).median(), 100.0);
        assert_eq!(Stats::of(&polluted).median(), 100.0);

        assert_eq!(Stats::of(&clean).mean(), 100);
        assert_eq!(Stats::of(&polluted).mean(), 190);
    }

    /// Issue #205 needs to know *when* a slow sample arrived, and a sorted
    /// array cannot say. The order statistics stay what they were.
    #[test]
    fn the_report_carries_the_series_in_the_order_it_was_taken() {
        let stats = Stats::of(&[30, 10, 20]);
        assert!(
            stats
                .to_json_with_samples()
                .contains("\"samples\":[30,10,20]"),
            "{}",
            stats.to_json_with_samples()
        );
        assert_eq!(stats.median(), 20.0);
        assert_eq!(stats.min(), 10);
        assert_eq!(stats.max(), 30);
    }

    #[test]
    fn stats_keeps_what_adr_0012_named() {
        let stats = Stats::of(&[30, 10, 20]);
        assert_eq!(stats.min(), 10);
        assert_eq!(stats.max(), 30);
        assert_eq!(stats.mean(), 20);
        assert_eq!(stats.samples(), &[10, 20, 30]);
        let json = stats.to_json();
        assert!(
            !json.contains("samples"),
            "the summary alone carries no samples: {json}"
        );
        for field in ["\"min\":10", "\"mean\":20", "\"max\":30"] {
            assert!(json.contains(field), "{json} is missing {field}");
        }
        for field in ["\"p25\":", "\"median\":", "\"p75\":", "\"iqr\":"] {
            assert!(json.contains(field), "{json} is missing {field}");
        }
    }

    #[test]
    fn the_iqr_is_the_middle_half() {
        // A tight middle with one outlier on each side: the range doubles,
        // the interquartile range does not move.
        let tight: Vec<u64> = vec![100, 101, 102, 103, 104, 105, 106, 107, 108];
        let mut wide = tight.clone();
        wide[0] = 1;
        wide[8] = 500;
        assert_eq!(Stats::of(&tight).iqr(), Stats::of(&wide).iqr());
        assert!(Stats::of(&wide).max() - Stats::of(&wide).min() > 400);
    }

    #[test]
    fn a_run_against_itself_is_inside_the_noise() {
        let baseline = series(100_000.0, 6.0, 15, 1);
        let current = series(100_000.0, 6.0, 15, 2);
        let comparison = Comparison::of(&baseline, &current);
        assert_eq!(comparison.verdict, Verdict::InsideTheNoise);
        assert!(
            comparison.low_pct <= 0.0 && comparison.high_pct >= 0.0,
            "the interval {:.2}..{:.2} should contain zero",
            comparison.low_pct,
            comparison.high_pct
        );
    }

    #[test]
    fn a_twenty_percent_regression_clears_the_band() {
        let baseline = series(100_000.0, 6.0, 15, 3);
        let current = series(120_000.0, 6.0, 15, 4);
        let comparison = Comparison::of(&baseline, &current);
        assert_eq!(comparison.verdict, Verdict::Regression);
        assert!(
            (comparison.delta_pct - 20.0).abs() < 5.0,
            "the shift should be near 20%, was {:.2}%",
            comparison.delta_pct
        );
        assert!(
            comparison.low_pct > 0.0,
            "the interval {:.2}..{:.2} should exclude zero",
            comparison.low_pct,
            comparison.high_pct
        );
        assert!(
            comparison.low_pct <= comparison.delta_pct
                && comparison.delta_pct <= comparison.high_pct,
            "the interval {:.2}..{:.2} should bracket the shift {:.2}",
            comparison.low_pct,
            comparison.high_pct,
            comparison.delta_pct
        );
    }

    #[test]
    fn a_twenty_percent_improvement_clears_the_band_the_other_way() {
        let baseline = series(100_000.0, 6.0, 15, 5);
        let current = series(80_000.0, 6.0, 15, 6);
        let comparison = Comparison::of(&baseline, &current);
        assert_eq!(comparison.verdict, Verdict::Improvement);
        assert!(comparison.high_pct < 0.0);
    }

    #[test]
    fn a_one_percent_shift_under_six_percent_noise_is_not_a_claim() {
        // This is the case issue #126 is made of: a change smaller than the
        // band `docs/VM_ARCHITECTURE.md` records for `arith`. Fifteen
        // samples a side must not call it.
        let baseline = series(100_000.0, 6.0, 15, 7);
        let current = series(101_000.0, 6.0, 15, 8);
        let comparison = Comparison::of(&baseline, &current);
        assert_eq!(comparison.verdict, Verdict::InsideTheNoise);
    }

    #[test]
    fn enough_samples_resolve_what_few_cannot() {
        // The same 3% shift under the same 6% noise: not resolvable at
        // fifteen samples a side, resolvable at two hundred. This is what
        // `--iterations` buys, and it is why the flag is the answer to "how
        // many runs" rather than a second concept beside it.
        let few = Comparison::of(
            &series(100_000.0, 6.0, 15, 9),
            &series(103_000.0, 6.0, 15, 10),
        );
        assert_eq!(few.verdict, Verdict::InsideTheNoise);

        let many = Comparison::of(
            &series(100_000.0, 6.0, 200, 11),
            &series(103_000.0, 6.0, 200, 12),
        );
        assert_eq!(many.verdict, Verdict::Regression);
    }

    #[test]
    fn below_the_floor_nothing_is_claimed() {
        // One sample a side and a 50% difference: still not a claim, because
        // one sample has no spread and an interval built from it would be a
        // fabrication. This is the shape `--iterations 1` has, which is what
        // CI runs.
        let one = Comparison::of(&[100_000], &[150_000]);
        assert_eq!(one.verdict, Verdict::Underpowered);
        assert!((one.delta_pct - 50.0).abs() < 1e-9);
        assert!(one.low_pct.is_nan() && one.high_pct.is_nan());

        // Five is still below the floor; six is the first that is not.
        let five = Comparison::of(
            &series(100_000.0, 6.0, 5, 13),
            &series(150_000.0, 6.0, 5, 14),
        );
        assert_eq!(five.verdict, Verdict::Underpowered);
        let six = Comparison::of(
            &series(100_000.0, 6.0, 6, 15),
            &series(150_000.0, 6.0, 6, 16),
        );
        assert_eq!(six.verdict, Verdict::Regression);
    }

    #[test]
    fn the_floor_is_where_a_ninety_five_percent_statement_first_exists() {
        // `[min, max]` misses the median with probability 2 * (1/2)^n. The
        // floor is the smallest `n` for which that is at most 5%.
        let covers = |n: u32| 1.0 - 2.0 * 0.5_f64.powi(n as i32) >= CONFIDENCE;
        assert!(!covers(MIN_SAMPLES as u32 - 1));
        assert!(covers(MIN_SAMPLES as u32));
    }

    #[test]
    fn the_same_data_always_gives_the_same_answer() {
        let baseline = series(100_000.0, 6.0, 15, 17);
        let current = series(104_000.0, 6.0, 15, 18);
        let first = Comparison::of(&baseline, &current);
        let second = Comparison::of(&baseline, &current);
        assert_eq!(first.verdict, second.verdict);
        assert_eq!(first.low_pct.to_bits(), second.low_pct.to_bits());
        assert_eq!(first.high_pct.to_bits(), second.high_pct.to_bits());
    }

    #[test]
    fn order_does_not_change_the_answer() {
        let baseline = series(100_000.0, 6.0, 15, 19);
        let mut shuffled = baseline.clone();
        shuffled.reverse();
        let current = series(112_000.0, 6.0, 15, 20);
        let straight = Comparison::of(&baseline, &current);
        let reversed = Comparison::of(&shuffled, &current);
        assert_eq!(straight.low_pct.to_bits(), reversed.low_pct.to_bits());
        assert_eq!(straight.high_pct.to_bits(), reversed.high_pct.to_bits());
    }

    #[test]
    fn a_baseline_round_trips_through_the_report_format() {
        let samples = vec![7u64, 3, 5, 9];
        let stats = Stats::of(&samples);
        let line = format!(
            "{{\"benchmark\":\"field\",\"kind\":\"vm\",\"backend\":\"vm\",\"iterations\":4,\"wall_ns\":{},\"fuel_spent\":1,\"ok\":true}}",
            stats.to_json_with_samples()
        );
        let baseline = Baseline::parse(&line).expect("the line parses");
        assert_eq!(baseline.len(), 1);
        // In the order the run took them, not sorted: the report carries the
        // series as it happened, and a comparison sorts what it is given.
        assert_eq!(
            baseline.samples("field", "vm", "vm"),
            Some([7u64, 3, 5, 9].as_slice())
        );
        assert_eq!(baseline.samples("field", "interpreter", "ast"), None);
    }

    #[test]
    fn lines_without_samples_are_skipped_rather_than_failing() {
        let text = "\
{\"benchmark\":\"pure\",\"kind\":\"unsupported\",\"backend\":\"vm\",\"what\":\"a `spawn`\",\"ok\":false}
{\"benchmark\":\"pure\",\"kind\":\"trace_overhead\",\"backend\":\"vm\",\"untraced_wall_ns\":1,\"traced_wall_ns\":2,\"overhead_ratio\":2.0}

{\"benchmark\":\"pure\",\"kind\":\"vm\",\"backend\":\"vm\",\"wall_ns\":{\"min\":1,\"mean\":2,\"max\":3,\"samples\":[1,2,3]},\"ok\":true}";
        let baseline = Baseline::parse(text).expect("one row parses");
        assert_eq!(baseline.len(), 1);
        assert_eq!(
            baseline.samples("pure", "vm", "vm"),
            Some([1u64, 2, 3].as_slice())
        );
    }

    #[test]
    fn a_baseline_from_a_build_that_recorded_no_samples_is_refused() {
        let old = "{\"benchmark\":\"pure\",\"kind\":\"vm\",\"backend\":\"vm\",\"iterations\":5,\"wall_ns\":{\"min\":1,\"mean\":2,\"max\":3},\"ok\":true}";
        assert!(Baseline::parse(old).is_err());
        assert!(Baseline::parse("").is_err());
    }

    #[test]
    fn an_underpowered_comparison_reports_no_interval() {
        let json = Comparison::of(&[10], &[20]).to_json("pure", "vm", "vm");
        assert!(json.contains("\"verdict\":\"underpowered\""));
        assert!(json.contains("\"ci_low_pct\":null"));
        assert!(json.contains("\"kind\":\"comparison\""));
        assert!(json.contains("\"of\":\"vm\""));
    }
}
