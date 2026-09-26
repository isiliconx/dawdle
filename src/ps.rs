//! Reading the process table, cheaply, repeatedly.
//!
//! A profile is a sampling problem: poll every `interval`, note which processes
//! exist, and derive CPU from the deltas between polls. There is no portable
//! way to ask the OS "give me a stack trace for this pid", so this module
//! deliberately stays at the process level — see `README.md` for why that is
//! the right trade for build profiling, and for what the next step would be.

use std::collections::HashMap;
use std::process::Command;

/// One process as observed during a single poll.
#[derive(Debug, Clone)]
pub struct ProcInfo {
    pub ppid: i32,
    /// Cumulative CPU time consumed by this process since it started, in ms.
    pub cpu_ms: i64,
    /// Resident set size in kilobytes.
    pub rss_kb: i64,
    /// Raw command line, empty when the OS refuses to expose it.
    pub cmdline: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    Linux,
    MacOs,
    /// We can still run, we just cannot sample anything. Reported to the user
    /// rather than silently producing an empty profile.
    Unsupported,
}

pub fn platform() -> Platform {
    match std::env::consts::OS {
        "linux" => Platform::Linux,
        "macos" => Platform::MacOs,
        _ => Platform::Unsupported,
    }
}

pub fn platform_name() -> &'static str {
    match platform() {
        Platform::Linux => "linux",
        Platform::MacOs => "macos",
        Platform::Unsupported => "unsupported",
    }
}

/// A source of process snapshots. Abstracted so tests can feed synthetic
/// process tables and assert on the accounting without touching a real system.
pub trait ProcSource {
    /// Every visible process, keyed by pid. Returns an empty map on failure —
    /// a missed poll should degrade the profile, not abort the run.
    fn snapshot(&mut self) -> HashMap<i32, ProcInfo>;
    fn describe(&self) -> String;
}

// ---------------------------------------------------------------------------
// Linux: /proc
// ---------------------------------------------------------------------------

pub struct LinuxProcSource {
    /// USER_HZ. Linux reports CPU in clock ticks and does not always expose the
    /// tick rate in a stable place, so we measure it once against a known
    /// duration rather than guessing 100.
    ticks_per_ms: f64,
    /// /proc is mounted somewhere other than / in some sandboxes and containers.
    proc_root: String,
}

impl LinuxProcSource {
    pub fn new() -> Self {
        let proc_root = detect_proc_root();
        Self {
            ticks_per_ms: ticks_per_ms(),
            proc_root,
        }
    }

    fn read_dir(&self) -> Vec<String> {
        match std::fs::read_dir(&self.proc_root) {
            Ok(entries) => entries
                .filter_map(|entry| entry.ok())
                .map(|entry| entry.file_name().to_string_lossy().to_string())
                .filter(|name| name.chars().all(|c| c.is_ascii_digit()))
                .collect(),
            Err(_) => Vec::new(),
        }
    }

    /// Parse one `/proc/<pid>/stat` line.
    ///
    /// The comm field is parenthesised and may itself contain spaces and
    /// parentheses (`(Web Content)`, `sh (weird name)`), so the parser splits
    /// on the *last* `)` before reading the numeric tail, per proc(5).
    fn parse_stat(line: &str, ticks_per_ms: f64) -> Option<(i32, i32, i64, String)> {
        let close = line.rfind(')')?;
        let pid: i32 = line[..line.find(' ')?].parse().ok()?;
        let comm = line[line.find('(')? + 1..close].to_string();
        let rest: Vec<&str> = line[close + 1..].split_whitespace().collect();
        // rest[0] is state, rest[1] is ppid, rest[11] utime, rest[12] stime.
        let ppid: i32 = rest.get(1)?.parse().ok()?;
        let utime: i64 = rest.get(11)?.parse().ok()?;
        let stime: i64 = rest.get(12)?.parse().ok()?;
        Some((
            pid,
            ppid,
            ((utime + stime) as f64 / ticks_per_ms) as i64,
            comm,
        ))
    }

    fn rss_kb(&self, pid: i32) -> i64 {
        let statm = format!("{}/{pid}/statm", self.proc_root);
        let Ok(contents) = std::fs::read_to_string(&statm) else {
            return 0;
        };
        // statm field 2 is resident pages.
        contents
            .split_whitespace()
            .nth(1)
            .and_then(|pages| pages.parse::<i64>().ok())
            .map(|pages| pages * page_size_kb())
            .unwrap_or(0)
    }

    fn cmdline(&self, pid: i32) -> String {
        let path = format!("{}/{pid}/cmdline", self.proc_root);
        let Ok(bytes) = std::fs::read(&path) else {
            return String::new();
        };
        if bytes.is_empty() {
            // Kernel threads have an empty cmdline. A zombie reads as "(name)".
            return String::new();
        }
        let text = String::from_utf8_lossy(&bytes).to_string();
        // Arguments are NUL-separated; make them space-separated for display.
        text.split('\0')
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join(" ")
    }
}

