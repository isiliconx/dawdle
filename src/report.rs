//! Rendering: terminal tables, sparklines, markdown for PR comments, and JSON.
//!
//! dawdle's output has one job, which is to be read by someone who did not
//! write the tool and is in a hurry. That drives every choice here: regressions
//! first and largest-first, a sparkline so a trend is visible without scrolling,
//! and a plain-text summary line at the end telling you the next command to run.

use serde::Serialize;

use crate::db::{RunRow, RunTrend, StepRow, Thresholds, Trend};
use crate::fmt;

/// A fixed-width text table with per-column alignment.
pub struct Table {
    headers: Vec<String>,
    rows: Vec<Vec<String>>,
    right_align: Vec<bool>,
}

impl Table {
    pub fn new(headers: &[&str]) -> Self {
        Self {
            headers: headers.iter().map(|h| h.to_string()).collect(),
            rows: Vec::new(),
            right_align: headers.iter().map(|_| false).collect(),
        }
    }

    pub fn right_align(&mut self, columns: &[usize]) {
        for index in columns {
            if *index < self.right_align.len() {
                self.right_align[*index] = true;
            }
        }
    }

    pub fn push(&mut self, row: Vec<String>) {
        self.rows.push(row);
    }

    fn widths(&self) -> Vec<usize> {
        let mut widths: Vec<usize> = self
            .headers
            .iter()
            .map(|h| fmt::strip_ansi(h).chars().count())
            .collect();
        for row in &self.rows {
            for (index, cell) in row.iter().enumerate() {
                if index < widths.len() {
                    let len = fmt::strip_ansi(cell).chars().count();
                    widths[index] = widths[index].max(len);
                }
            }
        }
        widths
    }

    fn render_line(&self, widths: &[usize], separator: &str) -> String {
        let mut out = String::new();
        for (index, width) in widths.iter().enumerate() {
            if index > 0 {
                out.push_str(separator);
            }
            out.push_str(&fmt::pad(
                &fmt::ellipsis(&self.headers[index], *width),
                *width,
            ));
        }
        out.trim_end().to_string()
    }

    pub fn render(&self) -> String {
        let widths = self.widths();
        let mut out = String::new();
        out.push_str(&fmt::bold(&self.render_line(&widths, "  ")));
        out.push('\n');
        for row in &self.rows {
            let mut line = String::new();
            for (index, width) in widths.iter().enumerate() {
                if index > 0 {
                    line.push_str("  ");
                }
                let cell = row.get(index).cloned().unwrap_or_default();
                let cell = fmt::ellipsis(&cell, *width);
                if self.right_align.get(index).copied().unwrap_or(false) {
                    // Pad manually so ANSI codes do not break alignment.
                    let visible = fmt::strip_ansi(&cell).chars().count();
                    let pad = width.saturating_sub(visible);
                    line.push_str(&" ".repeat(pad));
                    line.push_str(&cell);
                } else {
                    line.push_str(&fmt::pad(&cell, *width));
                }
            }
            out.push_str(line.trim_end());
            out.push('\n');
        }
        out
    }

    pub fn markdown(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!("| {} |\n", self.headers.join(" | ")));
        let rule: Vec<String> = self
            .right_align
            .iter()
            .map(|right| if *right { "---:" } else { "---" })
            .map(str::to_string)
            .collect();
        out.push_str(&format!("| {} |\n", rule.join(" | ")));
        for row in &self.rows {
            let cells: Vec<String> = (0..self.headers.len())
                .map(|index| {
                    // Pipes inside a cell would break the table for anyone
                    // pasting it into a PR comment.
                    fmt::strip_ansi(row.get(index).map(|s| s.as_str()).unwrap_or(""))
                        .replace('|', "\\|")
                })
                .collect();
            out.push_str(&format!("| {} |\n", cells.join(" | ")));
        }
        out
    }
}

