//! Human-facing formatting helpers: durations, relative times, and colours.

use std::io::IsTerminal;
use std::time::{SystemTime, UNIX_EPOCH};

/// Whether we should emit ANSI colour. Honours the NO_COLOR convention and
/// turns colour off when stdout is not a terminal (so piped output stays clean).
pub fn color_enabled() -> bool {
    if std::env::var_os("NO_COLOR").is_some() {
        return false;
    }
    if std::env::var_os("DAWDLE_NO_COLOR").is_some() {
        return false;
    }
    if let Ok(force) = std::env::var("DAWDLE_FORCE_COLOR") {
        if force != "0" {
            return true;
        }
    }
    std::io::stdout().is_terminal()
}

pub fn paint(text: &str, code: &str) -> String {
    if color_enabled() {
        format!("\x1b[{code}m{text}\x1b[0m")
    } else {
        text.to_string()
    }
}

pub fn green(text: &str) -> String {
    paint(text, "32")
}
pub fn red(text: &str) -> String {
    paint(text, "31")
}
pub fn yellow(text: &str) -> String {
    paint(text, "33")
}
pub fn blue(text: &str) -> String {
    paint(text, "34")
}
pub fn dim(text: &str) -> String {
    paint(text, "2")
}
pub fn bold(text: &str) -> String {
    paint(text, "1")
}

/// Render a millisecond count the way a human reads a stopwatch.
///
/// Deliberately keeps three significant digits rather than scaling to two, because
/// the question this tool answers is always "how much bigger is this than it was",
/// and 1.23s reads better than "1s".
pub fn ms(value: i64) -> String {
    if value < 0 {
        return format!("-{}", ms(-value));
    }
    if value < 1000 {
        return format!("{value}ms");
    }
    if value < 10_000 {
        return format!("{:.2}s", value as f64 / 1000.0);
    }
    if value < 60_000 {
        return format!("{:.1}s", value as f64 / 1000.0);
    }
    let total_seconds = value / 1000;
    let minutes = total_seconds / 60;
    let seconds = total_seconds % 60;
    if minutes < 60 {
        return format!("{minutes}m {seconds:02}s");
    }
    let hours = minutes / 60;
    let minutes = minutes % 60;
    format!("{hours}h {minutes:02}m")
}

/// Right-pad a cell to `width` visible characters, ignoring ANSI escapes.
pub fn pad(text: &str, width: usize) -> String {
    let visible = strip_ansi(text);
    let len = visible.chars().count();
    if len >= width {
        return text.to_string();
    }
    let mut out = text.to_string();
    for _ in 0..(width - len) {
        out.push(' ');
    }
    out
}

/// Truncate to `width` visible characters, appending an ellipsis when cut.
pub fn ellipsis(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        return text.to_string();
    }
    if width <= 1 {
        return "…".to_string();
    }
    let kept: String = text.chars().take(width - 1).collect();
    format!("{kept}…")
}

pub fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            // Skip the escape sequence up to and including its terminator.
            for next in chars.by_ref() {
                if next.is_ascii_alphabetic() {
                    break;
                }
            }
            continue;
        }
        out.push(c);
    }
    out
}

/// Percentage change from `baseline` to `current`, or None when there is no
/// meaningful baseline to compare against.
pub fn pct_change(baseline: f64, current: f64) -> Option<f64> {
    if baseline <= 0.0 {
        return None;
    }
    Some((current - baseline) / baseline * 100.0)
}

pub fn signed_pct(pct: f64) -> String {
    if pct.is_finite() {
        format!("{pct:+.1}%")
    } else {
        "n/a".to_string()
    }
}

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// "3d ago", "5h ago", "just now" — coarse on purpose, since these labels are
/// for scanning a list of recent runs, not for precise audit logs.
pub fn ago(timestamp_ms: i64) -> String {
    let delta = now_ms() - timestamp_ms;
    if delta < 60_000 {
        return "just now".to_string();
    }
    let minutes = delta / 60_000;
    if minutes < 60 {
        return format!("{minutes}m ago");
    }
    let hours = minutes / 60;
    if hours < 48 {
        return format!("{hours}h ago");
    }
    let days = hours / 24;
    if days < 30 {
        return format!("{days}d ago");
    }
    format!("{}mo ago", days / 30)
}

/// Format unix millis as YYYY-MM-DD using Howard Hinnant's civil-from-days
/// algorithm. Avoids pulling a date-time crate for one line of output.
pub fn iso_date(timestamp_ms: i64) -> String {
    let total_seconds = timestamp_ms.div_euclid(1000);
    let days = total_seconds.div_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
}

/// Render bytes as a short human string.
/// Shorten a string to `limit` characters by eliding the middle.
///
/// Command lines are mostly a program and a few flags at each end, so cutting
/// the middle keeps both the tool and the important flags while staying inside
/// a terminal width.
pub fn truncate_middle(text: &str, limit: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= limit || limit < 5 {
        return text.to_string();
    }
    let keep = limit - 3;
    let head = keep / 2;
    let tail = keep - head;
    let mut out: String = chars[..head].iter().collect();
    out.push('…');
    out.extend(chars[chars.len() - tail..].iter());
    out
}

pub fn bytes(kb: i64) -> String {
    if kb <= 0 {
        return "-".to_string();
    }
    let mb = kb as f64 / 1024.0;
    if mb < 1024.0 {
        format!("{mb:.0}MB")
    } else {
        format!("{:.1}GB", mb / 1024.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_durations_at_readable_scales() {
        assert_eq!(ms(0), "0ms");
        assert_eq!(ms(999), "999ms");
        assert_eq!(ms(1_000), "1.00s");
        assert_eq!(ms(1_234), "1.23s");
        assert_eq!(ms(12_300), "12.3s");
        assert_eq!(ms(63_000), "1m 03s");
        assert_eq!(ms(3_600_000), "1h 00m");
    }

    #[test]
    fn signed_pct_always_carries_a_sign() {
        assert_eq!(signed_pct(12.34), "+12.3%");
        assert_eq!(signed_pct(-4.0), "-4.0%");
    }

    #[test]
    fn pct_change_refuses_to_divide_by_zero() {
        assert_eq!(pct_change(0.0, 10.0), None);
        assert_eq!(pct_change(100.0, 150.0), Some(50.0));
    }

    #[test]
    fn pad_ignores_ansi_escapes_when_measuring() {
        let coloured = "\x1b[31mabc\x1b[0m".to_string();
        assert_eq!(pad(&coloured, 6), format!("{coloured}   "));
        assert_eq!(pad(&coloured, 3), coloured, "never truncates");
        assert_eq!(strip_ansi(&coloured), "abc");
    }

    #[test]
    fn iso_date_matches_known_unix_days() {
        assert_eq!(iso_date(0), "1970-01-01");
        assert_eq!(iso_date(1_000_000_000_000), "2001-09-09");
        assert_eq!(iso_date(1_735_689_600_000), "2025-01-01");
        // Leap day, which is the case a hand-rolled civil-from-days gets wrong.
        assert_eq!(iso_date(1_709_164_800_000), "2024-02-29");
    }

    #[test]
    fn ago_uses_coarse_buckets() {
        let now = now_ms();
        assert_eq!(ago(now - 5_000), "just now");
        assert_eq!(ago(now - 5 * 60_000), "5m ago");
        assert_eq!(ago(now - 3 * 3_600_000), "3h ago");
        assert_eq!(ago(now - 3 * 86_400_000), "3d ago");
    }
}
