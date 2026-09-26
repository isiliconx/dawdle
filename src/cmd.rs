//! The commands themselves: run, report, blame, show, ls, prune.

use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{bail, Context, Result};

use crate::cli::{BlameArgs, LsArgs, PruneArgs, ReportArgs, RunArgs, ShowArgs};
use crate::db::{
    self, analyse_run, analyse_steps, default_db_path, ensure_self_ignored, Metric, RunFacts,
    RunFilter, RunRow, Store, Trend,
};
use crate::fmt;
use crate::git;
use crate::ps;
use crate::report::{self, ReportView, RunJson, Table, TrendJson};
use crate::sampler::Sampler;

/// Set by the signal handler below. The main loop polls it between samples and
/// shuts the child down, so a cancelled CI job still leaves a usable record of
/// how far the build got.
static INTERRUPTED: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(_signum: libc::c_int) {
    // Async-signal-safe: one relaxed store, nothing else.
    INTERRUPTED.store(true, Ordering::SeqCst);
}

#[cfg(not(unix))]
fn install_signal_handlers() {
    // No portable equivalent, and dawdle only claims Linux and macOS. The
    // interrupted flag is simply never set, so a build is not shut down early.
}

#[cfg(unix)]
fn install_signal_handlers() {
    // The handler goes through a pointer because that is what the C signature
    // takes; casting a function item straight to an integer is not the same
    // thing and newer clippy is right to object.
    let handler = on_signal as *const () as libc::sighandler_t;
    // SAFETY: `on_signal` is `extern "C"`, takes one int, and only does a
    // relaxed atomic store, so it is async-signal-safe. The returned previous
    // handler is deliberately discarded.
    unsafe {
        libc::signal(libc::SIGINT, handler);
        libc::signal(libc::SIGTERM, handler);
    }
}

/// Ring buffer of the last N output lines, kept so a failing run records enough
/// context to be debugged from the profile alone.
struct Tail {
    lines: Mutex<Vec<String>>,
    limit: usize,
}

impl Tail {
    fn new(limit: usize) -> Self {
        Self {
            lines: Mutex::new(Vec::new()),
            limit,
        }
    }

    fn push(&self, line: &str) {
        if let Ok(mut lines) = self.lines.lock() {
            lines.push(line.to_string());
            let len = lines.len();
            if len > self.limit {
                lines.drain(0..len - self.limit);
            }
        }
    }

    fn snapshot(&self) -> Vec<String> {
        self.lines.lock().map(|l| l.clone()).unwrap_or_default()
    }
}

fn detect_runner() -> String {
    for key in [
        "CI",
        "GITHUB_ACTIONS",
        "GITLAB_CI",
        "BUILDKITE",
        "JENKINS_URL",
        "TF_BUILD",
    ] {
        let value = std::env::var(key).unwrap_or_default();
        if value == "true" || (key == "CI" && !value.is_empty() && value != "false") {
            return "ci".to_string();
        }
    }
    "local".to_string()
}

fn hostname() -> String {
    std::env::var("HOSTNAME").unwrap_or_default()
}

/// Copy a child's output through to our own streams while keeping a tail.
///
/// We do not inherit the pipes directly: a build's output has to be visible
/// live (so CI logs and progress bars keep working) *and* recoverable from the
/// profile, and doing that with a straight inherit gives you only one of the
/// two.
fn pump<R: Read + Send + 'static>(
    reader: R,
    sink: Box<dyn Write + Send>,
    tail: Arc<Tail>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let mut reader = BufReader::new(reader);
        let mut line = Vec::new();
        let mut sink = sink;
        loop {
            line.clear();
            match reader.read_until(b'\n', &mut line) {
                Ok(0) => break,
                Ok(_) => {
                    let text = String::from_utf8_lossy(&line);
                    let _ = sink.write_all(&line);
                    let _ = sink.flush();
                    tail.push(text.trim_end_matches(['\r', '\n']));
                }
                Err(_) => break,
            }
        }
    })
}

