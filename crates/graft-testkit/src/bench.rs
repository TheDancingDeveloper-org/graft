//! Timing and results tables for the pilot harness (work item G1.16).
//!
//! A [`Results`] table holds one [`Row`] per scenario and system, with
//! p50, p95 and mean over its samples. It renders as `results.tsv` (one
//! header line, tab-separated, stable column order) and as a Markdown
//! table. [`spread`] compares the same table from several runs, for the
//! "reproducible within 10% across three runs" acceptance check.

use std::fmt::Write as _;
use std::time::{Duration, Instant};

/// Runs `f` and returns its result and how long it took.
pub fn timed<T>(f: impl FnOnce() -> T) -> (T, Duration) {
    let start = Instant::now();
    let out = f();
    (out, start.elapsed())
}

/// The value at quantile `q` (0..=1) of `sorted`, by nearest rank.
/// `None` when there are no samples.
pub fn quantile(sorted: &[f64], q: f64) -> Option<f64> {
    if sorted.is_empty() {
        return None;
    }
    let rank = (q.clamp(0.0, 1.0) * sorted.len() as f64).ceil() as usize;
    Some(sorted[rank.clamp(1, sorted.len()) - 1])
}

/// p50, p95 and mean of a set of samples.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Summary {
    /// Samples.
    pub n: usize,
    /// Median, nearest rank.
    pub p50: f64,
    /// 95th percentile, nearest rank.
    pub p95: f64,
    /// Mean.
    pub mean: f64,
}

impl Summary {
    /// Summarizes `samples`; `None` when there are none.
    pub fn of(samples: &[f64]) -> Option<Summary> {
        let mut sorted = samples.to_vec();
        sorted.sort_by(f64::total_cmp);
        Some(Summary {
            n: sorted.len(),
            p50: quantile(&sorted, 0.5)?,
            p95: quantile(&sorted, 0.95)?,
            mean: sorted.iter().sum::<f64>() / sorted.len() as f64,
        })
    }
}

/// Durations as milliseconds, for [`Summary::of`].
pub fn millis(samples: &[Duration]) -> Vec<f64> {
    samples.iter().map(|d| d.as_secs_f64() * 1e3).collect()
}

/// One measured scenario on one system.
#[derive(Clone, Debug, PartialEq)]
pub struct Row {
    /// Scenario, for example `log --path`.
    pub scenario: String,
    /// `graft` or `git`.
    pub system: String,
    /// Unit of the values: `ms` or `%`.
    pub unit: String,
    /// The summary.
    pub summary: Summary,
    /// What was measured, briefly (a path, a range). Never file contents.
    pub note: String,
}

/// The header of `results.tsv`.
pub const TSV_HEADER: &str = "scenario\tsystem\tunit\tn\tp50\tp95\tmean\tnote";

/// A results table.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Results {
    /// Rows, in the order they were measured.
    pub rows: Vec<Row>,
}

impl Results {
    /// Adds a row for `samples`; a scenario with no samples is skipped.
    pub fn push(&mut self, scenario: &str, system: &str, unit: &str, samples: &[f64], note: &str) {
        if let Some(summary) = Summary::of(samples) {
            self.rows.push(Row {
                scenario: scenario.to_owned(),
                system: system.to_owned(),
                unit: unit.to_owned(),
                summary,
                note: one_line(note),
            });
        }
    }

    /// Adds a row of millisecond timings.
    pub fn push_times(&mut self, scenario: &str, system: &str, samples: &[Duration], note: &str) {
        self.push(scenario, system, "ms", &millis(samples), note);
    }

    /// The row for `scenario` on `system`.
    pub fn get(&self, scenario: &str, system: &str) -> Option<&Row> {
        self.rows
            .iter()
            .find(|r| r.scenario == scenario && r.system == system)
    }

    /// `results.tsv`: the header, then one line per row.
    pub fn to_tsv(&self) -> String {
        let mut out = String::from(TSV_HEADER);
        out.push('\n');
        for r in &self.rows {
            let s = &r.summary;
            let _ = writeln!(
                out,
                "{}\t{}\t{}\t{}\t{:.3}\t{:.3}\t{:.3}\t{}",
                r.scenario, r.system, r.unit, s.n, s.p50, s.p95, s.mean, r.note
            );
        }
        out
    }

