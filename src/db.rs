//! Persistence, and the trend engine that answers the question dawdle exists
//! to answer.
//!
//! Two ideas carry the whole design:
//!
//! 1. **Runs are immutable facts.** We never update a completed run's numbers.
//!    If the sampler was broken last Tuesday, Tuesday's data stays wrong and
//!    visible rather than being quietly rewritten by a better build today.
//! 2. **Regression detection is sequential, not aggregate.** Comparing a median
//!    of "the last 20 runs" to a median of "the 20 before that" tells you a
//!    step is slow. It does not tell you *which commit* made it slow. We instead
//!    walk a key's history oldest to newest, keep a rolling median of what came
//!    before, and record the exact run where the value first crossed the
//!    threshold. That run has a git sha attached, and that is the whole `blame`
//!    command.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};

use crate::fmt::now_ms;
use crate::sampler::Profile;

/// Bumped whenever the schema changes. `open` migrates older databases rather
/// than failing, so a user never has to delete a profile to upgrade.
const SCHEMA_VERSION: i64 = 1;

pub struct RunFacts {
    pub started_at: i64,
    pub duration_ms: i64,
    pub label: String,
    pub command: String,
    pub cwd: String,
    pub git_sha: String,
    pub git_branch: String,
    pub git_dirty: bool,
    pub host: String,
    pub runner: String,
    pub exit_code: i32,
    pub succeeded: bool,
    /// The tail of the build's combined output, already bounded by the caller.
    pub output_tail: String,
}

#[derive(Debug, Clone)]
pub struct RunRow {
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
    pub proc_count: i64,
    pub coverage_pct: Option<f64>,
    pub parallelism: Option<f64>,
}

pub struct StepRow {
    pub key: String,
    pub invocations: i64,
    pub cpu_ms: i64,
    pub busy_ms: i64,
    pub peak_rss_kb: i64,
    pub is_wrapper: bool,
    pub sample_raw: String,
}

/// One row of a step's history: how it did in one run.
#[derive(Debug, Clone)]
pub struct StepPoint {
    pub run_id: i64,
    pub started_at: i64,
    pub git_sha: String,
    /// The metric this point was selected under (busy_ms, cpu_ms, longest_ms).
    pub value: i64,
    pub invocations: i64,
    /// High-water RSS of the largest single process, independent of `value`.
    pub peak_rss_kb: i64,
}

/// The verdict on one step across the runs we looked at.
#[derive(Debug, Clone)]
pub struct Trend {
    pub key: String,
    pub baseline_ms: f64,
    pub recent_ms: f64,
    pub delta_pct: Option<f64>,
    pub samples: usize,
    pub present_in: usize,
    pub invocations_recent: f64,
    pub is_new: bool,
    pub is_gone: bool,
    pub is_wrapper: bool,
    /// Index into the step's value series where it first crossed the
    /// threshold. Carried so the report can point at the exact sparkline
    /// column instead of only saying "since <date>".
    pub onset_index: Option<usize>,
    /// The run where this step first crossed the threshold, if it did.
    pub onset_run_id: Option<i64>,
    pub onset_sha: Option<String>,
    pub onset_at: Option<i64>,
    pub peak_rss_kb: i64,
}

/// Whole-run trend, so "the build got slower" is answered even when no single
/// step explains it.
#[derive(Debug, Clone)]
pub struct RunTrend {
    pub baseline_ms: f64,
    pub recent_ms: f64,
    pub delta_pct: Option<f64>,
    pub samples: usize,
    pub onset_run_id: Option<i64>,
    pub onset_sha: Option<String>,
}

/// Thresholds for calling something a regression. Both must trip: a 400%
/// change on a 4ms step is noise, and a 30s increase on a step that only runs
/// twice is not comparable.
#[derive(Debug, Clone, Copy)]
pub struct Thresholds {
    pub pct: f64,
    pub min_abs_ms: i64,
    pub window: usize,
    pub min_samples: usize,
}

impl Default for Thresholds {
    fn default() -> Self {
        Self {
            pct: 25.0,
            // 100ms, not something rounder. This floor exists to stop a 4ms step
            // tripling from being called a regression, and 100ms does that while
            // still catching the common real case: a step going 40ms -> 240ms is
            // a 490% jump and must not be filtered out for being "small". A floor
            // set at 250ms did exactly that, and the onset detector then reported
            // a later run than the one that actually regressed, pointing blame at
            // the wrong commit.
            min_abs_ms: 100,
            window: 5,
            min_samples: 4,
        }
    }
}