pub fn run(args: RunArgs) -> Result<i32> {
    let db_path = default_db_path(args.db.as_deref());
    if let Some(parent) = db_path.parent() {
        ensure_self_ignored(parent)?;
    }
    let mut store = Store::open(&db_path)?;

    let Some((program, program_args)) = args.command.split_first() else {
        bail!("nothing to run");
    };

    // Read git state before the build: the question is always "what code was
    // this, and what did it do", and a build that commits mid-run must not
    // rewrite its own history.
    let git_facts = git::facts();
    let started_at = db::now();

    let mut child = Command::new(program)
        .args(program_args)
        .stdin(Stdio::inherit())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("could not start {program:?}. Is it on your PATH?"))?;
    let root_pid = child.id() as i32;

    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let tail = Arc::new(Tail::new(200));
    let mut pumps = Vec::new();
    if let Some(stdout) = stdout {
        pumps.push(pump(stdout, Box::new(std::io::stdout()), Arc::clone(&tail)));
    }
    if let Some(stderr) = stderr {
        pumps.push(pump(stderr, Box::new(std::io::stderr()), Arc::clone(&tail)));
    }

    install_signal_handlers();

    let mut sampler = match ps::default_source() {
        Some(source) => Some(Sampler::new(
            source,
            root_pid,
            args.interval_ms,
            Some(&args.command.join(" ")),
        )),
        None => {
            eprintln!(
                "dawdle: process sampling is not supported on {}, recording wall time only",
                ps::platform_name()
            );
            None
        }
    };

    let mut interrupted = false;
    let status = loop {
        if let Some(sampler) = sampler.as_mut() {
            sampler.poll();
        }
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if INTERRUPTED.load(Ordering::SeqCst) && !interrupted {
            interrupted = true;
            eprintln!("\ndawdle: interrupted, stopping {program}");
            let _ = child.kill();
        }
        std::thread::sleep(Duration::from_millis(args.interval_ms));
    };
    for handle in pumps {
        let _ = handle.join();
    }

    // One final sample after the child is reaped, so a process that finished
    // between the last tick and the exit is not lost.
    let profile = match sampler {
        Some(sampler) => sampler.finish(),
        None => crate::sampler::Profile {
            interval_ms: args.interval_ms,
            source: format!("unsupported ({})", ps::platform_name()),
            ..Default::default()
        },
    };

    let facts = RunFacts {
        started_at,
        duration_ms: profile.duration_ms,
        label: args.label.clone(),
        command: args.command.join(" "),
        cwd: std::env::current_dir()
            .map(|p| p.display().to_string())
            .unwrap_or_default(),
        git_sha: git_facts.sha.clone(),
        git_branch: git_facts.branch.clone(),
        git_dirty: git_facts.dirty,
        host: hostname(),
        runner: detect_runner(),
        exit_code: status.code().unwrap_or(-1),
        succeeded: status.success() && !interrupted,
        // Interleaved stdout and stderr in arrival order, which is not the true
        // interleaving but is close enough to read and is what a CI log shows
        // anyway.
        output_tail: tail.snapshot().join("\n"),
    };
    let run_id = store.insert_run(facts, &profile)?;

    if args.json {
        let payload = RunJson {
            id: run_id,
            started_at,
            duration_ms: profile.duration_ms,
            // Cloned: `args` is read again by the comparison below.
            label: args.label.clone(),
            command: args.command.join(" "),
            git_sha: git_facts.sha,
            git_branch: git_facts.branch,
            exit_code: status.code().unwrap_or(-1),
            succeeded: status.success(),
            cpu_ms: profile.total_cpu_ms,
            peak_rss_kb: profile.peak_tree_rss_kb,
            processes: profile.processes.len() as i64,
            coverage_pct: profile.coverage(),
            parallelism: profile.parallelism(),
        };
        println!("{}", serde_json::to_string_pretty(&payload)?);
    } else if !args.quiet {
        print_run_summary(run_id, &profile, &git_facts, status.success());
        // The whole point of the tool: the verdict on this build, not just the
        // fact that it happened. Cheap because it is a local SQLite read.
        if let Some(line) = comparison_line(&store, &args) {
            println!("{line}");
        }
    }

    // Propagate the child's status: dawdle is a wrapper, and a wrapper that
    // swallows a failing build's exit code is worse than no wrapper at all.
    Ok(if interrupted {
        130
    } else {
        status.code().unwrap_or(1)
    })
}

/// One line comparing the run we just recorded against recent history, or
/// `None` while there is not enough history to say anything.
///
/// Printed after every `dawdle run` so the tool is useful with no second
/// command and no reading of a table: the common case is a CI job that wants a
/// verdict, not a report.
fn comparison_line(store: &Store, args: &RunArgs) -> Option<String> {
    let filter = RunFilter {
        // Compare against runs carrying the same label, so a build recorded as
        // `ci` is judged against other ci builds rather than against whatever
        // else happened to run in this repo.
        label: if args.label.is_empty() {
            None
        } else {
            Some(args.label.clone())
        },
        since_ms: None,
        branch: None,
        succeeded_only: false,
    };
    let runs = store.recent_runs(&filter, 20).ok()?;
    if runs.len() < 3 {
        return None;
    }
    let ids: Vec<i64> = runs.iter().map(|row| row.id).collect();
    let metric = Metric::Busy;
    let history = store.step_history(&ids, metric).ok()?;
    let trends = analyse_steps(&history, ids.len(), db::Thresholds::default(), metric);
    let series = store.run_duration_series(&ids).ok()?;
    let run_trend = analyse_run(&series, db::Thresholds::default());
    let view = build_view(
        &runs,
        &history,
        trends,
        run_trend,
        metric,
        db::Thresholds::default(),
        3,
        false,
    );
    let line = view.summary_line(runs.first());
    if line.is_empty() {
        None
    } else {
        Some(line)
    }
}