    /// Parses what [`Results::to_tsv`] wrote.
    pub fn from_tsv(text: &str) -> Result<Results, String> {
        let mut lines = text.lines();
        if lines.next() != Some(TSV_HEADER) {
            return Err("not a results.tsv: wrong header".into());
        }
        let mut rows = Vec::new();
        for (n, line) in lines.enumerate().filter(|(_, l)| !l.is_empty()) {
            let f: Vec<&str> = line.split('\t').collect();
            let bad = || format!("results.tsv line {}: malformed", n + 2);
            if f.len() != 8 {
                return Err(bad());
            }
            let num = |s: &str| s.parse::<f64>().map_err(|_| bad());
            rows.push(Row {
                scenario: f[0].to_owned(),
                system: f[1].to_owned(),
                unit: f[2].to_owned(),
                summary: Summary {
                    n: f[3].parse().map_err(|_| bad())?,
                    p50: num(f[4])?,
                    p95: num(f[5])?,
                    mean: num(f[6])?,
                },
                note: f[7].to_owned(),
            });
        }
        Ok(Results { rows })
    }

    /// The rows as a Markdown table, with graft against git where both ran.
    pub fn to_markdown(&self) -> String {
        let mut out = String::from(
            "| Scenario | System | n | p50 | p95 | mean | graft ÷ git (p50) | Note |\n\
             | --- | --- | ---: | ---: | ---: | ---: | ---: | --- |\n",
        );
        for r in &self.rows {
            let s = &r.summary;
            let ratio = match (r.system.as_str(), self.get(&r.scenario, "git")) {
                ("graft", Some(git)) if git.summary.p50 > 0.0 && r.unit == git.unit => {
                    format!("{:.2}", s.p50 / git.summary.p50)
                }
                _ => String::new(),
            };
            let u = &r.unit;
            let _ = writeln!(
                out,
                "| {} | {} | {} | {:.2} {u} | {:.2} {u} | {:.2} {u} | {ratio} | {} |",
                r.scenario,
                r.system,
                s.n,
                s.p50,
                s.p95,
                s.mean,
                r.note.replace('|', "\\|")
            );
        }
        out
    }
}

/// Raw samples by scenario and system, in first-seen order, so runs can be
/// pooled before they are summarized.
#[derive(Clone, Debug, Default)]
pub struct Samples {
    series: Vec<Series>,
}

#[derive(Clone, Debug)]
struct Series {
    scenario: String,
    system: String,
    unit: String,
    note: String,
    values: Vec<f64>,
}

impl Samples {
    /// Records one value. The unit and note of the first value win.
    pub fn add(&mut self, scenario: &str, system: &str, unit: &str, note: &str, value: f64) {
        match self
            .series
            .iter_mut()
            .find(|s| s.scenario == scenario && s.system == system)
        {
            Some(s) => s.values.push(value),
            None => self.series.push(Series {
                scenario: scenario.to_owned(),
                system: system.to_owned(),
                unit: unit.to_owned(),
                note: note.to_owned(),
                values: vec![value],
            }),
        }
    }

    /// Records one duration in milliseconds.
    pub fn add_time(&mut self, scenario: &str, system: &str, note: &str, d: Duration) {
        self.add(scenario, system, "ms", note, d.as_secs_f64() * 1e3);
    }

    /// The values of one series.
    pub fn values(&self, scenario: &str, system: &str) -> &[f64] {
        self.series
            .iter()
            .find(|s| s.scenario == scenario && s.system == system)
            .map_or(&[], |s| &s.values)
    }

    /// Every run's samples in one set.
    pub fn pool(runs: &[Samples]) -> Samples {
        let mut out = Samples::default();
        for run in runs {
            for s in &run.series {
                for v in &s.values {
                    out.add(&s.scenario, &s.system, &s.unit, &s.note, *v);
                }
            }
        }
        out
    }

    /// The summary table.
    pub fn results(&self) -> Results {
        let mut r = Results::default();
        for s in &self.series {
            r.push(&s.scenario, &s.system, &s.unit, &s.values, &s.note);
        }
        r
    }
}

/// Tabs and newlines would break a TSV field.
fn one_line(s: &str) -> String {
    s.replace(['\t', '\n', '\r'], " ")
}

/// How far one row's p50 strays across runs: `(max - min) / median` of the
/// per-run p50s.
#[derive(Clone, Debug, PartialEq)]
pub struct Spread {
    /// Scenario.
    pub scenario: String,
    /// System.
    pub system: String,
    /// The p50 of each run, in run order.
    pub p50s: Vec<f64>,
    /// `(max - min) / median`, 0 when the median is 0.
    pub relative: f64,
}