/// Render a series as a single-line sparkline using block characters.
///
/// Sparklines are here because the number they encode — "this step has been flat
/// for thirty runs and then jumped" — is the thing a reader is scanning for, and
/// eight glyphs say it faster than two numbers and a percentage sign.
pub fn sparkline(values: &[f64]) -> String {
    const LEVELS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    if values.is_empty() {
        return String::new();
    }
    let max = values.iter().cloned().fold(0.0_f64, f64::max);
    if max <= 0.0 {
        return "▁".repeat(values.len());
    }
    values
        .iter()
        .map(|value| {
            let ratio = (value / max).clamp(0.0, 1.0);
            let index = (ratio * (LEVELS.len() - 1) as f64).round() as usize;
            LEVELS[index.min(LEVELS.len() - 1)]
        })
        .collect()
}

/// Like [`sparkline`], but marks one column so the reader can see *which*
/// sample is the one that regressed rather than having to count blocks.
pub fn sparkline_marked(series: &[f64], marker: Option<usize>) -> String {
    let plain = sparkline(series);
    let Some(index) = marker else {
        return plain;
    };
    let mut out = String::new();
    for (position, glyph) in plain.chars().enumerate() {
        if position == index {
            out.push_str(&fmt::bold(&glyph.to_string()));
        } else {
            out.push(glyph);
        }
    }
    out
}

/// Colour a percentage change. Improvement is green, regression red, and
/// anything without a baseline stays dim rather than being coloured as if it
/// meant something.
pub fn colour_delta(delta: Option<f64>) -> String {
    match delta {
        None => fmt::dim("—"),
        Some(pct) if pct >= 0.0 => fmt::red(&format!("{:+.0}%", pct)),
        Some(pct) => fmt::green(&format!("{pct:+.0}%")),
    }
}

fn short_sha(sha: &str) -> String {
    if sha.is_empty() {
        String::new()
    } else {
        sha.chars().take(7).collect()
    }
}

fn trend_row(trend: &Trend, series: &[f64]) -> Vec<String> {
    let onset = match (trend.onset_sha.as_deref(), trend.onset_at) {
        (Some(sha), Some(at)) => format!("{} {}", short_sha(sha), fmt::ago(at)),
        _ => String::new(),
    };
    let name = if trend.is_wrapper {
        fmt::dim(&trend.key)
    } else {
        trend.key.clone()
    };
    let flag = if trend.is_new {
        fmt::yellow("new")
    } else if trend.is_gone {
        fmt::yellow("gone")
    } else {
        String::new()
    };
    vec![
        name,
        sparkline_marked(series, trend.onset_index),
        fmt::ms(trend.baseline_ms as i64),
        fmt::ms(trend.recent_ms as i64),
        colour_delta(trend.delta_pct),
        // How many times this key ran recently, next to its duration. A step
        // that is 3x slower because it now runs 3x as often needs a different
        // fix than one that got 3x slower per invocation, and the two columns
        // together are the only way to tell them apart.
        if trend.invocations_recent >= 1.0 {
            format!("{:.0}", trend.invocations_recent)
        } else {
            fmt::dim("1")
        },
        // Peak RSS of the single largest process, not the sum: a build running
        // forty workers in parallel can have a huge total and a small high-water
        // mark, and it is the high-water mark that decides whether CI OOMs.
        fmt::bytes(trend.peak_rss_kb),
        onset,
        flag,
    ]
}

const TREND_HEADERS: &[&str] = &[
    "step", "trend", "before", "now", "change", "n", "peak rss", "since", "",
];

/// The full `dawdle report` view.
pub struct ReportView {
    pub subtitle: String,
    pub regressions: Vec<(Trend, Vec<f64>)>,
    pub unchanged: Vec<(Trend, Vec<f64>)>,
    pub run_trend: Option<RunTrend>,
    pub total_runs: usize,
    pub metric_label: String,
    pub thresholds: Thresholds,
    pub empty_hint: Option<String>,
}