fn print_run_summary(
    run_id: i64,
    profile: &crate::sampler::Profile,
    git_facts: &git::GitFacts,
    succeeded: bool,
) {
    let mark = if succeeded {
        fmt::green("✓")
    } else {
        fmt::red("✗")
    };
    let steps = profile.top_keys().len();
    let mut line = format!(
        "\n{} {} run #{} · {} wall · {} cpu",
        mark,
        fmt::bold("recorded"),
        run_id,
        fmt::ms(profile.duration_ms),
        fmt::ms(profile.total_cpu_ms)
    );
    if let Some(ratio) = profile.parallelism() {
        line.push_str(&format!(" · {ratio:.1}x parallel"));
    }
    println!("{line}");

    let mut detail = format!(
        "  {} steps · {} processes · {} polls @ {}ms",
        steps,
        profile.processes.len(),
        profile.polls,
        profile.interval_ms
    );
    if let Some(coverage) = profile.coverage() {
        detail.push_str(&format!(" · {coverage:.0}% sampling coverage"));
    }
    if !git_facts.in_repo {
        detail.push_str(" · not a git repo, so blame will not be available");
    }
    if !git_facts.sha.is_empty() {
        let dirty = if git_facts.dirty { " (dirty)" } else { "" };
        let branch = if git_facts.branch.is_empty() {
            String::new()
        } else {
            format!("{}@", git_facts.branch)
        };
        detail.push_str(&format!(
            " · {branch}{}{dirty}",
            git_facts.sha.chars().take(7).collect::<String>()
        ));
    }
    println!("{}", fmt::dim(&detail));
    println!("  {} {}", fmt::dim("next:"), fmt::blue("dawdle report"));
}

fn subtitle(runs: &[RunRow], metric: Metric) -> String {
    let mut parts = vec![format!(
        "{} run{}",
        runs.len(),
        if runs.len() == 1 { "" } else { "s" }
    )];
    if let (Some(first), Some(last)) = (runs.last(), runs.first()) {
        parts.push(format!(
            "{} → {}",
            fmt::iso_date(first.started_at),
            fmt::iso_date(last.started_at)
        ));
    }
    if let Some(latest) = runs.first() {
        parts.push(fmt::ago(latest.started_at));
    }
    parts.push(format!("metric {metric}"));
    parts.join(" · ")
}

fn series_for(
    history: &std::collections::BTreeMap<String, Vec<db::StepPoint>>,
    key: &str,
) -> Vec<f64> {
    history
        .get(key)
        .map(|points| points.iter().map(|p| p.value as f64).collect())
        .unwrap_or_default()
}

fn is_regression(trend: &Trend, thresholds: &db::Thresholds) -> bool {
    match trend.delta_pct {
        Some(pct) => pct >= thresholds.pct,
        None => false,
    }
}

pub fn report(args: ReportArgs) -> Result<()> {
    let db_path = default_db_path(args.db.as_deref());
    let store = Store::open(&db_path)?;
    let filter = RunFilter {
        label: args.label.clone(),
        since_ms: args.since_ms,
        branch: args.branch.clone(),
        succeeded_only: false,
    };
    let runs = store.recent_runs(&filter, args.last)?;
    if runs.is_empty() {
        println!(
            "{}",
            no_runs_hint(&db_path, args.since_ms, args.label.as_deref())
        );
        return Ok(());
    }
    let ids: Vec<i64> = runs.iter().map(|row| row.id).collect();
    let history = store.step_history(&ids, args.metric)?;
    let trends = analyse_steps(&history, ids.len(), args.thresholds, args.metric);
    let run_trend = analyse_run(&store.run_duration_series(&ids)?, args.thresholds);
    let current = runs.first();
    let view = build_view(
        &runs,
        &history,
        trends,
        run_trend,
        args.metric,
        args.thresholds,
        args.top,
        args.show_all,
    );

    if args.json {
        let payload: Vec<TrendJson> = trends_json(&history, &ids, args.thresholds, args.metric)?;
        println!("{}", serde_json::to_string_pretty(&payload)?);
    } else if args.markdown {
        print!("{}", view.markdown(current));
    } else {
        print!("{}", view.render());
    }
    Ok(())
}