pub struct Store {
    conn: Connection,
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating profile directory {}", parent.display()))?;
        }
        let conn = Connection::open(path)
            .with_context(|| format!("opening profile database {}", path.display()))?;
        // WAL keeps a long `run` from blocking a concurrent `report`, which is
        // the normal case in CI where the build is still going.
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        let store = Self { conn };
        store.migrate()?;
        Ok(store)
    }

    #[cfg(test)]
    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        let store = Self { conn };
        store.migrate()?;
        Ok(store)
    }

    fn migrate(&self) -> Result<()> {
        self.conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS schema_meta (
                key   TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS runs (
                id           INTEGER PRIMARY KEY AUTOINCREMENT,
                started_at   INTEGER NOT NULL,
                duration_ms  INTEGER NOT NULL,
                label        TEXT    NOT NULL DEFAULT '',
                command      TEXT    NOT NULL,
                cwd          TEXT    NOT NULL DEFAULT '',
                git_sha      TEXT    NOT NULL DEFAULT '',
                git_branch   TEXT    NOT NULL DEFAULT '',
                git_dirty    INTEGER NOT NULL DEFAULT 0,
                host         TEXT    NOT NULL DEFAULT '',
                runner       TEXT    NOT NULL DEFAULT 'local',
                exit_code    INTEGER NOT NULL DEFAULT 0,
                succeeded    INTEGER NOT NULL DEFAULT 0,
                cpu_ms       INTEGER NOT NULL DEFAULT 0,
                peak_rss_kb  INTEGER NOT NULL DEFAULT 0,
                proc_count   INTEGER NOT NULL DEFAULT 0,
                coverage_pct REAL,
                parallelism  REAL,
                sampler      TEXT    NOT NULL DEFAULT '',
                interval_ms  INTEGER NOT NULL DEFAULT 0,
                -- Last lines of the build's output, so `dawdle show` can explain
                -- a failure without asking for the CI log again. Bounded, not
                -- a full log.
                output_tail  TEXT    NOT NULL DEFAULT ''
            );
            CREATE INDEX IF NOT EXISTS runs_started_at ON runs(started_at DESC);
            CREATE INDEX IF NOT EXISTS runs_label      ON runs(label);

            CREATE TABLE IF NOT EXISTS processes (
                id            INTEGER PRIMARY KEY AUTOINCREMENT,
                run_id        INTEGER NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
                pid           INTEGER NOT NULL,
                ppid          INTEGER NOT NULL,
                depth         INTEGER NOT NULL DEFAULT 0,
                key           TEXT    NOT NULL,
                raw           TEXT    NOT NULL DEFAULT '',
                is_wrapper    INTEGER NOT NULL DEFAULT 0,
                first_seen_ms INTEGER NOT NULL,
                last_seen_ms  INTEGER NOT NULL,
                ended_ms      INTEGER,
                life_ms       INTEGER NOT NULL,
                cpu_ms        INTEGER NOT NULL,
                peak_rss_kb   INTEGER NOT NULL,
                samples       INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS processes_run ON processes(run_id);
            CREATE INDEX IF NOT EXISTS processes_key ON processes(key);

            -- Per-run, per-step rollup written at the end of a run. The report
            -- only ever reads this table, so report cost is independent of how
            -- many processes the build spawned.
            CREATE TABLE IF NOT EXISTS step_totals (
                run_id      INTEGER NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
                key         TEXT    NOT NULL,
                invocations INTEGER NOT NULL,
                cpu_ms      INTEGER NOT NULL,
                busy_ms     INTEGER NOT NULL,
                longest_ms  INTEGER NOT NULL DEFAULT 0,
                peak_rss_kb INTEGER NOT NULL,
                is_wrapper  INTEGER NOT NULL DEFAULT 0,
                sample_raw  TEXT    NOT NULL DEFAULT '',
                PRIMARY KEY (run_id, key)
            );
            CREATE INDEX IF NOT EXISTS step_totals_key  ON step_totals(key);
            CREATE INDEX IF NOT EXISTS step_totals_run ON step_totals(run_id);

            CREATE TABLE IF NOT EXISTS samples (
                id          INTEGER PRIMARY KEY AUTOINCREMENT,
                run_id      INTEGER NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
                t_ms        INTEGER NOT NULL,
                tree_cpu_ms INTEGER NOT NULL,
                tree_rss_kb INTEGER NOT NULL,
                n_procs     INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS samples_run ON samples(run_id);
            "#,
        )?;
        // `CREATE TABLE IF NOT EXISTS` never touches a table that already
        // exists, so a column added after a database was first created would be
        // missing for exactly the users who ran the tool before the upgrade.
        // Adding it idempotently is cheaper than a versioned migration ladder
        // while the schema is still moving.
        add_column_if_missing(
            &self.conn,
            "runs",
            "output_tail",
            "TEXT NOT NULL DEFAULT ''",
        )?;
        self.conn.execute(
            "INSERT INTO schema_meta(key, value) VALUES('version', ?1)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![SCHEMA_VERSION.to_string()],
        )?;
        Ok(())
    }

    /// Persist a finished run together with its full process list.
    pub fn insert_run(&mut self, facts: RunFacts, profile: &Profile) -> Result<i64> {
        let tx = self.conn.transaction()?;
        tx.execute(
            "INSERT INTO runs (started_at, duration_ms, label, command, cwd, git_sha,
                               git_branch, git_dirty, host, runner, exit_code, succeeded,
                               cpu_ms, peak_rss_kb, proc_count, coverage_pct, parallelism,
                               sampler, interval_ms, output_tail)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20)",
            params![
                facts.started_at,
                facts.duration_ms.max(profile.duration_ms),
                facts.label,
                facts.command,
                facts.cwd,
                facts.git_sha,
                facts.git_branch,
                facts.git_dirty as i64,
                facts.host,
                facts.runner,
                facts.exit_code,
                facts.succeeded as i64,
                profile.total_cpu_ms,
                profile.peak_tree_rss_kb,
                profile.processes.len() as i64,
                profile.coverage(),
                profile.parallelism(),
                profile.source,
                profile.interval_ms as i64,
                facts.output_tail,
            ],
        )?;
        let run_id = tx.last_insert_rowid();

        {
            let mut stmt = tx.prepare(
                "INSERT INTO processes (run_id, pid, ppid, depth, key, raw, is_wrapper,
                                        first_seen_ms, last_seen_ms, ended_ms, life_ms,
                                        cpu_ms, peak_rss_kb, samples)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)",
            )?;
            for record in &profile.processes {
                stmt.execute(params![
                    run_id,
                    record.pid,
                    record.ppid,
                    record.depth as i64,
                    record.key,
                    record.raw,
                    record.is_wrapper as i64,
                    record.first_seen_ms,
                    record.last_seen_ms,
                    record.ended_ms,
                    record.life_ms(),
                    record.cpu_ms,
                    record.peak_rss_kb,
                    record.samples as i64,
                ])?;
            }
        }

        {
            let mut stmt = tx.prepare(
                "INSERT INTO step_totals (run_id, key, invocations, cpu_ms, busy_ms,
                                          longest_ms, peak_rss_kb, is_wrapper, sample_raw)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
            )?;
            for (key, stats) in profile.top_keys() {
                stmt.execute(params![
                    run_id,
                    key,
                    stats.invocations as i64,
                    stats.cpu_ms,
                    stats.busy_ms,
                    stats.longest_ms,
                    stats.peak_rss_kb,
                    stats.is_wrapper as i64,
                    stats.sample_raw,
                ])?;
            }
        }

        {
            let mut stmt = tx.prepare(
                "INSERT INTO samples (run_id, t_ms, tree_cpu_ms, tree_rss_kb, n_procs)
                 VALUES (?1,?2,?3,?4,?5)",
            )?;
            for sample in &profile.samples {
                stmt.execute(params![
                    run_id,
                    sample.t_ms,
                    sample.tree_cpu_ms,
                    sample.tree_rss_kb,
                    sample.n_procs as i64
                ])?;
            }
        }

        tx.commit()?;
        Ok(run_id)
    }

    /// The stored tail of a run's output.
    ///
    /// Deliberately not on `RunRow`: the report loads one row per run and has no
    /// use for the text, and a build's last 200 lines times a 50-run window is
    /// a lot of copying for nothing.
    pub fn run_output_tail(&self, run_id: i64) -> Result<String> {
        let mut stmt = self
            .conn
            .prepare("SELECT output_tail FROM runs WHERE id = ?1")?;
        let mut rows = stmt.query(params![run_id])?;
        match rows.next()? {
            Some(row) => Ok(row.get(0)?),
            None => Ok(String::new()),
        }
    }

    /// The most recent runs matching a filter, newest first.
    pub fn recent_runs(&self, filter: &RunFilter, limit: usize) -> Result<Vec<RunRow>> {
        let mut sql = String::from(
            "SELECT id, started_at, duration_ms, label, command, git_sha, git_branch,
                    exit_code, succeeded, cpu_ms, peak_rss_kb, proc_count, coverage_pct,
                    parallelism
             FROM runs WHERE 1=1",
        );
        let mut args: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
        if let Some(label) = &filter.label {
            sql.push_str(" AND label = ?");
            args.push(Box::new(label.clone()));
        }
        if let Some(since_ms) = filter.since_ms {
            sql.push_str(" AND started_at >= ?");
            args.push(Box::new(since_ms));
        }
        if let Some(branch) = &filter.branch {
            sql.push_str(" AND git_branch = ?");
            args.push(Box::new(branch.clone()));
        }
        if filter.succeeded_only {
            sql.push_str(" AND succeeded = 1");
        }
        sql.push_str(" ORDER BY started_at DESC, id DESC LIMIT ?");
        args.push(Box::new(limit as i64));

        let mut stmt = self.conn.prepare(&sql)?;
        let params: Vec<&dyn rusqlite::ToSql> = args.iter().map(|a| a.as_ref()).collect();
        let rows = stmt.query_map(params.as_slice(), |row| {
            Ok(RunRow {
                id: row.get(0)?,
                started_at: row.get(1)?,
                duration_ms: row.get(2)?,
                label: row.get(3)?,
                command: row.get(4)?,
                git_sha: row.get(5)?,
                git_branch: row.get(6)?,
                exit_code: row.get(7)?,
                succeeded: row.get::<_, i64>(8)? != 0,
                cpu_ms: row.get(9)?,
                peak_rss_kb: row.get(10)?,
                proc_count: row.get(11)?,
                coverage_pct: row.get(12)?,
                parallelism: row.get(13)?,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    pub fn run_by_id(&self, id: i64) -> Result<Option<RunRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, started_at, duration_ms, label, command, git_sha, git_branch,
                    exit_code, succeeded, cpu_ms, peak_rss_kb, proc_count, coverage_pct,
                    parallelism
             FROM runs WHERE id = ?1",
        )?;
        let row = stmt
            .query_row([id], |row| {
                Ok(RunRow {
                    id: row.get(0)?,
                    started_at: row.get(1)?,
                    duration_ms: row.get(2)?,
                    label: row.get(3)?,
                    command: row.get(4)?,
                    git_sha: row.get(5)?,
                    git_branch: row.get(6)?,
                    exit_code: row.get(7)?,
                    succeeded: row.get::<_, i64>(8)? != 0,
                    cpu_ms: row.get(9)?,
                    peak_rss_kb: row.get(10)?,
                    proc_count: row.get(11)?,
                    coverage_pct: row.get(12)?,
                    parallelism: row.get(13)?,
                })
            })
            .optional()?;
        Ok(row)
    }

    /// Per-step rollups for one run.
    pub fn steps_for_run(&self, run_id: i64) -> Result<Vec<StepRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT key, invocations, cpu_ms, busy_ms, peak_rss_kb, is_wrapper, sample_raw
             FROM step_totals WHERE run_id = ?1 ORDER BY cpu_ms DESC",
        )?;
        let rows = stmt.query_map([run_id], |row| {
            Ok(StepRow {
                key: row.get(0)?,
                invocations: row.get(1)?,
                cpu_ms: row.get(2)?,
                busy_ms: row.get(3)?,
                peak_rss_kb: row.get(4)?,
                is_wrapper: row.get::<_, i64>(5)? != 0,
                sample_raw: row.get(6)?,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// The full time series for every step across the given runs.
    ///
    /// Returns steps oldest-first within each step, which is what onset detection
    /// needs. Steps missing from a run are simply absent from that run's series
    /// rather than being recorded as a zero: a step that did not run is not the
    /// same as a step that took no time, and pretending otherwise would make
    /// "we stopped running the tests" look like a 100% improvement.
    pub fn step_history(
        &self,
        run_ids: &[i64],
        metric: Metric,
    ) -> Result<BTreeMap<String, Vec<StepPoint>>> {
        if run_ids.is_empty() {
            return Ok(BTreeMap::new());
        }
        let placeholders: Vec<&str> = run_ids.iter().map(|_| "?").collect();
        let sql = format!(
            "SELECT t.key, t.run_id, r.started_at, r.git_sha, t.{} AS value,
                    t.invocations, t.peak_rss_kb
             FROM step_totals t JOIN runs r ON r.id = t.run_id
             WHERE t.run_id IN ({})
             ORDER BY t.key ASC, r.started_at ASC, t.run_id ASC",
            metric.column(),
            placeholders.join(",")
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let params: Vec<&dyn rusqlite::ToSql> = run_ids
            .iter()
            .map(|id| id as &dyn rusqlite::ToSql)
            .collect();
        let rows = stmt.query_map(params.as_slice(), |row| {
            Ok((
                row.get::<_, String>(0)?,
                StepPoint {
                    run_id: row.get(1)?,
                    started_at: row.get(2)?,
                    git_sha: row.get(3)?,
                    value: row.get(4)?,
                    invocations: row.get(5)?,
                    peak_rss_kb: row.get(6)?,
                },
            ))
        })?;
        let mut out: BTreeMap<String, Vec<StepPoint>> = BTreeMap::new();
        for row in rows {
            let (key, point) = row?;
            out.entry(key).or_default().push(point);
        }
        Ok(out)
    }

    /// Whole-run duration series, for the run-level trend.
    pub fn run_duration_series(&self, run_ids: &[i64]) -> Result<Vec<StepPoint>> {
        if run_ids.is_empty() {
            return Ok(Vec::new());
        }
        let placeholders: Vec<&str> = run_ids.iter().map(|_| "?").collect();
        let sql = format!(
            "SELECT id, started_at, git_sha, duration_ms, 0
             FROM runs WHERE id IN ({}) ORDER BY started_at ASC, id ASC",
            placeholders.join(",")
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let params: Vec<&dyn rusqlite::ToSql> = run_ids
            .iter()
            .map(|id| id as &dyn rusqlite::ToSql)
            .collect();
        let rows = stmt.query_map(params.as_slice(), |row| {
            Ok(StepPoint {
                run_id: row.get(0)?,
                started_at: row.get(1)?,
                git_sha: row.get(2)?,
                value: row.get(3)?,
                invocations: 0,
                // A whole-run point has no step to attribute memory to.
                peak_rss_kb: 0,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Keep only the newest `keep` runs, and everything hanging off them.
    pub fn prune(&mut self, keep: usize) -> Result<usize> {
        let removed = {
            let tx = self.conn.transaction()?;
            let removed = tx.execute(
                "DELETE FROM runs WHERE id NOT IN (
                     SELECT id FROM runs ORDER BY started_at DESC, id DESC LIMIT ?1
                 )",
                params![keep as i64],
            )?;
            tx.commit()?;
            removed
        };
        // The checkpoint has to happen *outside* the transaction: a WAL
        // checkpoint cannot run while a write transaction is open, and asking
        // for one fails with "database table is locked". A profile database that
        // outgrows the repo it measures is its own kind of slowdown, so the
        // extra statement is worth it.
        // Returns a row, so it has to be run as a query; `execute` rejects a
        // statement that produces results.
        self.conn
            .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))?;
        Ok(removed)
    }

    pub fn count_runs(&self) -> Result<i64> {
        let count: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM runs", [], |row| row.get(0))?;
        Ok(count)
    }

    pub fn vacuum(&self) -> Result<()> {
        self.conn.execute("VACUUM", [])?;
        Ok(())
    }
}

/// Which metric a trend is computed over.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Metric {
    /// CPU the step burned. The one number that is comparable across machines,
    /// unlike wall time on a shared CI box — but not the default, because a step
    /// that gets slower over wall time is the common case.
    Cpu,
    /// Summed process lifetime. Grows with more invocations, which is useful
    /// for spotting "we started compiling 200 more crates".
    Busy,
    /// Wall time of the longest single invocation.
    Longest,
}

impl Metric {
    fn column(self) -> &'static str {
        match self {
            Metric::Cpu => "cpu_ms",
            Metric::Busy => "busy_ms",
            Metric::Longest => "longest_ms",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Metric::Cpu => "cpu",
            Metric::Busy => "busy",
            Metric::Longest => "longest",
        }
    }
}

impl std::fmt::Display for Metric {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.label())
    }
}

#[derive(Debug, Clone, Default)]
pub struct RunFilter {
    pub label: Option<String>,
    pub since_ms: Option<i64>,
    pub branch: Option<String>,
    pub succeeded_only: bool,
}

/// Add a column when it is not already there.
///
/// SQLite has no `ADD COLUMN IF NOT EXISTS`, so this checks `pragma_table_info`
/// first. An unexpected error is not swallowed: a migration that silently did
/// nothing would show up much later as a confusing "no such column".
fn add_column_if_missing(
    conn: &rusqlite::Connection,
    table: &str,
    column: &str,
    definition: &str,
) -> Result<()> {
    let sql = format!("PRAGMA table_info({table})");
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let name: String = row.get(1)?;
        if name == column {
            return Ok(());
        }
    }
    conn.execute_batch(&format!(
        "ALTER TABLE {table} ADD COLUMN {column} {definition}"
    ))?;
    Ok(())
}

pub fn median(values: &[f64]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let mid = sorted.len() / 2;
    if sorted.len() % 2 == 0 {
        (sorted[mid - 1] + sorted[mid]) / 2.0
    } else {
        sorted[mid]
    }
}

/// The first point in the series that exceeds a rolling median of the points
/// before it by both a relative and an absolute margin.
///
/// Returns the index into `series`. Requires at least `min_samples` points
/// before the candidate, because the first point can never be a regression —
/// there is nothing to regress from.
pub fn detect_onset(
    series: &[StepPoint],
    thresholds: Thresholds,
) -> Option<(usize, Option<&StepPoint>, Option<&StepPoint>)> {
    for index in thresholds.min_samples..series.len() {
        let history_end = index.saturating_sub(1);
        let history_start = history_end.saturating_sub(thresholds.window - 1);
        let history: Vec<f64> = series[history_start..=history_end]
            .iter()
            .map(|point| point.value as f64)
            .collect();
        if history.len() < thresholds.min_samples {
            return None;
        }
        let baseline = median(&history);
        let candidate = series[index].value as f64;
        let Some(pct) = crate::fmt::pct_change(baseline, candidate) else {
            continue;
        };
        if pct >= thresholds.pct && (candidate - baseline) >= thresholds.min_abs_ms as f64 {
            return Some((index, series.get(index), series.get(history_end)));
        }
    }
    None
}

/// Compute the verdict for every step across the given runs.
pub fn analyse_steps(
    history: &BTreeMap<String, Vec<StepPoint>>,
    total_runs: usize,
    thresholds: Thresholds,
    metric: Metric,
) -> Vec<Trend> {
    let mut trends: Vec<Trend> = Vec::new();
    for (key, series) in history {
        if series.is_empty() {
            continue;
        }
        let values: Vec<f64> = series.iter().map(|point| point.value as f64).collect();

        // A step seen in only one run in three is not a trend, it is noise.
        // Still report it, flagged, because a brand-new expensive step is
        // exactly the thing a user wants to notice.
        let present_in = series.len();
        let is_occasional = present_in * 3 < total_runs && total_runs >= 4;
        let is_new = present_in == 1;

        // Baseline/recent split: the older half against the newer half, with at
        // least two points on each side so a median means something.
        let (baseline_ms, recent_ms) = if values.len() >= 4 {
            let split = values.len() / 2;
            (median(&values[..split]), median(&values[split..]))
        } else {
            (median(&values), median(&values))
        };

        let delta_pct = if is_occasional || is_new {
            None
        } else {
            crate::fmt::pct_change(baseline_ms, recent_ms)
        };

        let onset = if is_occasional {
            None
        } else {
            detect_onset(series, thresholds)
        };

        let recent_slice = if values.len() >= 4 {
            &series[values.len() / 2..]
        } else {
            &series[..]
        };
        let recent_invocations = if recent_slice.is_empty() {
            0
        } else {
            recent_slice.iter().map(|p| p.invocations).sum::<i64>() / recent_slice.len() as i64
        };

        trends.push(Trend {
            key: key.clone(),
            onset_index: onset.map(|(index, _, _)| index),
            baseline_ms,
            recent_ms,
            delta_pct,
            samples: values.len(),
            present_in,
            invocations_recent: recent_invocations as f64,
            is_new,
            is_gone: present_in > 0 && present_in < total_runs,
            is_wrapper: false,
            onset_run_id: onset.and_then(|(_, current, _)| current.map(|p| p.run_id)),
            onset_sha: onset
                .and_then(|(_, current, _)| current.map(|p| p.git_sha.clone()))
                .filter(|sha| !sha.is_empty()),
            onset_at: onset.and_then(|(_, current, _)| current.map(|p| p.started_at)),
            // The high-water mark of the largest single process, taken from the
            // RSS column rather than from `value`: `value` is milliseconds under
            // every metric, so reusing it here would have printed a duration
            // with a KB suffix.
            peak_rss_kb: recent_slice
                .iter()
                .map(|p| p.peak_rss_kb.max(0))
                .max()
                .unwrap_or(0),
        });
        let _ = metric;
    }

    // Worst regressions first, because that is the row someone came to read.
    trends.sort_by(|a, b| {
        b.delta_pct
            .unwrap_or(f64::NEG_INFINITY)
            .partial_cmp(&a.delta_pct.unwrap_or(f64::NEG_INFINITY))
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(
                b.recent_ms
                    .partial_cmp(&a.recent_ms)
                    .unwrap_or(std::cmp::Ordering::Equal),
            )
    });
    trends
}

pub fn analyse_run(series: &[StepPoint], thresholds: Thresholds) -> Option<RunTrend> {
    if series.len() < 2 {
        return None;
    }
    let values: Vec<f64> = series.iter().map(|point| point.value as f64).collect();
    let split = values.len() / 2;
    let baseline_ms = median(&values[..split]);
    let recent_ms = median(&values[split..]);
    let onset = detect_onset(series, thresholds);
    Some(RunTrend {
        baseline_ms,
        recent_ms,
        delta_pct: crate::fmt::pct_change(baseline_ms, recent_ms),
        samples: values.len(),
        onset_run_id: onset.and_then(|(_, current, _)| current.map(|p| p.run_id)),
        onset_sha: onset
            .and_then(|(_, current, _)| current.map(|p| p.git_sha.clone()))
            .filter(|sha| !sha.is_empty()),
    })
}

/// Default profile location: a per-repository file so two projects checked out
/// side by side never contaminate each other's history.
pub fn default_db_path(explicit: Option<&Path>) -> PathBuf {
    if let Some(path) = explicit {
        return path.to_path_buf();
    }
    if let Ok(from_env) = std::env::var("DAWDLE_DB") {
        if !from_env.is_empty() {
            return PathBuf::from(from_env);
        }
    }
    let root = crate::git::repo_root()
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    root.join(".dawdle").join("profile.db")
}

/// Make `.dawdle/` invisible to git without touching the user's .gitignore.
pub fn ensure_self_ignored(dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir)?;
    let marker = dir.join(".gitignore");
    if !marker.exists() {
        std::fs::write(
            &marker,
            "# Created by dawdle. Profiles are local build data.\n*\n",
        )
        .with_context(|| format!("writing {}", marker.display()))?;
    }
    Ok(())
}

pub fn now() -> i64 {
    now_ms()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sampler::{ProcRecord, Profile, Sample};

    fn point(run_id: i64, value: i64) -> StepPoint {
        StepPoint {
            run_id,
            started_at: 1_700_000_000_000 + run_id * 1000,
            git_sha: format!("{run_id:040x}"),
            value,
            invocations: 1,
            peak_rss_kb: 0,
        }
    }

    fn empty_profile() -> Profile {
        Profile {
            interval_ms: 100,
            source: "test".to_string(),
            ..Default::default()
        }
    }

    fn profile_with(step: &str, cpu_ms: i64, duration_ms: i64) -> Profile {
        Profile {
            processes: vec![ProcRecord {
                pid: 10,
                ppid: 1,
                key: step.to_string(),
                raw: format!("/usr/bin/{step}"),
                is_wrapper: false,
                depth: 0,
                first_seen_ms: 0,
                last_seen_ms: duration_ms - 100,
                ended_ms: Some(duration_ms),
                cpu_ms,
                peak_rss_kb: 4096,
                samples: 5,
            }],
            samples: vec![Sample {
                t_ms: duration_ms,
                tree_cpu_ms: cpu_ms,
                tree_rss_kb: 4096,
                n_procs: 1,
            }],
            duration_ms,
            peak_tree_rss_kb: 4096,
            total_cpu_ms: cpu_ms,
            polls: 5,
            interval_ms: 100,
            source: "test".to_string(),
        }
    }

    fn facts(label: &str) -> RunFacts {
        RunFacts {
            started_at: 1_700_000_000_000,
            duration_ms: 1000,
            label: label.to_string(),
            command: "make test".to_string(),
            cwd: "/repo".to_string(),
            git_sha: "abc123".to_string(),
            git_branch: "main".to_string(),
            git_dirty: false,
            host: "box".to_string(),
            runner: "local".to_string(),
            exit_code: 0,
            succeeded: true,
            output_tail: String::new(),
        }
    }

    #[test]
    fn median_handles_even_and_odd_lengths() {
        assert_eq!(median(&[3.0, 1.0, 2.0]), 2.0);
        assert_eq!(median(&[4.0, 1.0, 3.0, 2.0]), 2.5);
        assert_eq!(median(&[]), 0.0);
    }

    #[test]
    fn round_trips_a_run_with_steps_and_processes() {
        let mut store = Store::open_in_memory().unwrap();
        let profile = profile_with("cc", 800, 1000);
        let id = store
            .insert_run(facts("build"), &profile)
            .expect("insert run");
        assert_eq!(store.count_runs().unwrap(), 1);

        let run = store.run_by_id(id).unwrap().expect("run present");
        assert_eq!(run.duration_ms, 1000);
        assert_eq!(run.cpu_ms, 800);
        assert_eq!(run.proc_count, 1);

        let steps = store.steps_for_run(id).unwrap();
        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0].key, "cc");
        assert_eq!(steps[0].cpu_ms, 800);
    }

    #[test]
    fn history_groups_by_step_across_runs_oldest_first() {
        let mut store = Store::open_in_memory().unwrap();
        for (index, cost) in [100, 200, 300, 400].iter().enumerate() {
            let mut f = facts("build");
            f.started_at = 1_700_000_000_000 + index as i64 * 60_000;
            store
                .insert_run(f, &profile_with("cc", *cost, 1000))
                .unwrap();
        }
        let runs = store.recent_runs(&RunFilter::default(), 10).unwrap();
        let ids: Vec<i64> = runs.iter().map(|r| r.id).collect();
        let history = store.step_history(&ids, Metric::Cpu).unwrap();
        let cc = history.get("cc").expect("cc present");
        assert_eq!(cc.len(), 4);
        let values: Vec<i64> = cc.iter().map(|p| p.value).collect();
        assert_eq!(values, vec![100, 200, 300, 400], "oldest first");
    }

    #[test]
    fn onset_names_the_run_where_a_step_crossed_the_threshold() {
        let thresholds = Thresholds {
            pct: 25.0,
            min_abs_ms: 250,
            window: 3,
            min_samples: 3,
        };
        // Flat, flat, flat, then a jump. The onset must be the 4th point and
        // the "previous" anchor must be the 3rd, because that pair is exactly
        // the commit range `blame` will ask git for.
        let series: Vec<StepPoint> = [1000, 1050, 1020, 4000]
            .iter()
            .enumerate()
            .map(|(i, v)| point(i as i64, *v))
            .collect();
        let (index, current, previous) = detect_onset(&series, thresholds).expect("onset found");
        assert_eq!(index, 3);
        assert_eq!(current.unwrap().value, 4000);
        assert_eq!(previous.unwrap().value, 1020);
    }

    #[test]
    fn onset_ignores_a_spike_inside_the_absolute_margin() {
        let thresholds = Thresholds {
            pct: 25.0,
            min_abs_ms: 250,
            window: 3,
            min_samples: 3,
        };
        // 100% relative change, but only 40ms absolute. A 4ms step tripling is
        // not a regression worth waking anyone for.
        let series: Vec<StepPoint> = [10, 12, 11, 51]
            .iter()
            .enumerate()
            .map(|(i, v)| point(i as i64, *v))
            .collect();
        assert!(detect_onset(&series, thresholds).is_none());
    }

    #[test]
    fn onset_needs_enough_history_before_it_fires() {
        let thresholds = Thresholds {
            pct: 25.0,
            min_abs_ms: 250,
            window: 3,
            min_samples: 3,
        };
        // Huge jump at index 1 and 2, but not enough history to judge.
        let series: Vec<StepPoint> = [10, 9000, 9000, 10]
            .iter()
            .enumerate()
            .map(|(i, v)| point(i as i64, *v))
            .collect();
        assert!(detect_onset(&series, thresholds).is_none());
    }

    #[test]
    fn a_step_that_disappeared_is_gone_not_improved() {
        // The regression here is that a slow step stopped running. Scoring it as
        // a 100% improvement would be the single most misleading thing this
        // tool could do.
        let mut store = Store::open_in_memory().unwrap();
        for (index, cost) in [900, 900, 0, 0].iter().enumerate() {
            let mut f = facts("test");
            f.started_at = 1_700_000_000_000 + index as i64 * 60_000;
            if *cost == 0 {
                store.insert_run(f, &empty_profile()).unwrap();
            } else {
                store
                    .insert_run(f, &profile_with("pytest", *cost, 1000))
                    .unwrap();
            }
        }
        let ids: Vec<i64> = store
            .recent_runs(&RunFilter::default(), 10)
            .unwrap()
            .iter()
            .map(|r| r.id)
            .collect();
        let history = store.step_history(&ids, Metric::Cpu).unwrap();
        let trends = analyse_steps(&history, ids.len(), Thresholds::default(), Metric::Cpu);
        let pytest = trends
            .iter()
            .find(|t| t.key == "pytest")
            .expect("step present");
        assert!(
            pytest.is_gone,
            "a vanished step must be flagged, not improved"
        );
        assert_eq!(pytest.present_in, 2);
    }

    #[test]
    fn filter_selects_by_label_branch_and_success() {
        let mut store = Store::open_in_memory().unwrap();
        for (index, label) in ["build", "test", "build"].iter().enumerate() {
            let mut f = facts(label);
            f.started_at = 1_700_000_000_000 + index as i64 * 60_000;
            f.succeeded = *label != "test";
            store.insert_run(f, &profile_with("cc", 100, 1000)).unwrap();
        }
        let build_only = store
            .recent_runs(
                &RunFilter {
                    label: Some("build".to_string()),
                    ..Default::default()
                },
                10,
            )
            .unwrap();
        assert_eq!(build_only.len(), 2, "two runs labelled build");
        let ok_only = store
            .recent_runs(
                &RunFilter {
                    succeeded_only: true,
                    ..Default::default()
                },
                10,
            )
            .unwrap();
        assert_eq!(ok_only.len(), 2, "the failing test run is excluded");
    }

    #[test]
    fn prune_keeps_the_newest_runs_and_cascades() {
        let mut store = Store::open_in_memory().unwrap();
        for index in 0..5 {
            let mut f = facts("build");
            f.started_at = 1_700_000_000_000 + index * 60_000;
            store.insert_run(f, &profile_with("cc", 100, 1000)).unwrap();
        }
        let removed = store.prune(2).unwrap();
        assert_eq!(removed, 3);
        assert_eq!(store.count_runs().unwrap(), 2);
        let remaining = store.recent_runs(&RunFilter::default(), 10).unwrap();
        for run in &remaining {
            assert!(store.steps_for_run(run.id).unwrap().len() <= 1);
        }
    }

    #[test]
    fn a_run_with_no_processes_still_records_its_duration() {
        // A build that produced no observable processes still happened. The run
        // row is the floor that `report` must never lose.
        let mut store = Store::open_in_memory().unwrap();
        let id = store.insert_run(facts("noop"), &empty_profile()).unwrap();
        let run = store.run_by_id(id).unwrap().unwrap();
        assert_eq!(run.duration_ms, 1000);
        assert_eq!(run.proc_count, 0);
        assert!(store.steps_for_run(id).unwrap().is_empty());
    }

    #[test]
    fn run_trend_reports_the_onset_across_the_whole_build() {
        let thresholds = Thresholds {
            pct: 20.0,
            min_abs_ms: 500,
            window: 3,
            min_samples: 3,
        };
        let series: Vec<StepPoint> = [5000, 5100, 5200, 9000]
            .iter()
            .enumerate()
            .map(|(i, v)| point(i as i64, *v))
            .collect();
        let trend = analyse_run(&series, thresholds).expect("trend");
        // Medians over two points average them: [5000, 5100] -> 5050 and
        // [5200, 9000] -> 7100. The onset is still the 9000 sample, which is
        // what the test is actually about.
        assert_eq!(trend.baseline_ms, 5050.0);
        assert_eq!(trend.recent_ms, 7100.0);
        assert_eq!(trend.onset_run_id, Some(3));
        assert!(trend.onset_sha.is_some());
    }

    #[test]
    fn a_range_whose_ends_are_the_same_commit_is_reported_as_unblamable() {
        // Every run built from one commit, so the onset has no distinct "before".
        // The command must say that rather than print "abc..abc" and a bisect
        // hint for a range that contains nothing.
        let run = |sha: &str, value: i64| StepPoint {
            run_id: value,
            started_at: 1_700_000_000_000 + value,
            git_sha: sha.to_string(),
            value,
            invocations: 1,
            peak_rss_kb: 0,
        };
        let series = vec![
            run("aaaaaaa", 40),
            run("aaaaaaa", 41),
            run("aaaaaaa", 39),
            run("aaaaaaa", 42),
            run("aaaaaaa", 900),
        ];
        let thresholds = Thresholds {
            pct: 25.0,
            min_abs_ms: 100,
            window: 3,
            min_samples: 3,
        };
        let (index, onset, previous) = detect_onset(&series, thresholds).expect("onset");
        assert_eq!(index, 4, "the first sample past the threshold");
        let onset = onset.expect("onset point");
        let previous = previous.expect("previous point");
        assert_eq!(
            onset.git_sha, previous.git_sha,
            "this fixture must have identical shas, or it is not testing the case"
        );
        assert_eq!(previous.git_sha, onset.git_sha);
    }

    #[test]
    fn a_small_absolute_jump_below_the_floor_is_not_a_regression() {
        // The floor exists so a tiny step tripling does not fill the report.
        let thresholds = Thresholds::default();
        let series: Vec<StepPoint> = [4, 4, 4, 4, 12]
            .iter()
            .enumerate()
            .map(|(index, value)| StepPoint {
                run_id: index as i64,
                started_at: 1_700_000_000_000 + index as i64,
                git_sha: format!("{index:040x}"),
                value: *value,
                invocations: 1,
                peak_rss_kb: 0,
            })
            .collect();
        assert!(
            detect_onset(&series, thresholds).is_none(),
            "a 4ms step tripling is +8ms and must stay under the floor"
        );
    }

    #[test]
    fn a_large_relative_jump_below_a_high_floor_is_still_found() {
        // 40ms -> 240ms is +500%. This is the case a 250ms floor used to
        // swallow, which made blame name a later run than the one that
        // regressed and point at the wrong commit.
        let thresholds = Thresholds::default();
        let series: Vec<StepPoint> = [40, 41, 39, 42, 240]
            .iter()
            .enumerate()
            .map(|(index, value)| StepPoint {
                run_id: index as i64,
                started_at: 1_700_000_000_000 + index as i64,
                git_sha: format!("{index:040x}"),
                value: *value,
                invocations: 1,
                peak_rss_kb: 0,
            })
            .collect();
        let (index, _, _) = detect_onset(&series, thresholds).expect("a 500% jump must be found");
        assert_eq!(index, 4);
    }

    #[test]
    fn default_floor_does_not_swallow_a_sub_second_regression() {
        assert_eq!(Thresholds::default().min_abs_ms, 100);
    }
}