impl ReportView {
    pub fn render(&self) -> String {
        if self.regressions.is_empty() && self.unchanged.is_empty() {
            let mut out = format!("{}\n\n", fmt::bold(&self.subtitle));
            out.push_str(&format!(
                "{}\n\n",
                self.empty_hint
                    .clone()
                    .unwrap_or_else(|| "No steps recorded in these runs.".to_string())
            ));
            return out;
        }

        let mut out = format!("{}\n\n", fmt::bold(&self.subtitle));

        // Run-level trend first: if the whole build got slower, the reader
        // should know that before they start blaming a step.
        if let Some(run_trend) = &self.run_trend {
            let changed = run_trend
                .delta_pct
                .map(|pct| pct.abs() >= 1.0)
                .unwrap_or(false);
            if changed {
                let mut line = format!(
                    "  {} {} → {}  {}",
                    fmt::bold("run total"),
                    fmt::ms(run_trend.baseline_ms as i64),
                    fmt::ms(run_trend.recent_ms as i64),
                    colour_delta(run_trend.delta_pct)
                );
                if let (Some(sha), Some(run_id)) =
                    (run_trend.onset_sha.as_deref(), run_trend.onset_run_id)
                {
                    line.push_str(&format!(
                        "  {}",
                        fmt::dim(&format!("since run #{run_id} ({})", short_sha(sha)))
                    ));
                }
                out.push_str(&format!("{line}  over {} runs\n\n", run_trend.samples));
            }
        }

        if !self.regressions.is_empty() {
            out.push_str(&format!("{}\n", fmt::red(&fmt::bold("REGRESSIONS"))));
            let mut table = Table::new(TREND_HEADERS);
            table.right_align(&[2, 3, 5, 6]);
            for (trend, series) in &self.regressions {
                table.push(trend_row(trend, series));
            }
            out.push_str(&indent(&table.render(), "  "));
            out.push('\n');
            out.push_str(&format!(
                "  {} {}\n\n",
                fmt::dim("next:"),
                fmt::blue(&format!(
                    "dawdle blame {}",
                    shell_quote(&self.regressions[0].0.key)
                ))
            ));
        }

        if !self.unchanged.is_empty() {
            out.push_str(&format!(
                "{}\n",
                fmt::dim(&format!("UNCHANGED ({})", self.unchanged.len()))
            ));
            let mut table = Table::new(TREND_HEADERS);
            table.right_align(&[2, 3, 5, 6]);
            for (trend, series) in &self.unchanged {
                table.push(trend_row(trend, series));
            }
            out.push_str(&indent(&table.render(), "  "));
            out.push('\n');
        }

        let _ = self.metric_label;
        out
    }

    /// One line a CI job can print as its summary.
    pub fn summary_line(&self, current: Option<&RunRow>) -> String {
        match (&self.run_trend, current) {
            (Some(trend), Some(run)) => {
                match trend.delta_pct {
                    Some(pct) if pct.abs() >= 5.0 => {
                        format!(
                    "dawdle: run {} was {} vs a recent median of {} ({}), {} regressed step(s)",
                    fmt::ms(run.duration_ms),
                    fmt::signed_pct(pct),
                    fmt::ms(trend.recent_ms as i64),
                    trend.onset_sha.as_deref().map(short_sha).unwrap_or_else(|| "no-sha".into()),
                    self.regressions.len()
                )
                    }
                    _ => format!(
                        "dawdle: run {} was within {} of the recent median, {} regressed step(s)",
                        fmt::ms(run.duration_ms),
                        "1%",
                        self.regressions.len()
                    ),
                }
            }
            (None, Some(run)) => {
                format!(
                    "dawdle: run {} recorded, not enough history to compare",
                    fmt::ms(run.duration_ms)
                )
            }
            _ => format!(
                "dawdle: {} run(s) compared, no current run",
                self.total_runs
            ),
        }
    }

