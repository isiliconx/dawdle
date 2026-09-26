//! Command-line parsing.
//!
//! Hand-rolled rather than derived, for two reasons: dawdle ships as one static
//! binary and a parser is the single largest dependency most CLIs carry, and the
//! flag set is small enough that hand-writing it produces better error messages
//! than a generic one.

use std::path::PathBuf;

use anyhow::{bail, Result};

use crate::db::{Metric, Thresholds};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Debug, Clone)]
pub struct RunArgs {
    pub command: Vec<String>,
    pub label: String,
    pub interval_ms: u64,
    pub db: Option<PathBuf>,
    pub json: bool,
    pub quiet: bool,
}

#[derive(Debug, Clone)]
pub struct ReportArgs {
    pub last: usize,
    pub since_ms: Option<i64>,
    pub label: Option<String>,
    pub branch: Option<String>,
    pub metric: Metric,
    pub thresholds: Thresholds,
    pub top: usize,
    pub markdown: bool,
    pub json: bool,
    pub show_all: bool,
    pub db: Option<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct BlameArgs {
    pub target: String,
    pub last: usize,
    pub since_ms: Option<i64>,
    pub label: Option<String>,
    pub metric: Metric,
    pub thresholds: Thresholds,
    pub max_commits: usize,
    pub json: bool,
    pub db: Option<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct ShowArgs {
    pub run_id: i64,
    pub processes: bool,
    pub json: bool,
    pub db: Option<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct LsArgs {
    pub last: usize,
    pub json: bool,
    pub db: Option<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct PruneArgs {
    pub keep: usize,
    pub vacuum: bool,
    pub db: Option<PathBuf>,
}

#[derive(Debug, Clone)]
pub enum Command {
    Run(RunArgs),
    Report(ReportArgs),
    Blame(BlameArgs),
    Show(ShowArgs),
    Ls(LsArgs),
    Prune(PruneArgs),
    Help(String),
    Version,
}

pub const HELP: &str = r#"dawdle — find the step that made your build slow, and the commit that did it

USAGE
  dawdle run [options] -- <command> [args...]
  dawdle report [options]
  dawdle blame <step|total> [options]
  dawdle show <run-id> [options]
  dawdle ls [options]
  dawdle prune [options]

COMMANDS
  run       Run a build, sampling it, and record the result.
  report    Compare recent runs and show which steps changed.
  blame     Name the commit range where a step regressed.
  show      Everything dawdle saw during one run.
  ls        List recorded runs.
  prune     Delete old runs, keeping the newest.

RUN OPTIONS
  -l, --label <text>     Tag the run so it can be compared against others
                         (for example "ci" or "release").
  -i, --interval <ms>    Sampling interval, default 20. This is the shortest
                         step dawdle can see: lower catches more, costs more.
  -q, --quiet            Do not print a summary when the build finishes.
      --db <path>        Profile database, default ./.dawdle/profile.db
      --json             Print the run summary as JSON.
      --                 Everything after this is the command to run.

REPORT / BLAME OPTIONS
  -n, --last <N>         Runs to compare, default 20.
      --since <window>   Only runs newer than this: 45m, 6h, 7d, 4w.
      --label <text>     Only runs with this label.
      --branch <name>    Only runs on this git branch.
  -m, --metric <metric>  busy (default), cpu, or longest.
      --threshold <pct>  Percentage change that counts as a regression,
                         default 25.
      --min-ms <ms>      Absolute change that also has to be exceeded,
                         default 100. Stops a 4ms step tripling from counting.
      --top <N>          Rows in the report, default 20.
      --all              Include steps that did not regress.
      --markdown         Emit a table ready to paste as a PR comment.
      --json             Emit JSON.
      --db <path>        Profile database.

BLAME
  <target>               A step key, a prefix of one, or "total" for the whole
                         build.

SHOW OPTIONS
  -p, --processes        List every sampled process, not just step rollups.

PRUNE OPTIONS
  -k, --keep <N>         Newest runs to keep, default 50.
      --vacuum           Compact the database file after pruning.

ENVIRONMENT
  DAWDLE_DB          Override the profile database path.
  DAWDLE_INTERVAL    Override the default sampling interval, in ms.
  NO_COLOR           Disable colour, as does DAWDLE_NO_COLOR.

EXAMPLES
  dawdle run --label ci -- make test
  dawdle run -- cargo test --lib
  dawdle report --since 7d
  dawdle report --metric busy --threshold 15
  dawdle blame "cargo test"
  dawdle blame total --since 30d
"#;

/// A cursor over the raw argv, with typed accessors that produce a good error
/// message naming the flag they failed on.
struct Cursor {
    items: Vec<String>,
    position: usize,
}

impl Cursor {
    fn new(items: Vec<String>) -> Self {
        Self { items, position: 0 }
    }

    fn peek(&self) -> Option<&String> {
        self.items.get(self.position)
    }

    /// Consume the next token. Callers that handle a flag must call this to step
    /// over the flag *before* reading its value, or the value read comes back as
    /// the flag itself.
    fn take(&mut self) -> Option<String> {
        let item = self.items.get(self.position).cloned();
        if item.is_some() {
            self.position += 1;
        }
        item
    }

    /// Consume a flag's value, or fail with a message that says which flag.
    fn value(&mut self, flag: &str) -> Result<String> {
        match self.take() {
            Some(value) => Ok(value),
            None => bail!("{flag} needs a value"),
        }
    }

    fn number<T: std::str::FromStr>(&mut self, flag: &str) -> Result<T> {
        let raw = self.value(flag)?;
        raw.parse::<T>()
            .map_err(|_| anyhow::anyhow!("{flag} needs a number, got {raw:?}"))
    }

    /// Consume the next token only if it is one of `names`, returning the flag
    /// that matched. A token that is not in `names` is left in place so the next
    /// parser in the chain gets a chance at it.
    ///
    /// This is what makes the flag chain below safe: the flag is always stepped
    /// over *before* its value is read, and an unrecognised token never gets
    /// silently eaten.
    fn take_if(&mut self, names: &[&str]) -> Option<String> {
        match self.peek() {
            Some(next) if names.contains(&next.as_str()) => {
                let matched = next.clone();
                self.position += 1;
                Some(matched)
            }
            _ => None,
        }
    }

    /// Everything left over, for `run -- <command>`.
    fn rest(&mut self) -> Vec<String> {
        let rest = self.items[self.position..].to_vec();
        self.position = self.items.len();
        rest
    }
}

/// Parse a duration like `45m`, `6h`, `7d`, `4w`, or a bare `30` meaning
/// minutes. Returns milliseconds.
pub fn parse_window(text: &str) -> Result<i64> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        bail!("--since needs a window like 7d");
    }
    let (digits, multiplier) = match trimmed.chars().last() {
        Some('m') => (&trimmed[..trimmed.len() - 1], 60_000i64),
        Some('h') => (&trimmed[..trimmed.len() - 1], 3_600_000),
        Some('d') => (&trimmed[..trimmed.len() - 1], 86_400_000),
        Some('w') => (&trimmed[..trimmed.len() - 1], 604_800_000),
        _ => (trimmed, 60_000),
    };
    let count: i64 = digits
        .parse()
        .map_err(|_| anyhow::anyhow!("--since got {text:?}; try 45m, 6h, 7d or 4w"))?;
    if count <= 0 {
        bail!("--since must be positive, got {text:?}");
    }
    Ok(count * multiplier)
}

pub fn parse_metric(text: &str) -> Result<Metric> {
    match text {
        "cpu" => Ok(Metric::Cpu),
        "busy" => Ok(Metric::Busy),
        "longest" => Ok(Metric::Longest),
        other => bail!("--metric got {other:?}; try cpu, busy or longest"),
    }
}

fn check_thresholds(thresholds: &Thresholds) -> Result<()> {
    if thresholds.pct < 0.0 {
        bail!("--threshold cannot be negative");
    }
    if thresholds.min_abs_ms < 0 {
        bail!("--min-ms cannot be negative");
    }
    Ok(())
}

pub fn parse(argv: Vec<String>) -> Result<Command> {
    if argv.is_empty() {
        return Ok(Command::Help(HELP.to_string()));
    }
    let mut cursor = Cursor::new(argv);
    let verb = cursor.take().unwrap_or_default();

    match verb.as_str() {
        "-h" | "--help" | "help" => {
            // `dawdle help report` should show the topic, not everything.
            let topic = cursor.rest().join(" ");
            return Ok(Command::Help(if topic.trim().is_empty() {
                HELP.to_string()
            } else {
                topic
            }));
        }
        "-V" | "--version" | "version" => return Ok(Command::Version),
        _ => {}
    }

    match verb.as_str() {
        "run" | "record" => parse_run(&mut cursor),
        "report" | "r" => parse_report(&mut cursor),
        "blame" | "why" => parse_blame(&mut cursor),
        "show" | "inspect" => parse_show(&mut cursor),
        "ls" | "list" | "runs" => parse_ls(&mut cursor),
        "prune" | "clean" => parse_prune(&mut cursor),
        other => {
            // A bare command is a convenience: `dawdle make test` records it.
            if other.starts_with('-') {
                bail!("unknown option {other:?}. Try `dawdle --help`.");
            }
            Ok(Command::Run(RunArgs {
                command: {
                    let mut full = vec![other.to_string()];
                    full.extend(cursor.rest());
                    full
                },
                label: String::new(),
                interval_ms: default_interval(),
                db: None,
                json: false,
                quiet: false,
            }))
        }
    }
}

/// Default sampling interval.
///
/// 20ms, not the 100ms that feels like an obvious default: sampling is a race
/// against process exit, so the interval is the *shortest step dawdle can
/// possibly see*. A `python3 -c` that runs for 30ms is invisible at 100ms, and
/// a test suite of such steps reports no CPU at all while looking like it
/// worked. Reading `/proc` is cheap enough that 50 polls a second costs less
/// than the build it is measuring.
pub fn default_interval() -> u64 {
    std::env::var("DAWDLE_INTERVAL")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|v: &u64| *v >= 10)
        .unwrap_or(20)
}

fn parse_run(cursor: &mut Cursor) -> Result<Command> {
    let mut label = String::new();
    let mut interval_ms = default_interval();
    let mut db = None;
    let mut json = false;
    let mut quiet = false;

    let mut saw_separator = false;
    while let Some(flag) = cursor.take_if(&[
        "-l",
        "--label",
        "-i",
        "--interval",
        "-q",
        "--quiet",
        "--json",
        "--db",
        "--",
    ]) {
        match flag.as_str() {
            "-l" | "--label" => label = cursor.value("--label")?,
            "-i" | "--interval" => {
                interval_ms = cursor.number("--interval")?;
                if interval_ms < 10 {
                    bail!("--interval must be at least 10ms, got {interval_ms}");
                }
            }
            "-q" | "--quiet" => quiet = true,
            "--json" => json = true,
            "--db" => db = Some(PathBuf::from(cursor.value("--db")?)),
            _ => saw_separator = true,
        }
    }

    // Whatever is left is the command to run. The `--` separator is a nicety,
    // not a requirement, so `dawdle run make test` works too. An unrecognised
    // leading flag is a typo, and profiling nothing because of a typo would be
    // indistinguishable from a fast build.
    let command: Vec<String> = cursor.rest();
    if let Some(first) = command.first() {
        if !saw_separator && first.starts_with('-') {
            bail!(
                "unknown option {first:?} for `dawdle run`.\n\
                 Put `--` before the command if it takes flags: dawdle run -- {first} ..."
            );
        }
    }
    if command.is_empty() {
        bail!("nothing to run. Try: dawdle run -- make test");
    }
    Ok(Command::Run(RunArgs {
        command,
        label,
        interval_ms,
        db,
        json,
        quiet,
    }))
}

fn parse_report(cursor: &mut Cursor) -> Result<Command> {
    let mut args = ReportArgs {
        last: 20,
        since_ms: None,
        label: None,
        branch: None,
        // Wall time, not CPU: a build getting slower is a statement about
        // duration, and under a CPU default a wrapper like `make` — which burns
        // no CPU of its own — reads as a permanently flat row of nothing.
        metric: Metric::Busy,
        thresholds: Thresholds::default(),
        top: 20,
        markdown: false,
        json: false,
        show_all: false,
        db: None,
    };
    // One pass, every flag. Chaining separate loops per flag group silently
    // makes argument order significant: `report --last 50 --threshold 10
    // --markdown` would fail because --markdown belongs to a group the parser
    // had already walked past.
    while let Some(flag) = cursor.take_if(&[
        "-n",
        "--last",
        "-m",
        "--metric",
        "--top",
        "--markdown",
        "--json",
        "--all",
        "--threshold",
        "--min-ms",
        "--label",
        "--branch",
        "--since",
        "--db",
    ]) {
        match flag.as_str() {
            "-n" | "--last" => args.last = cursor.number("--last")?,
            "-m" | "--metric" => args.metric = parse_metric(&cursor.value("--metric")?)?,
            "--top" => args.top = cursor.number("--top")?,
            "--threshold" => args.thresholds.pct = cursor.number("--threshold")?,
            "--min-ms" => args.thresholds.min_abs_ms = cursor.number("--min-ms")?,
            "--label" => args.label = Some(cursor.value("--label")?),
            "--branch" => args.branch = Some(cursor.value("--branch")?),
            "--since" => args.since_ms = Some(parse_window(&cursor.value("--since")?)?),
            "--db" => args.db = Some(PathBuf::from(cursor.value("--db")?)),
            "--markdown" => args.markdown = true,
            "--json" => args.json = true,
            _ => args.show_all = true,
        }
    }
    check_thresholds(&args.thresholds)?;
    if args.last == 0 {
        bail!("--last must be at least 1");
    }
    if cursor.peek().is_some() {
        bail!(
            "unexpected argument {:?} for `dawdle report`",
            cursor.peek().unwrap()
        );
    }
    Ok(Command::Report(args))
}

fn parse_blame(cursor: &mut Cursor) -> Result<Command> {
    let Some(target) = cursor.take() else {
        bail!(
            "`dawdle blame` needs a step key, or `total` for the whole build.\n\
               Run `dawdle report` first to see the step keys."
        );
    };
    if target.starts_with('-') {
        bail!("`dawdle blame` needs a step key first, then options. Got {target:?}.");
    }
    let mut args = BlameArgs {
        target,
        last: 20,
        since_ms: None,
        label: None,
        metric: Metric::Busy,
        thresholds: Thresholds::default(),
        max_commits: 20,
        json: false,
        db: None,
    };
    while let Some(flag) = cursor.take_if(&[
        "-n",
        "--last",
        "-m",
        "--metric",
        "--max-commits",
        "--json",
        "--threshold",
        "--min-ms",
        "--label",
        "--since",
        "--db",
    ]) {
        match flag.as_str() {
            "-n" | "--last" => args.last = cursor.number("--last")?,
            "-m" | "--metric" => args.metric = parse_metric(&cursor.value("--metric")?)?,
            "--max-commits" => args.max_commits = cursor.number("--max-commits")?,
            "--threshold" => args.thresholds.pct = cursor.number("--threshold")?,
            "--min-ms" => args.thresholds.min_abs_ms = cursor.number("--min-ms")?,
            "--label" => args.label = Some(cursor.value("--label")?),
            "--since" => args.since_ms = Some(parse_window(&cursor.value("--since")?)?),
            "--db" => args.db = Some(PathBuf::from(cursor.value("--db")?)),
            // `blame` deliberately has no --branch: the commit range it prints is
            // the interesting part, and narrowing by branch usually empties it.
            _ => args.json = true,
        }
    }
    check_thresholds(&args.thresholds)?;
    if args.last == 0 {
        bail!("--last must be at least 1");
    }
    if cursor.peek().is_some() {
        bail!(
            "unexpected argument {:?} for `dawdle blame`",
            cursor.peek().unwrap()
        );
    }
    Ok(Command::Blame(args))
}

fn parse_show(cursor: &mut Cursor) -> Result<Command> {
    let Some(raw) = cursor.take() else {
        bail!("`dawdle show` needs a run id. Try `dawdle ls`.");
    };
    let run_id = raw
        .parse::<i64>()
        .map_err(|_| anyhow::anyhow!("run id must be a number, got {raw:?}"))?;
    let mut args = ShowArgs {
        run_id,
        processes: false,
        json: false,
        db: None,
    };
    while let Some(flag) = cursor.take_if(&["-p", "--processes", "--json", "--db"]) {
        match flag.as_str() {
            "-p" | "--processes" => args.processes = true,
            "--json" => args.json = true,
            _ => args.db = Some(PathBuf::from(cursor.value("--db")?)),
        }
    }
    if let Some(extra) = cursor.peek() {
        bail!("unknown option {extra:?} for `dawdle show`");
    }
    Ok(Command::Show(args))
}

fn parse_ls(cursor: &mut Cursor) -> Result<Command> {
    let mut args = LsArgs {
        last: 20,
        json: false,
        db: None,
    };
    while let Some(flag) = cursor.take_if(&["-n", "--last", "--json", "--db"]) {
        match flag.as_str() {
            "-n" | "--last" => args.last = cursor.number("--last")?,
            "--json" => args.json = true,
            _ => args.db = Some(PathBuf::from(cursor.value("--db")?)),
        }
    }
    if let Some(extra) = cursor.peek() {
        bail!("unknown option {extra:?} for `dawdle ls`");
    }
    Ok(Command::Ls(args))
}

fn parse_prune(cursor: &mut Cursor) -> Result<Command> {
    let mut args = PruneArgs {
        keep: 50,
        vacuum: false,
        db: None,
    };
    while let Some(flag) = cursor.take_if(&["-k", "--keep", "--vacuum", "--db"]) {
        match flag.as_str() {
            "-k" | "--keep" => args.keep = cursor.number("--keep")?,
            "--vacuum" => args.vacuum = true,
            _ => args.db = Some(PathBuf::from(cursor.value("--db")?)),
        }
    }
    if let Some(extra) = cursor.peek() {
        bail!("unknown option {extra:?} for `dawdle prune`");
    }
    Ok(Command::Prune(args))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(words: &[&str]) -> Vec<String> {
        words.iter().map(|w| w.to_string()).collect()
    }

    fn run_args(words: &[&str]) -> RunArgs {
        match parse(args(words)).unwrap() {
            Command::Run(parsed) => parsed,
            other => panic!("expected run, got {other:?}"),
        }
    }

    #[test]
    fn parses_run_with_an_explicit_separator() {
        let parsed = run_args(&["run", "--label", "ci", "--", "make", "-j8", "test"]);
        assert_eq!(parsed.command, vec!["make", "-j8", "test"]);
        assert_eq!(parsed.label, "ci");
    }

    #[test]
    fn parses_run_without_a_separator() {
        // The separator is a nicety, not a requirement. `dawdle run make test`
        // should obviously work.
        let parsed = run_args(&["run", "make", "test"]);
        assert_eq!(parsed.command, vec!["make", "test"]);
    }

    #[test]
    fn a_bare_command_is_treated_as_a_run() {
        let parsed = run_args(&["cargo", "test", "--lib"]);
        assert_eq!(parsed.command, vec!["cargo", "test", "--lib"]);
    }

    #[test]
    fn flags_after_the_command_belong_to_the_command() {
        // This is the classic footgun: `dawdle run make test --json` must pass
        // --json to make, not to dawdle.
        let parsed = run_args(&["run", "make", "test", "--json", "--label", "x"]);
        assert_eq!(
            parsed.command,
            vec!["make", "test", "--json", "--label", "x"]
        );
        assert!(parsed.label.is_empty());
    }

    #[test]
    fn run_requires_something_to_execute() {
        let err = parse(args(&["run"])).unwrap_err().to_string();
        assert!(err.contains("nothing to run"), "got {err}");
    }

    #[test]
    fn run_rejects_an_unknown_flag_with_the_flag_named() {
        let err = parse(args(&["run", "--nope", "make"]))
            .unwrap_err()
            .to_string();
        assert!(err.contains("--nope"), "got {err}");
    }

    #[test]
    fn run_rejects_a_sub_millisecond_interval() {
        // 1ms polling would spend more time reading /proc than the build does.
        let err = parse(args(&["run", "--interval", "1", "make"]))
            .unwrap_err()
            .to_string();
        assert!(err.contains("at least 10ms"), "got {err}");
    }

    #[test]
    fn run_rejects_a_non_numeric_interval() {
        let err = parse(args(&["run", "--interval", "fast", "make"]))
            .unwrap_err()
            .to_string();
        assert!(err.contains("needs a number"), "got {err}");
    }

    #[test]
    fn parses_report_options() {
        let command = parse(args(&[
            "report",
            "--last",
            "50",
            "--metric",
            "busy",
            "--threshold",
            "10",
            "--min-ms",
            "100",
            "--markdown",
        ]))
        .unwrap();
        let Command::Report(parsed) = command else {
            panic!("expected report");
        };
        assert_eq!(parsed.last, 50);
        assert_eq!(parsed.metric, Metric::Busy);
        assert_eq!(parsed.thresholds.pct, 10.0);
        assert_eq!(parsed.thresholds.min_abs_ms, 100);
        assert!(parsed.markdown);
    }

    #[test]
    fn parses_report_filters_after_the_report_flags() {
        let command = parse(args(&[
            "report", "-n", "5", "--label", "ci", "--branch", "main",
        ]))
        .unwrap();
        let Command::Report(parsed) = command else {
            panic!("expected report");
        };
        assert_eq!(parsed.last, 5);
        assert_eq!(parsed.label.as_deref(), Some("ci"));
        assert_eq!(parsed.branch.as_deref(), Some("main"));
    }

    #[test]
    fn parses_windows_into_milliseconds() {
        assert_eq!(parse_window("30").unwrap(), 30 * 60_000, "bare = minutes");
        assert_eq!(parse_window("45m").unwrap(), 45 * 60_000);
        assert_eq!(parse_window("6h").unwrap(), 6 * 3_600_000);
        assert_eq!(parse_window("7d").unwrap(), 7 * 86_400_000);
        assert_eq!(parse_window("4w").unwrap(), 4 * 604_800_000);
    }

    #[test]
    fn rejects_nonsense_windows() {
        for bad in ["", "abc", "0", "0d", "-3d", "7x"] {
            assert!(
                parse_window(bad).is_err(),
                "{bad:?} should not parse as a window"
            );
        }
    }

    #[test]
    fn rejects_an_unknown_metric_with_the_valid_ones_listed() {
        let err = parse_metric("flame").unwrap_err().to_string();
        assert!(err.contains("cpu"), "got {err}");
        assert!(err.contains("busy"), "got {err}");
    }

    #[test]
    fn blame_requires_a_target() {
        let err = parse(args(&["blame"])).unwrap_err().to_string();
        assert!(err.contains("needs a step key"), "got {err}");
    }

    #[test]
    fn blame_takes_a_quoted_step_key() {
        let command = parse(args(&["blame", "cargo test --lib", "--last", "30"])).unwrap();
        let Command::Blame(parsed) = command else {
            panic!("expected blame");
        };
        assert_eq!(parsed.target, "cargo test --lib");
        assert_eq!(parsed.last, 30);
    }

    #[test]
    fn blame_rejects_a_flag_in_the_target_position() {
        let err = parse(args(&["blame", "--json"])).unwrap_err().to_string();
        assert!(err.contains("step key first"), "got {err}");
    }

    #[test]
    fn parses_show_and_validates_the_run_id() {
        let command = parse(args(&["show", "12", "--processes"])).unwrap();
        let Command::Show(parsed) = command else {
            panic!("expected show");
        };
        assert_eq!(parsed.run_id, 12);
        assert!(parsed.processes);

        let err = parse(args(&["show", "abc"])).unwrap_err().to_string();
        assert!(err.contains("must be a number"), "got {err}");
    }

    #[test]
    fn help_and_version_short_circuit() {
        assert!(matches!(
            parse(args(&["--help"])).unwrap(),
            Command::Help(_)
        ));
        assert!(matches!(parse(args(&[])).unwrap(), Command::Help(_)));
        assert!(matches!(parse(args(&["-V"])).unwrap(), Command::Version));
    }

    #[test]
    fn unknown_leading_flags_are_rejected_rather_than_run() {
        // Otherwise a typo silently profiles nothing and reports "no runs".
        let err = parse(args(&["--frobnicate"])).unwrap_err().to_string();
        assert!(err.contains("--frobnicate"), "got {err}");
    }

    #[test]
    fn report_flags_are_order_independent() {
        // The bug this guards: with the parser split into per-flag-group passes,
        // a flag belonging to a later group is treated as a stray argument when
        // it appears before one from an earlier group.
        let expected = ["report", "--threshold", "10", "--last", "50", "--markdown"];
        let shuffled = ["report", "--markdown", "--last", "50", "--threshold", "10"];
        for argv in [expected, shuffled] {
            let Command::Report(parsed) = parse(args(&argv)).unwrap() else {
                panic!("expected report for {argv:?}");
            };
            assert_eq!(parsed.last, 50, "{argv:?}");
            assert_eq!(parsed.thresholds.pct, 10.0, "{argv:?}");
            assert!(parsed.markdown, "{argv:?}");
        }
    }

    #[test]
    fn blame_flags_are_order_independent() {
        let Command::Blame(parsed) = parse(args(&[
            "blame", "cc", "--min-ms", "50", "--last", "9", "--json",
        ]))
        .unwrap() else {
            panic!("expected blame");
        };
        assert_eq!(parsed.last, 9);
        assert_eq!(parsed.thresholds.min_abs_ms, 50);
        assert!(parsed.json);
        assert_eq!(parsed.target, "cc");
    }

    #[test]
    fn report_rejects_a_negative_threshold() {
        let err = parse(args(&["report", "--threshold", "-5"]))
            .unwrap_err()
            .to_string();
        assert!(err.contains("cannot be negative"), "got {err}");
    }

    #[test]
    fn report_rejects_a_stray_positional() {
        let err = parse(args(&["report", "extra"])).unwrap_err().to_string();
        assert!(err.contains("unexpected argument"), "got {err}");
    }
}