impl Default for LinuxProcSource {
    fn default() -> Self {
        Self::new()
    }
}

impl ProcSource for LinuxProcSource {
    fn snapshot(&mut self) -> HashMap<i32, ProcInfo> {
        let mut out = HashMap::new();
        for name in self.read_dir() {
            let Ok(pid) = name.parse::<i32>() else {
                continue;
            };
            let stat_path = format!("{}/{pid}/stat", self.proc_root);
            let Ok(line) = std::fs::read_to_string(&stat_path) else {
                continue; // Process exited between listing and reading.
            };
            let Some((pid, ppid, cpu_ms, comm)) = Self::parse_stat(&line, self.ticks_per_ms) else {
                continue;
            };
            let cmdline = self.cmdline(pid);
            out.insert(
                pid,
                ProcInfo {
                    ppid,
                    cpu_ms,
                    rss_kb: self.rss_kb(pid),
                    // Prefer the real command line; fall back to comm for
                    // kernel threads and for processes we lack permission to read.
                    cmdline: if cmdline.is_empty() { comm } else { cmdline },
                },
            );
        }
        out
    }

    fn describe(&self) -> String {
        format!("linux /proc at {}", self.proc_root)
    }
}

fn detect_proc_root() -> String {
    if std::path::Path::new("/proc/self/stat").exists() {
        return "/proc".to_string();
    }
    for candidate in ["/host/proc", "/proc"] {
        if std::path::Path::new(&format!("{candidate}/self/stat")).exists() {
            return candidate.to_string();
        }
    }
    "/proc".to_string()
}

fn page_size_kb() -> i64 {
    // getconf PAGESIZE is 4096 on every platform we support; reading it from
    // /proc/self/smaps_rollup would be more correct but costs a file read per
    // process per poll, which is exactly what we are trying to avoid.
    4
}

// `_SC_CLK_TCK` in glibc. Declared directly rather than pulling in libc as a
// dependency: it is a single symbol we already link against.
extern "C" {
    fn sysconf(name: i32) -> i64;
}
const SC_CLK_TCK: i32 = 2;

/// How many `/proc` CPU ticks make up one millisecond.
///
/// Linux reports CPU in USER_HZ, a kernel build-time constant — 100 almost
/// everywhere, but not guaranteed. Ask the OS rather than guessing, because
/// getting it wrong scales every duration in the profile by the same factor and
/// the ratios driving regression detection survive it, which means the error
/// would be invisible until someone compared a dawdle number to `time`.
fn ticks_per_ms() -> f64 {
    // SAFETY: sysconf with a valid name is a pure query with no preconditions.
    let ticks_per_second = unsafe { sysconf(SC_CLK_TCK) };
    if ticks_per_second > 0 {
        return ticks_per_second as f64 / 1000.0;
    }
    0.1 // USER_HZ = 100.
}

// ---------------------------------------------------------------------------
// macOS: ps
// ---------------------------------------------------------------------------

// macOS `ps` reports wall-clock seconds, not CPU time, so the numbers on this
// platform are an approximation. For build profiling the distinction is small
// and the alternative is no profile at all; it is documented as a known
// limitation rather than papered over.
pub struct MacProcSource;

impl Default for MacProcSource {
    fn default() -> Self {
        Self
    }
}

impl ProcSource for MacProcSource {
    fn snapshot(&mut self) -> HashMap<i32, ProcInfo> {
        // One `-o` per field, deliberately. Apple's ps(1) warns that a
        // comma-separated list of `keyword=` headers "may be one column named
        // X,comm=Y or two columns" and says to use multiple -o options when in
        // doubt. With a single combined `-o`, BSD/macOS ps can emit the whole
        // spec as one literal column, every line then fails to parse, and rss
        // silently becomes 0. Separate options make each field its own column.
        let Ok(output) = Command::new("ps")
            .args([
                "-axo", "-o", "pid=", "-o", "ppid=", "-o", "rss=", "-o", "time=", "-o", "command=",
            ])
            .output()
        else {
            return HashMap::new();
        };
        let text = String::from_utf8_lossy(&output.stdout).to_string();
        text.lines().filter_map(parse_ps_line).collect()
    }

    fn describe(&self) -> String {
        "macos ps".to_string()
    }
}