    /// Markdown, shaped for a PR comment: the same findings, plus the one line
    /// that says whether this run was slower than the recent baseline.
    pub fn markdown(&self, current: Option<&RunRow>) -> String {
        let mut out = String::new();
        out.push_str("### dawdle build report\n\n");

        if let Some(run) = current {
            let verdict = match &self.run_trend {
                Some(trend) => match trend.delta_pct {
                    Some(pct) if pct >= 5.0 => format!(
                        "This run took **{}**, {} vs a recent median of {}.",
                        fmt::ms(run.duration_ms),
                        colour_delta(Some(pct)),
                        fmt::ms(trend.recent_ms as i64)
                    ),
                    Some(pct) if pct <= -5.0 => format!(
                        "This run took **{}**, {} vs a recent median of {}.",
                        fmt::ms(run.duration_ms),
                        colour_delta(Some(pct)),
                        fmt::ms(trend.recent_ms as i64)
                    ),
                    _ => format!("This run took **{}**.", fmt::ms(run.duration_ms)),
                },
                None => format!("This run took **{}**.", fmt::ms(run.duration_ms)),
            };
            out.push_str(&verdict);
            out.push_str("\n\n");
        }

        if self.regressions.is_empty() {
            out.push_str("No step regressed beyond the configured threshold.\n");
            return out;
        }

        let mut table = Table::new(&["step", "before", "now", "change", "regressed at"]);
        for (trend, _) in &self.regressions {
            table.push(vec![
                trend.key.clone(),
                fmt::ms(trend.baseline_ms as i64),
                fmt::ms(trend.recent_ms as i64),
                fmt::signed_pct(trend.delta_pct.unwrap_or(0.0)),
                trend
                    .onset_sha
                    .as_deref()
                    .map(short_sha)
                    .unwrap_or_else(|| "—".to_string()),
            ]);
        }
        out.push_str(&table.markdown());
        out.push('\n');
        out.push_str(&format!(
            "_dawdle {} · metric {} · threshold {}% or {}ms, over {} runs_\n",
            crate::cli::VERSION,
            self.metric_label,
            self.thresholds.pct as i64,
            self.thresholds.min_abs_ms,
            self.total_runs,
        ));
        out
    }
}