/// Split analysed trends into the two tables and apply the row limits.
///
/// Shared by `report` and `run` so the one-line summary printed after a build
/// and the full report can never disagree about what regressed.
#[allow(clippy::too_many_arguments)]
fn build_view(
    runs: &[RunRow],
    history: &std::collections::BTreeMap<String, Vec<db::StepPoint>>,
    trends: Vec<Trend>,
    run_trend: Option<db::RunTrend>,
    metric: Metric,
    thresholds: db::Thresholds,
    top: usize,
    show_all: bool,
) -> ReportView {
    // A step counts as a regression when it cleared the relative threshold. The
    // absolute margin is enforced at onset detection, because that is where a
    // single noisy sample can be compared against its own history; here we are
    // comparing two whole populations, where the median already absorbs one-off
    // spikes.
    let (mut regressions, mut unchanged): (Vec<_>, Vec<_>) = trends
        .into_iter()
        .map(|trend| {
            let series = series_for(history, &trend.key);
            (trend, series)
        })
        .partition(|(trend, _)| is_regression(trend, &thresholds));

    if show_all {
        regressions.truncate(top.max(1));
    } else {
        regressions.truncate(top);
        unchanged.truncate(top);
    }

    ReportView {
        subtitle: subtitle(runs, metric),
        regressions,
        unchanged,
        run_trend,
        total_runs: runs.len(),
        metric_label: metric.to_string(),
        thresholds,
        empty_hint: Some(
            "Not enough runs to compare yet. dawdle needs a few runs before it will call something a regression."
                .to_string(),
        ),
    }
}

/// JSON for `report --json`, built from the same analysis the terminal view
/// uses so the two outputs can never disagree about what regressed.
fn trends_json(
    history: &std::collections::BTreeMap<String, Vec<db::StepPoint>>,
    ids: &[i64],
    thresholds: db::Thresholds,
    metric: Metric,
) -> Result<Vec<TrendJson>> {
    let trends = analyse_steps(history, ids.len(), thresholds, metric);
    let mut out: Vec<TrendJson> = trends
        .iter()
        .map(|trend| {
            let values = series_for(history, &trend.key)
                .into_iter()
                .map(|v| v as i64)
                .collect();
            TrendJson::new(trend, values)
        })
        .collect();
    out.retain(|entry| entry.samples > 0);
    Ok(out)
}

fn no_runs_hint(db_path: &Path, since_ms: Option<i64>, label: Option<&str>) -> String {
    let window = match since_ms {
        Some(ms) if ms % 86_400_000 == 0 => format!("{}d", ms / 86_400_000),
        Some(ms) if ms % 3_600_000 == 0 => format!("{}h", ms / 3_600_000),
        Some(ms) => format!("{}m", ms / 60_000),
        None => "the selected window".to_string(),
    };
    let mut hint = format!("No runs recorded in {window}.");
    if let Some(label) = label {
        hint.push_str(&format!(" (label {label:?})"));
    }
    hint.push_str("\n\nRecord one with:\n  dawdle run --label ");
    if let Some(label) = label {
        hint.push_str(&format!("{label} -- "));
    } else {
        hint.push('"');
    }
    hint.push_str("make test\n\nProfiles live in ");
    hint.push_str(&db_path.display().to_string());
    hint
}

pub fn blame(args: BlameArgs) -> Result<()> {
    let db_path = default_db_path(args.db.as_deref());
    let store = Store::open(&db_path)?;
    let filter = RunFilter {
        label: args.label.clone(),
        since_ms: args.since_ms,
        branch: None,
        succeeded_only: false,
    };
    let runs = store.recent_runs(&filter, args.last)?;
    if runs.is_empty() {
        println!(
            "{}",
            no_runs_hint(&db_path, args.since_ms, args.label.as_deref())
        );
        return Ok(());
    }
    let ids: Vec<i64> = runs.iter().map(|row| row.id).collect();

    if args.target.eq_ignore_ascii_case("total") {
        return blame_total(&store, &ids, &args);
    }

    let history = store.step_history(&ids, args.metric)?;
    // Match on a whole word anywhere in the key, not just a prefix. Nobody
    // remembers to type the leading program name: the report says
    // `dawdle blame "sh burn.sh"`, the user types `burn.sh`, and a
    // prefix-only matcher tells them no such step exists while listing it
    // right underneath. Exact and prefix matches are still preferred, so an
    // exact hit is never shadowed by a looser one.
    let matches = match_step(&history.keys().collect::<Vec<_>>(), &args.target);
    let key = match matches.len() {
        0 => {
            println!(
                "No step matching {:?} in the last {} runs.\n\nKnown steps:",
                args.target,
                runs.len()
            );
            for key in history.keys().take(20) {
                println!("  {key}");
            }
            return Ok(());
        }
        1 => matches[0].clone(),
        _ => {
            println!("{:?} matches {} steps:", args.target, matches.len());
            for key in matches.iter().take(20) {
                println!("  {key}");
            }
            println!("\nBe more specific.");
            return Ok(());
        }
    };
    blame_step(&store, &history, &key, &args)
}

/// Resolve a user-supplied step target against the known step keys.
///
/// Three passes, loosest last, so a precise answer is never displaced by a
/// sloppier one: an exact key, then a case-insensitive prefix, then a whole-word
/// substring. The substring pass is what lets `dawdle blame burn.sh` find
/// `sh burn.sh` — the report prints the full key, but nobody types the leading
/// program name.
fn match_step(keys: &[&String], target: &str) -> Vec<String> {
    let exact: Vec<String> = keys
        .iter()
        .filter(|key| key.as_str() == target)
        .map(|key| (*key).clone())
        .collect();
    if !exact.is_empty() {
        return exact;
    }
    let needle = target.to_ascii_lowercase();
    let prefix: Vec<String> = keys
        .iter()
        .filter(|key| key.to_ascii_lowercase().starts_with(&needle))
        .map(|key| (*key).clone())
        .collect();
    if !prefix.is_empty() {
        return prefix;
    }
    keys.iter()
        .filter(|key| contains_word(key, &needle))
        .map(|key| (*key).clone())
        .collect()
}