/// The spread of every row present in all of `runs`. Rows missing from
/// some run are left out.
pub fn spread(runs: &[Results]) -> Vec<Spread> {
    let Some(first) = runs.first() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for row in &first.rows {
        let p50s: Option<Vec<f64>> = runs
            .iter()
            .map(|r| r.get(&row.scenario, &row.system).map(|x| x.summary.p50))
            .collect();
        let Some(p50s) = p50s else { continue };
        let mut sorted = p50s.clone();
        sorted.sort_by(f64::total_cmp);
        let median = quantile(&sorted, 0.5).unwrap_or(0.0);
        let relative = if median > 0.0 {
            (sorted[sorted.len() - 1] - sorted[0]) / median
        } else {
            0.0
        };
        out.push(Spread {
            scenario: row.scenario.clone(),
            system: row.system.clone(),
            p50s,
            relative,
        });
    }
    out
}

/// The spreads as a Markdown table, flagging those over `limit` (0.10 for
/// the acceptance check).
pub fn spread_markdown(spreads: &[Spread], limit: f64) -> String {
    let mut out = String::from(
        "| Scenario | System | p50 per run | Spread | Within limit |\n| --- | --- | --- | ---: | --- |\n",
    );
    for s in spreads {
        let runs: Vec<String> = s.p50s.iter().map(|v| format!("{v:.2}")).collect();
        let _ = writeln!(
            out,
            "| {} | {} | {} | {:.1}% | {} |",
            s.scenario,
            s.system,
            runs.join(", "),
            s.relative * 100.0,
            if s.relative <= limit { "yes" } else { "**no**" }
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quantiles_use_the_nearest_rank() {
        let v: Vec<f64> = (1..=20).map(f64::from).collect();
        assert_eq!(quantile(&v, 0.5), Some(10.0));
        assert_eq!(quantile(&v, 0.95), Some(19.0));
        assert_eq!(quantile(&v, 1.0), Some(20.0));
        assert_eq!(quantile(&v, 0.0), Some(1.0));
        assert_eq!(quantile(&[], 0.5), None);
        assert_eq!(quantile(&[7.0], 0.95), Some(7.0));
    }

    #[test]
    fn summary_sorts_its_samples() {
        let s = Summary::of(&[3.0, 1.0, 2.0]).expect("samples");
        assert_eq!((s.n, s.p50, s.p95, s.mean), (3, 2.0, 3.0, 2.0));
        assert!(Summary::of(&[]).is_none());
    }

    #[test]
    fn tsv_round_trips_and_strips_tabs_from_notes() {
        let mut r = Results::default();
        r.push_times(
            "log --path",
            "graft",
            &[Duration::from_millis(4), Duration::from_millis(6)],
            "src/a\tb.rs",
        );
        r.push("decompress share", "graft", "%", &[12.5], "");
        r.push("empty", "git", "ms", &[], "skipped");
        let text = r.to_tsv();
        assert!(text.starts_with(TSV_HEADER));
        assert_eq!(text.lines().count(), 3, "the empty scenario is skipped");
        let back = Results::from_tsv(&text).expect("parses");
        assert_eq!(back.rows.len(), 2);
        assert_eq!(back.rows[0].note, "src/a b.rs");
        assert_eq!(back.rows[0].summary.p50, 4.0);
        assert!(Results::from_tsv("nope\n").is_err());
    }

    #[test]
    fn markdown_shows_the_ratio_against_git() {
        let mut r = Results::default();
        r.push("diff", "graft", "ms", &[2.0], "");
        r.push("diff", "git", "ms", &[8.0], "");
        let md = r.to_markdown();
        assert!(md.contains("| diff | graft | 1 | 2.00 ms | 2.00 ms | 2.00 ms | 0.25 |"));
    }

    #[test]
    fn spread_is_range_over_median() {
        let run = |p50: f64| {
            let mut r = Results::default();
            r.push("clone", "graft", "ms", &[p50], "");
            r.push("only-first", "git", "ms", &[1.0], "");
            r
        };
        let mut second = run(10.0);
        second.rows.retain(|r| r.scenario == "clone");
        let s = spread(&[run(10.0), second, run(11.0)]);
        assert_eq!(s.len(), 1, "rows missing from a run are left out");
        assert!((s[0].relative - 0.1).abs() < 1e-9);
        assert!(spread_markdown(&s, 0.10).contains("| yes |"));
        assert!(spread_markdown(&s, 0.05).contains("**no**"));
    }
}