/// Minimal shell quoting so the suggested `dawdle blame "..."` command can be
/// pasted without the reader having to think about it.
pub fn shell_quote(value: &str) -> String {
    if !value.is_empty()
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./:=+@,".contains(c))
    {
        return value.to_string();
    }
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

fn indent(text: &str, prefix: &str) -> String {
    text.lines()
        .map(|line| {
            if line.is_empty() {
                String::new()
            } else {
                format!("{prefix}{line}")
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// JSON payloads. Stable field names, because the point of these is to be piped
/// into `jq` by someone who does not read this source.
#[derive(Debug, Serialize)]
pub struct RunJson {
    pub id: i64,
    pub started_at: i64,
    pub duration_ms: i64,
    pub label: String,
    pub command: String,
    pub git_sha: String,
    pub git_branch: String,
    pub exit_code: i32,
    pub succeeded: bool,
    pub cpu_ms: i64,
    pub peak_rss_kb: i64,
    pub processes: i64,
    pub coverage_pct: Option<f64>,
    pub parallelism: Option<f64>,
}

impl From<&RunRow> for RunJson {
    fn from(row: &RunRow) -> Self {
        Self {
            id: row.id,
            started_at: row.started_at,
            duration_ms: row.duration_ms,
            label: row.label.clone(),
            command: row.command.clone(),
            git_sha: row.git_sha.clone(),
            git_branch: row.git_branch.clone(),
            exit_code: row.exit_code,
            succeeded: row.succeeded,
            cpu_ms: row.cpu_ms,
            peak_rss_kb: row.peak_rss_kb,
            processes: row.proc_count,
            coverage_pct: row.coverage_pct,
            parallelism: row.parallelism,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct StepJson {
    pub key: String,
    pub invocations: i64,
    pub cpu_ms: i64,
    pub busy_ms: i64,
    pub peak_rss_kb: i64,
    pub trend: Option<f64>,
    pub baseline_ms: f64,
    pub recent_ms: f64,
    pub regressed_at: Option<String>,
    pub is_new: bool,
    pub is_gone: bool,
}

impl StepJson {
    pub fn new(step: &StepRow) -> Self {
        Self {
            key: step.key.clone(),
            invocations: step.invocations,
            cpu_ms: step.cpu_ms,
            busy_ms: step.busy_ms,
            peak_rss_kb: step.peak_rss_kb,
            trend: None,
            baseline_ms: step.cpu_ms as f64,
            recent_ms: step.cpu_ms as f64,
            regressed_at: None,
            is_new: false,
            is_gone: false,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct TrendJson {
    pub key: String,
    pub baseline_ms: f64,
    pub recent_ms: f64,
    pub change_pct: Option<f64>,
    pub samples: usize,
    pub present_in: usize,
    pub is_new: bool,
    pub is_gone: bool,
    pub regressed_at_run: Option<i64>,
    pub regressed_at_sha: Option<String>,
    pub regressed_at_ms: Option<i64>,
    pub history: Vec<i64>,
}

impl TrendJson {
    pub fn new(trend: &Trend, history: Vec<i64>) -> Self {
        Self {
            key: trend.key.clone(),
            baseline_ms: trend.baseline_ms,
            recent_ms: trend.recent_ms,
            change_pct: trend.delta_pct,
            samples: trend.samples,
            present_in: trend.present_in,
            is_new: trend.is_new,
            is_gone: trend.is_gone,
            regressed_at_run: trend.onset_run_id,
            regressed_at_sha: trend.onset_sha.clone(),
            regressed_at_ms: trend.onset_at,
            history,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn trend(key: &str, baseline: f64, recent: f64, delta: Option<f64>) -> Trend {
        Trend {
            key: key.to_string(),
            onset_index: None,
            baseline_ms: baseline,
            recent_ms: recent,
            delta_pct: delta,
            samples: 10,
            present_in: 10,
            invocations_recent: 3.0,
            is_new: false,
            is_gone: false,
            is_wrapper: false,
            onset_run_id: Some(7),
            onset_sha: Some("a1b2c3d4e5f6a7b8".to_string()),
            onset_at: Some(crate::fmt::now_ms() - 3 * 86_400_000),
            peak_rss_kb: 100,
        }
    }

    #[test]
    fn sparkline_scales_to_the_maximum_in_the_series() {
        // Eight levels, scaled to the series maximum: the midpoint of a 0..10
        // series is the fifth block, not the top one.
        assert_eq!(sparkline(&[0.0, 5.0, 10.0]), "▁▅█");
        assert_eq!(sparkline(&[3.0, 3.0, 3.0]), "███");
        assert_eq!(sparkline(&[10.0, 0.0, 10.0, 0.0]), "█▁█▁");
    }

    #[test]
    fn sparkline_handles_all_zero_and_empty_series() {
        assert_eq!(sparkline(&[0.0, 0.0]), "▁▁");
        assert_eq!(sparkline(&[]), "");
    }

    #[test]
    fn table_columns_align_regardless_of_ansi_colour() {
        let mut table = Table::new(&["step", "ms"]);
        table.push(vec![fmt::red("rustc --crate-name x"), "1234".to_string()]);
        table.push(vec!["cc".to_string(), "9".to_string()]);
        let rendered = table.render();
        let lines: Vec<&str> = rendered.lines().collect();
        assert_eq!(lines.len(), 3, "header plus two rows");
        // Trailing padding is trimmed, so equal line widths are not the
        // invariant — aligned column *starts* are. This is the property that
        // breaks the moment a cell is colourised.
        // Column one occupies [0, 20), the gutter is [20, 22), column two is
        // [22, 26) right-aligned. Asserting those slices catches both a drifting
        // column and a cell padded on the wrong side.
        for line in &lines {
            let plain: Vec<char> = fmt::strip_ansi(line).chars().collect();
            assert!(
                plain.len() <= 26,
                "row wider than the two columns: {plain:?}"
            );
            if plain.len() < 22 {
                // A short row: everything so far is column one, and the padding
                // after it must be blanks, not a stray escape remnant.
                assert!(
                    plain[20..].iter().all(|c| c.is_whitespace()),
                    "unexpected gutter content in {plain:?}"
                );
                continue;
            }
            assert!(
                plain[20..22].iter().all(|c| *c == ' '),
                "gutter is not two spaces in {plain:?}"
            );
        }
    }

    #[test]
    fn right_aligned_columns_line_up_on_their_digits() {
        let mut table = Table::new(&["a", "n"]);
        table.right_align(&[1]);
        table.push(vec!["x".to_string(), "1".to_string()]);
        table.push(vec!["longer".to_string(), "1000".to_string()]);
        let lines: Vec<String> = table.render().lines().map(|l| l.to_string()).collect();
        assert!(lines[1].ends_with("    1"), "got {:?}", lines[1]);
        assert!(lines[2].ends_with(" 1000"), "got {:?}", lines[2]);
    }

    #[test]
    fn markdown_escapes_pipes_so_a_pasted_table_survives() {
        let mut table = Table::new(&["step"]);
        table.push(vec!["sh -c 'a | b'".to_string()]);
        let markdown = table.markdown();
        assert!(markdown.contains("a \\| b"), "got {markdown}");
        assert_eq!(markdown.matches('\n').count(), 3, "header, rule, one row");
    }

    #[test]
    fn markdown_escapes_ansi_colour_away() {
        let mut table = Table::new(&["step"]);
        table.push(vec![fmt::red("cc")]);
        assert!(
            !table.markdown().contains('\x1b'),
            "colour leaked into markdown"
        );
    }

    #[test]
    fn report_leads_with_the_regression_and_suggests_the_next_command() {
        let view = ReportView {
            subtitle: "dawdle · 12 runs".to_string(),
            regressions: vec![(
                trend("cargo test --lib", 4200.0, 11800.0, Some(181.0)),
                vec![1.0, 2.0, 9.0],
            )],
            unchanged: vec![(trend("cc", 2100.0, 2000.0, Some(-5.0)), vec![2.0, 2.0])],
            run_trend: Some(RunTrend {
                baseline_ms: 12100.0,
                recent_ms: 19700.0,
                delta_pct: Some(63.0),
                samples: 12,
                onset_run_id: Some(9),
                onset_sha: Some("deadbeefdeadbeef".to_string()),
            }),
            total_runs: 12,
            metric_label: "cpu".to_string(),
            thresholds: Thresholds {
                pct: 25.0,
                min_abs_ms: 250,
                window: 5,
                min_samples: 4,
            },
            empty_hint: None,
        };
        let text = view.render();
        let regressions_at = text.find("REGRESSIONS").expect("regression section");
        let run_total_at = text.find("run total").expect("run total line");
        assert!(
            run_total_at < regressions_at,
            "the whole-build number must come before the per-step table"
        );
        assert!(
            text.contains("dawdle blame \"cargo test --lib\""),
            "got {text}"
        );
        assert!(text.contains("a1b2c3d"), "onset sha should be shown short");
        // The run-total line names the run *whole-build* trend crossed at (#9),
        // which is not the same run as the step trend's onset (#7).
        assert!(
            text.contains("since run #9"),
            "the run that crossed the line should be named: {text}"
        );
        assert!(
            text.contains("over 12 runs"),
            "the size of the comparison should be stated: {text}"
        );
    }

    #[test]
    fn report_says_so_when_there_is_nothing_to_report() {
        let view = ReportView {
            subtitle: "dawdle · 3 runs".to_string(),
            regressions: vec![],
            unchanged: vec![],
            run_trend: None,
            total_runs: 3,
            metric_label: "cpu".to_string(),
            thresholds: Thresholds::default(),
            empty_hint: Some("Not enough runs yet.".to_string()),
        };
        assert!(view.render().contains("Not enough runs yet."));
    }

    #[test]
    fn missing_baseline_is_dimmed_not_coloured_as_a_regression() {
        assert_eq!(fmt::strip_ansi(&colour_delta(None)), "—");
    }

    #[test]
    fn shell_quote_only_quotes_when_it_has_to() {
        assert_eq!(shell_quote("cc"), "cc");
        assert_eq!(shell_quote("cargo test --lib"), "\"cargo test --lib\"");
        assert_eq!(
            shell_quote("rustc --crate-name x"),
            "\"rustc --crate-name x\""
        );
        assert_eq!(shell_quote("has\"quote"), "\"has\\\"quote\"");
        assert_eq!(shell_quote(""), "\"\"");
    }

    #[test]
    fn empty_table_renders_no_rows() {
        let table = Table::new(&["a"]);
        assert!(table.rows.is_empty());
        assert_eq!(table.render().lines().count(), 1, "header only");
    }
}