/// Whether `needle` appears in `haystack` delimited by non-word characters, so
/// `burn` and `burn.sh` both match `sh burn.sh` while `burn` does not match
/// `burnt_toast`. ASCII case is ignored.
///
/// `.` counts as a delimiter rather than part of a word, because step keys are
/// full of dots in filenames and flags, and a user asking about `burn` means
/// `burn.sh`.
fn contains_word(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return false;
    }
    // Fold case here rather than at the call site, so the function is correct
    // on its own and no caller has to remember to.
    let haystack = haystack.to_ascii_lowercase();
    let needle = needle.to_ascii_lowercase();
    let (haystack, needle) = (haystack.as_str(), needle.as_str());
    let is_word = |c: char| c.is_alphanumeric() || c == '_';
    let mut from = 0;
    while from <= haystack.len() {
        let Some(offset) = haystack[from..].find(needle) else {
            return false;
        };
        let start = from + offset;
        let end = start + needle.len();
        let before_ok = haystack[..start]
            .chars()
            .next_back()
            .map(|c| !is_word(c))
            .unwrap_or(true);
        let after_ok = haystack[end..]
            .chars()
            .next()
            .map(|c| !is_word(c))
            .unwrap_or(true);
        if before_ok && after_ok {
            return true;
        }
        // Advance a whole character so a multi-byte needle cannot split one.
        from = start
            + haystack[start..]
                .chars()
                .next()
                .map(char::len_utf8)
                .unwrap_or(1);
    }
    false
}

struct Blame {
    /// The step key, kept for diagnostics; callers already know it.
    #[allow(dead_code)]
    key: String,
    values: Vec<i64>,
    onset_index: usize,
    onset_sha: String,
    pre_onset_sha: String,
    onset_run: i64,
    onset_at: i64,
}

fn locate_onset(key: &str, points: &[db::StepPoint], thresholds: db::Thresholds) -> Option<Blame> {
    let (index, current, previous) = db::detect_onset(points, thresholds)?;
    let current = current?;
    let previous = previous?;
    Some(Blame {
        key: key.to_string(),
        values: points.iter().map(|p| p.value).collect(),
        onset_index: index,
        onset_sha: current.git_sha.clone(),
        pre_onset_sha: previous.git_sha.clone(),
        onset_run: current.run_id,
        onset_at: current.started_at,
    })
}