/// Parse one line of `ps -axo -o pid= -o ppid= -o rss= -o time= -o command=`
/// output into a [`ProcInfo`], or `None` if the line is not a process row.
///
/// Split out from [`MacProcSource::snapshot`] so it can be tested against
/// captured output on any platform. The macOS reader originally got this wrong
/// in a way only a real macOS could reveal; a synthetic line can hold every
/// host to the same contract.
pub fn parse_ps_line(line: &str) -> Option<(i32, ProcInfo)> {
    // `split_whitespace`, not `splitn(.., char::is_whitespace)`. ps right-aligns
    // its numeric columns, so consecutive spaces between fields are normal, and
    // splitting on individual whitespace chars yields an *empty* field for each
    // of those runs. Every line then failed to parse and rss came back 0 for
    // every process on macOS.
    let fields: Vec<&str> = line.split_whitespace().collect();
    // pid, ppid, rss, time, then the command as whatever is left, joined back
    // together because it legitimately contains spaces.
    let (leading, rest) = fields.split_at(4.min(fields.len()));
    let [pid, ppid, rss, time, ..] = leading else {
        return None;
    };
    let pid: i32 = pid.parse().ok()?;
    let ppid = ppid.parse().ok()?;
    Some((
        pid,
        ProcInfo {
            ppid,
            cpu_ms: parse_ps_time_ms(time),
            rss_kb: rss.parse().unwrap_or(0),
            cmdline: rest.join(" "),
        },
    ))
}

/// Parse the `[[dd-]hh:]mm:ss` elapsed-time format that BSD `ps` emits.
pub fn parse_ps_time_ms(text: &str) -> i64 {
    let mut rest = text.trim();
    let mut days: i64 = 0;
    if let Some((day_part, remainder)) = rest.split_once('-') {
        days = day_part.parse().unwrap_or(0);
        rest = remainder;
    }
    // The clock part accumulates in its own variable. Folding the days into the
    // same accumulator looks harmless and is not: the loop multiplies by 60 on
    // every component, so "2-00:00:00" would come back as 2 days times 3600.
    // BSD `ps -o time=` prints fractional seconds ("0:00.05", "1:02.03"), so
    // the final component is not a plain integer. Parsing it with `parse::<i64>`
    // failed, and because the failure was swallowed into 0 the *whole seconds
    // value* was lost: "1:02.03" came back as 60 seconds, not 62. Split the
    // fraction off before converting.
    let (whole, fraction) = match rest.rsplit_once('.') {
        Some((whole, frac)) => (whole, frac),
        None => (rest, ""),
    };
    let mut seconds: i64 = 0;
    for part in whole.split(':') {
        seconds = seconds
            .saturating_mul(60)
            .saturating_add(part.parse().unwrap_or(0));
    }
    let total = days
        .saturating_mul(86_400)
        .saturating_add(seconds)
        .saturating_mul(1000);
    // Hundredths at most, so two digits is plenty; anything longer is clamped
    // rather than allowed to overflow the millisecond scale.
    let millis: i64 = fraction
        .chars()
        .take(3)
        .collect::<String>()
        .parse::<i64>()
        .unwrap_or(0);
    let millis = if fraction.len() >= 3 {
        millis
    } else {
        millis * 10i64.saturating_pow(3 - fraction.len() as u32)
    };
    total.saturating_add(millis.min(999))
}