fn blame_step(
    store: &Store,
    history: &std::collections::BTreeMap<String, Vec<db::StepPoint>>,
    key: &str,
    args: &BlameArgs,
) -> Result<()> {
    let points = &history[key];
    let before = db::median(
        &points[..points.len() / 2]
            .iter()
            .map(|p| p.value as f64)
            .collect::<Vec<_>>(),
    );
    let after = db::median(
        &points[points.len() / 2..]
            .iter()
            .map(|p| p.value as f64)
            .collect::<Vec<_>>(),
    );

    let Some(blame) = locate_onset(key, points, args.thresholds) else {
        return report_no_onset(key, points, before, after, args);
    };

    let commits = git::commit_range(&blame.pre_onset_sha, &blame.onset_sha);
    if args.json {
        let payload = serde_json::json!({
            "step": key,
            "metric": args.metric.to_string(),
            "before_ms": before,
            "after_ms": after,
            "change_pct": fmt::pct_change(before, after),
            "regressed_at_run": blame.onset_run,
            "regressed_at_sha": blame.onset_sha,
            "regressed_at_ms": blame.onset_at,
            "previous_sha": blame.pre_onset_sha,
            "history": blame.values,
            "commits": commits.iter().map(|c| serde_json::json!({
                "sha": c.sha,
                "short": c.short,
                "author": c.author,
                "date_ms": c.date_ms,
                "subject": c.subject,
            })).collect::<Vec<_>>(),
        });
        println!("{}", serde_json::to_string_pretty(&payload)?);
        return Ok(());
    }

    println!(
        "{}",
        fmt::bold(&format!(
            "{} — {} → {}",
            key,
            fmt::ms(before as i64),
            fmt::ms(after as i64)
        ))
    );
    println!(
        "  metric {} · {} runs · {}",
        args.metric,
        points.len(),
        report::sparkline_marked(
            &blame.values.iter().map(|v| *v as f64).collect::<Vec<_>>(),
            Some(blame.onset_index)
        )
    );
    println!(
        "\n  first crossed the threshold at run #{} on {}, {}",
        blame.onset_run,
        fmt::iso_date(blame.onset_at),
        fmt::ago(blame.onset_at)
    );

    if blame.pre_onset_sha.is_empty() || blame.onset_sha.is_empty() {
        println!("\n  No git sha recorded for these runs, so the change cannot be attributed to a commit.");
        return Ok(());
    }
    if blame.pre_onset_sha == blame.onset_sha {
        // Both ends of the range are the same commit, which means the step
        // crossed the threshold partway through a series of runs that were all
        // built from one commit. There is nothing to bisect: whatever caused it
        // is already in the tree. Saying so is the whole value of this command,
        // and printing "abc123..abc123" with a bisect hint would send the reader
        // looking for a commit that cannot exist.
        println!(
            "\n  {} The step crossed the threshold {} runs into a series built from a single\n  commit ({}), so there is no commit range to blame. The cause is already in\n  your tree — compare against an older commit, or bisect from {}.",
            fmt::yellow("!"),
            points.len() - blame.onset_index.min(points.len().saturating_sub(1)),
            short(&blame.onset_sha),
            short(&blame.onset_sha)
        );
        return Ok(());
    }
    println!(
        "\n  {} {}..{}",
        fmt::dim("commit range"),
        short(&blame.pre_onset_sha),
        short(&blame.onset_sha)
    );
    if commits.is_empty() {
        println!(
            "\n  {} git could not resolve that range. That happens with shallow clones,\n  rebases, and force pushes. Try `git fetch --unshallow`, or compare {} to {} directly.",
            fmt::yellow("!"),
            short(&blame.pre_onset_sha),
            short(&blame.onset_sha)
        );
        return Ok(());
    }
    println!();
    let mut table = Table::new(&["commit", "when", "author", "subject"]);
    for commit in commits.iter().take(args.max_commits) {
        table.push(vec![
            fmt::yellow(&commit.short),
            fmt::iso_date(commit.date_ms),
            commit.author.clone(),
            commit.subject.clone(),
        ]);
    }
    print!("{}", table.render());
    if commits.len() > args.max_commits {
        println!("  … and {} more", commits.len() - args.max_commits);
    }
    println!(
        "\n  {} {}",
        fmt::dim("bisect:"),
        fmt::blue(&format!(
            "git bisect start {} {}",
            short(&blame.onset_sha),
            short(&blame.pre_onset_sha)
        ))
    );
    let _ = store;
    Ok(())
}

fn report_no_onset(
    key: &str,
    points: &[db::StepPoint],
    before: f64,
    after: f64,
    args: &BlameArgs,
) -> Result<()> {
    let spark = report::sparkline(&points.iter().map(|p| p.value as f64).collect::<Vec<_>>());
    println!("{}", fmt::bold(key));
    println!("  metric {} · {} runs · {spark}", args.metric, points.len());
    println!(
        "  {} {} → {} ({})",
        fmt::dim("median:"),
        fmt::ms(before as i64),
        fmt::ms(after as i64),
        fmt::signed_pct(fmt::pct_change(before, after).unwrap_or(0.0))
    );
    if points.len() < args.thresholds.min_samples + 1 {
        println!(
            "\n  Only {} runs recorded. dawdle needs {} before it will call a change a regression;\n  until then a jump here is indistinguishable from a cold cache or a noisy box.",
            points.len(),
            args.thresholds.min_samples + 1
        );
        return Ok(());
    }
    println!(
        "\n  No single run crossed {}% and {}ms. The step is either stable or drifting\n  gradually. {} to see every step's trend.",
        args.thresholds.pct as i64,
        args.thresholds.min_abs_ms,
        fmt::blue("dawdle report")
    );
    Ok(())
}

fn blame_total(store: &Store, ids: &[i64], args: &BlameArgs) -> Result<()> {
    let series = store.run_duration_series(ids)?;
    let Some(trend) = analyse_run(&series, args.thresholds) else {
        println!(
            "Need at least two runs to compare. Recorded: {}",
            series.len()
        );
        return Ok(());
    };
    let onset = db::detect_onset(&series, args.thresholds);
    let spark = report::sparkline(&series.iter().map(|p| p.value as f64).collect::<Vec<_>>());

    let payload = serde_json::json!({
        "target": "total",
        "before_ms": trend.baseline_ms,
        "after_ms": trend.recent_ms,
        "change_pct": trend.delta_pct,
        "runs": series.len(),
        "history": series.iter().map(|p| p.value).collect::<Vec<_>>(),
    });
    if args.json {
        println!("{}", serde_json::to_string_pretty(&payload)?);
        return Ok(());
    }

    println!(
        "{}",
        fmt::bold(&format!(
            "total — {} → {}",
            fmt::ms(trend.baseline_ms as i64),
            fmt::ms(trend.recent_ms as i64)
        ))
    );
    println!("  wall clock · {} runs · {spark}", series.len());
    if let Some(pct) = trend.delta_pct {
        println!("  {}", report::colour_delta(Some(pct)));
    }

    let Some((_, current, previous)) = onset else {
        println!(
            "\n  No single run crossed {}% and {}ms. The build is either stable or drifting\n  gradually rather than jumping. `dawdle report` breaks the total down by step.",
            args.thresholds.pct as i64,
            args.thresholds.min_abs_ms
        );
        return Ok(());
    };
    let (current, previous) = (current.unwrap(), previous.unwrap());
    let commits = git::commit_range(&previous.git_sha, &current.git_sha);
    println!(
        "\n  first crossed the threshold at run #{} on {}",
        current.run_id,
        fmt::iso_date(current.started_at)
    );
    if commits.is_empty() {
        println!("\n  No commits to attribute. If this is CI, check the history actually contains both shas.");
        return Ok(());
    }
    println!(
        "\n  {} {}..{}",
        fmt::dim("commit range"),
        short(&previous.git_sha),
        short(&current.git_sha)
    );
    let mut table = Table::new(&["commit", "when", "author", "subject"]);
    for commit in commits.iter().take(args.max_commits) {
        table.push(vec![
            fmt::yellow(&commit.short),
            fmt::iso_date(commit.date_ms),
            commit.author.clone(),
            commit.subject.clone(),
        ]);
    }
    print!("{}", table.render());
    println!(
        "\n  {} `dawdle report` will tell you which step moved.",
        fmt::dim("next:")
    );
    Ok(())
}

fn short(sha: &str) -> String {
    sha.chars().take(7).collect()
}

pub fn show(args: ShowArgs) -> Result<()> {
    let db_path = default_db_path(args.db.as_deref());
    let store = Store::open(&db_path)?;
    let Some(run) = store.run_by_id(args.run_id)? else {
        bail!("no run #{} in {}", args.run_id, db_path.display());
    };
    let steps = store.steps_for_run(args.run_id)?;
    let output_tail = store.run_output_tail(args.run_id)?;

    if args.json {
        let steps_json: Vec<serde_json::Value> = steps
            .iter()
            .map(|step| serde_json::to_value(report::StepJson::new(step)).unwrap_or_default())
            .collect();
        let payload = serde_json::json!({
            "run": RunJson::from(&run),
            "steps": steps_json,
        });
        println!("{}", serde_json::to_string_pretty(&payload)?);
        return Ok(());
    }

    println!("{}", fmt::bold(&format!("run #{}", run.id)));
    println!(
        "  {} · {} · {}",
        fmt::iso_date(run.started_at),
        fmt::ago(run.started_at),
        run.command
    );
    let facts = vec![
        ("wall", fmt::ms(run.duration_ms)),
        ("cpu", fmt::ms(run.cpu_ms)),
        (
            "parallel",
            run.parallelism
                .map(|r| format!("{r:.1}x"))
                .unwrap_or_else(|| "—".to_string()),
        ),
        ("peak rss", fmt::bytes(run.peak_rss_kb)),
        ("processes", run.proc_count.to_string()),
        (
            "coverage",
            run.coverage_pct
                .map(|c| format!("{c:.0}%"))
                .unwrap_or_else(|| "—".to_string()),
        ),
        (
            "git",
            if run.git_sha.is_empty() {
                "—".to_string()
            } else {
                format!(
                    "{}{}",
                    short(&run.git_sha),
                    if run.git_branch.is_empty() {
                        String::new()
                    } else {
                        format!(" ({})", run.git_branch)
                    }
                )
            },
        ),
        (
            "result",
            if run.succeeded {
                "ok".to_string()
            } else {
                format!("exit {}", run.exit_code)
            },
        ),
    ];
    let mut info = Table::new(&["", ""]);
    for (key, value) in facts {
        info.push(vec![key.to_string(), value]);
    }
    print!("{}", info.render());

    if steps.is_empty() {
        println!("  no steps observed");
    } else {
        println!();
        let mut table = Table::new(&["step", "n", "cpu", "busy", "peak rss"]);
        table.right_align(&[1, 2, 3, 4]);
        for step in steps.iter().take(if args.processes { 200 } else { 40 }) {
            // A wrapper (make, npm, cargo) is not itself the work; dimming it
            // keeps it from reading like a step that costs real time.
            let name = if step.is_wrapper {
                fmt::dim(&step.key)
            } else {
                step.key.clone()
            };
            table.push(vec![
                name,
                step.invocations.to_string(),
                fmt::ms(step.cpu_ms),
                fmt::ms(step.busy_ms),
                fmt::bytes(step.peak_rss_kb),
            ]);
        }
        print!("{}", table.render());
        if steps.len() > 40 && !args.processes {
            println!(
                "  … {} more steps, see them with --processes",
                steps.len() - 40
            );
        }
        // The real command behind a normalised key, for when the key is
        // abbreviated enough to be ambiguous.
        if let Some(example) = steps.iter().find(|s| !s.sample_raw.is_empty()) {
            println!();
            println!(
                "{} {}",
                fmt::dim("e.g."),
                fmt::dim(&fmt::truncate_middle(&example.sample_raw, 100))
            );
        }
    }

    if !output_tail.is_empty() {
        println!();
        println!("{}", fmt::bold("output (last lines)"));
        for line in output_tail.lines() {
            println!("  {}", fmt::dim(line));
        }
    }
    Ok(())
}