/// Build the right source for the running platform.
pub fn default_source() -> Option<Box<dyn ProcSource>> {
    match platform() {
        Platform::Linux => Some(Box::new(LinuxProcSource::new())),
        Platform::MacOs => Some(Box::new(MacProcSource)),
        Platform::Unsupported => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn linux_stat_parser_handles_comm_with_spaces_and_parens() {
        // (Web Content) and sh (weird name) both appear in the wild; a parser
        // that splits on whitespace gets the ppid wrong and every process ends
        // up parented to garbage.
        let line = "1234 (Web Content (tab)) S 42 1234 1234 0 -1 4194304 100 0 0 0 250 175 0 0 20 0 5 0 900 123456789 0 0 20 0 3 0 100 0 0 0 0 0 0 0 0 0 0 0";
        let (pid, ppid, _, comm) = LinuxProcSource::parse_stat(line, 0.01).unwrap();
        assert_eq!(pid, 1234);
        assert_eq!(ppid, 42);
        assert_eq!(comm, "Web Content (tab)");
    }

    #[test]
    fn linux_stat_parser_converts_ticks_to_milliseconds() {
        // 150 ticks at USER_HZ=100 is 1.5 seconds, i.e. 0.1 ticks per ms.
        let line = "9 (bash) S 1 9 9 0 -1 4194304 50 0 0 0 100 50 0 0 20 0 1 0 500 0 0 0 0 0 0 0 0 0 0 0 0";
        let (_, _, cpu_ms, comm) = LinuxProcSource::parse_stat(line, 0.1).unwrap();
        assert_eq!(comm, "bash");
        assert_eq!(cpu_ms, 1500);
    }

    #[test]
    fn linux_stat_parser_rejects_garbage() {
        assert!(LinuxProcSource::parse_stat("", 0.01).is_none());
        assert!(LinuxProcSource::parse_stat("no parens here", 0.01).is_none());
    }

    #[test]
    fn ps_elapsed_time_formats_all_parse() {
        assert_eq!(parse_ps_time_ms("00:00:05"), 5_000);
        assert_eq!(parse_ps_time_ms("01:30"), 90_000);
        assert_eq!(parse_ps_time_ms("2-00:00:00"), 172_800_000);
        assert_eq!(parse_ps_time_ms("0:02"), 2_000);
    }

    #[test]
    fn a_ps_line_parses_pid_ppid_rss_time_and_a_multiword_command() {
        // Captured shape of `ps -axo -o pid= -o ppid= -o rss= -o time= -o command=`
        // on a real BSD-flavoured ps: right-aligned numeric columns, and a
        // command that contains spaces.
        let (pid, info) =
            parse_ps_line("  47395  47394   12345 0:00.05 python3 -m pytest -x tests/")
                .expect("a well-formed line parses");
        assert_eq!(pid, 47395);
        assert_eq!(info.ppid, 47394);
        assert_eq!(
            info.rss_kb, 12345,
            "rss is the third column, not the fourth"
        );
        assert_eq!(info.cpu_ms, 50);
        assert_eq!(info.cmdline, "python3 -m pytest -x tests/");
    }

    #[test]
    fn a_ps_line_does_not_confuse_rss_with_the_time_column() {
        // The macOS reader parsed the column order as pid, ppid, time, rss, so
        // rss picked up the time field and became 0 for every process. A test
        // with the columns the way ps actually prints them holds every host to
        // the same contract, not just macOS.
        let (_, info) = parse_ps_line("  100  1  999 0:00.00 sh build.sh").expect("parses");
        assert_eq!(info.rss_kb, 999, "the time field must not land in rss");
        assert_eq!(info.cpu_ms, 0);
    }

    #[test]
    fn a_ps_line_without_a_command_still_parses() {
        // A kernel thread can have an empty command; dropping the whole row
        // would lose a real process from the tree.
        let (_, info) = parse_ps_line("  7  2  512 1:02.03").expect("parses without a command");
        assert_eq!(info.ppid, 2);
        assert_eq!(info.rss_kb, 512);
        assert_eq!(info.cpu_ms, 62_030);
        assert_eq!(info.cmdline, "");
    }

    #[test]
    fn a_ps_header_or_garbage_line_is_rejected_rather_than_half_parsed() {
        assert!(parse_ps_line("  PID  PPID  RSS TIME COMMAND").is_none());
        assert!(parse_ps_line("").is_none());
        // What BSD ps emits when a comma-separated -o spec is treated as one
        // literal column, which is exactly how the bug presented.
        assert!(parse_ps_line("pid=,ppid=,rss=,time=,command=").is_none());
    }

    #[test]
    fn a_ps_line_keeps_multibyte_commands_intact() {
        let (_, info) = parse_ps_line("  5  1  64 0:00.01 ./build --name café").expect("parses");
        assert_eq!(info.cmdline, "./build --name café");
    }

    #[test]
    fn a_ps_line_keeps_the_fractional_seconds_bsd_ps_prints() {
        // BSD `ps -o time=` renders hundredths ("0:00.05"). Reading that as a
        // plain integer failed and the failure was swallowed to 0, taking the
        // whole seconds value with it: "1:02.03" came back as 60 seconds.
        assert_eq!(parse_ps_time_ms("0:00.05"), 50);
        assert_eq!(parse_ps_time_ms("1:02.03"), 62_030);
        assert_eq!(parse_ps_time_ms("0:00.5"), 500, "one digit is tenths");
        assert_eq!(parse_ps_time_ms("0:00.123"), 123, "millis are kept too");
        assert_eq!(
            parse_ps_time_ms("00:00:05"),
            5_000,
            "no fraction still works"
        );
        assert_eq!(
            parse_ps_time_ms("1-00:00:00.50"),
            86_400_500,
            "one day plus 0.5s: the day prefix and the hundredths both count"
        );
    }

    #[test]
    fn live_snapshot_sees_this_process() {
        // The one integration-shaped test in this module: if the real source
        // cannot see us, every profile is empty and nothing else matters.
        let Some(mut source) = default_source() else {
            assert_eq!(platform(), Platform::Unsupported);
            return;
        };
        let snapshot = source.snapshot();
        let me = std::process::id() as i32;
        assert!(
            snapshot.contains_key(&me),
            "snapshot did not contain our own pid {me} (source: {})",
            source.describe()
        );
        let info = &snapshot[&me];
        assert!(
            info.ppid > 0,
            "expected a real parent pid, got {}",
            info.ppid
        );
        assert!(
            info.rss_kb > 0,
            "expected non-zero rss, got {}",
            info.rss_kb
        );
    }
}