pub fn ls(args: LsArgs) -> Result<()> {
    let db_path = default_db_path(args.db.as_deref());
    let store = Store::open(&db_path)?;
    let runs = store.recent_runs(&RunFilter::default(), args.last)?;
    if runs.is_empty() {
        println!("{}", no_runs_hint(&db_path, None, None));
        return Ok(());
    }
    if args.json {
        let payload: Vec<RunJson> = runs.iter().map(RunJson::from).collect();
        println!("{}", serde_json::to_string_pretty(&payload)?);
        return Ok(());
    }
    let mut table = Table::new(&["id", "when", "wall", "cpu", "par", "steps", "git", "label"]);
    table.right_align(&[2, 3, 4, 5]);
    for run in &runs {
        let mark = if run.succeeded { " " } else { "!" };
        table.push(vec![
            format!("{mark}{}", run.id),
            fmt::ago(run.started_at),
            fmt::ms(run.duration_ms),
            fmt::ms(run.cpu_ms),
            run.parallelism
                .map(|r| format!("{r:.1}x"))
                .unwrap_or_else(|| "—".to_string()),
            run.proc_count.to_string(),
            if run.git_sha.is_empty() {
                "—".to_string()
            } else {
                short(&run.git_sha)
            },
            run.label.clone(),
        ]);
    }
    print!("{}", table.render());
    println!(
        "\n{}",
        fmt::dim(&format!("{} in {}", runs.len(), db_path.display()))
    );
    Ok(())
}

pub fn prune(args: PruneArgs) -> Result<()> {
    let db_path = default_db_path(args.db.as_deref());
    let mut store = Store::open(&db_path)?;
    let total = store.count_runs()?;
    let removed = store.prune(args.keep)?;
    if args.vacuum {
        store.vacuum()?;
    }
    let remaining = store.count_runs()?;
    println!(
        "kept {remaining} of {total} runs, removed {removed} ({})",
        db_path.display()
    );
    if remaining as f64 > (args.keep as f64 * 1.1).max(50.0) {
        println!(
            "{}",
            fmt::dim("  tip: prune from a cron or a CI step, or set --keep higher if you want deep history")
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }
    fn refs(list: &[String]) -> Vec<&String> {
        list.iter().collect()
    }

    #[test]
    fn a_step_target_matches_on_a_whole_word_anywhere_in_the_key() {
        // The report prints `dawdle blame "sh burn.sh"`. The user types
        // `burn.sh`. Refusing that while listing the key right underneath is the
        // kind of small hostility that makes a tool feel unfinished.
        assert!(contains_word("sh burn.sh", "burn.sh"));
        assert!(contains_word("sh burn.sh", "burn"));
        assert!(contains_word("cargo test --lib", "test"));
        assert!(contains_word("cargo test --lib", "cargo test"));
        // Case-insensitive: nobody remembers which case a key used.
        assert!(contains_word("sh Burn.sh", "burn.sh"));
    }

    #[test]
    fn a_step_target_does_not_match_inside_a_longer_word() {
        assert!(!contains_word("sh burnt_toast.sh", "burn"));
        assert!(!contains_word("cargo build", "test"));
        assert!(!contains_word("precompile", "compile"));
    }

    #[test]
    fn an_empty_target_matches_nothing() {
        // Otherwise `dawdle blame ""` would resolve to an arbitrary step.
        assert!(!contains_word("sh burn.sh", ""));
    }

    #[test]
    fn step_target_matching_handles_multibyte_text() {
        // A byte-wise scan would slice a UTF-8 boundary and panic here.
        assert!(contains_word("sh caf\u{e9}.sh", "caf\u{e9}"));
        assert!(!contains_word("sh caf\u{e9}.sh", "caf"));
    }

    #[test]
    fn an_exact_key_wins_over_a_looser_match() {
        let list = keys(&["python3", "sh python3 helper"]);
        let got = match_step(&refs(&list), "python3");
        assert_eq!(
            got,
            vec![list[0].clone()],
            "exact match must not be shadowed"
        );
    }

    #[test]
    fn a_prefix_wins_over_a_substring() {
        let list = keys(&["cargo test --lib", "sh cargo test"]);
        let got = match_step(&refs(&list), "cargo test");
        assert_eq!(
            got,
            vec![list[0].clone()],
            "the prefix match is more precise"
        );
    }

    #[test]
    fn an_unmatched_target_resolves_to_nothing() {
        let list = keys(&["cargo build", "sh burn.sh"]);
        assert!(match_step(&refs(&list), "pytest").is_empty());
    }
}
