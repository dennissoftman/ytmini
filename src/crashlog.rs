//! Crash logs: a panic writes `crash-YYYY-MM-DD_HH-MM-SS.log` (local time)
//! into the `logs` folder next to the settings, with the message, where it
//! happened and a backtrace. URLs are cut out (stream URLs carry tokens);
//! cookies are never in panic messages. Settings → Storage opens the folder.

use std::path::PathBuf;
use std::sync::OnceLock;

/// The local UTC offset in seconds, read once at startup (`date +%z`).
static OFFSET: OnceLock<i64> = OnceLock::new();

pub fn dir() -> Option<PathBuf> {
    let d = crate::yt::config_dir().ok()?.join("logs");
    std::fs::create_dir_all(&d).ok()?;
    Some(d)
}

/// How many crash logs there are.
pub fn count() -> usize {
    dir()
        .and_then(|d| std::fs::read_dir(d).ok())
        .map_or(0, |rd| {
            rd.flatten()
                .filter(|e| e.file_name().to_string_lossy().starts_with("crash-"))
                .count()
        })
}

/// Call first thing in `main`.
pub fn install() {
    OFFSET.get_or_init(local_offset);
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = write(info);
        default(info);
    }));
}

fn write(info: &std::panic::PanicHookInfo<'_>) -> std::io::Result<()> {
    let d = dir().ok_or_else(|| std::io::Error::other("no logs dir"))?;
    let (date, time, zone) = now();
    let msg = info
        .payload()
        .downcast_ref::<&str>()
        .map(|s| s.to_string())
        .or_else(|| info.payload().downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "(no message)".into());
    let at = info
        .location()
        .map_or_else(String::new, |l| format!("{}:{}", l.file(), l.line()));
    let thread = std::thread::current();
    let body = format!(
        "ytmini {} crashed\nTime: {date} {time} {zone}\nOS: {} {}\nThread: {}\nWhere: {at}\nMessage: {}\n\nBacktrace:\n{}\n",
        env!("CARGO_PKG_VERSION"),
        std::env::consts::OS,
        std::env::consts::ARCH,
        thread.name().unwrap_or("unnamed"),
        redact(&msg),
        redact(&std::backtrace::Backtrace::force_capture().to_string()),
    );
    let name = format!("crash-{date}_{}.log", time.replace(':', "-"));
    std::fs::write(d.join(name), body)
}

/// Drop anything that looks like a URL (tokens, signed stream links),
/// wherever it starts: `url=https://…`, `"https://…"`, `(https://…)`.
pub fn redact(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    loop {
        let lower = rest.to_ascii_lowercase();
        let at = [lower.find("https://"), lower.find("http://")]
            .into_iter()
            .flatten()
            .min();
        let Some(at) = at else {
            out.push_str(rest);
            return out;
        };
        out.push_str(&rest[..at]);
        out.push_str("<url>");
        let tail = &rest[at..];
        let stop = tail
            .find(|c: char| {
                c.is_whitespace() || matches!(c, '"' | '\'' | ')' | '>' | ']' | '}' | ',')
            })
            .unwrap_or(tail.len());
        rest = &tail[stop..];
    }
}

fn local_offset() -> i64 {
    // Windows has no `date +%z` (a Git-for-Windows one would flash a console)
    if cfg!(windows) {
        return 0;
    }
    std::process::Command::new("date")
        .arg("+%z")
        .output()
        .ok()
        .and_then(|o| parse_offset(String::from_utf8_lossy(&o.stdout).trim()))
        .unwrap_or(0)
}

/// "+0200" → 7200
fn parse_offset(s: &str) -> Option<i64> {
    let (sign, rest) = match s.as_bytes().first()? {
        b'+' => (1, &s[1..]),
        b'-' => (-1, &s[1..]),
        _ => return None,
    };
    if rest.len() != 4 {
        return None;
    }
    let h: i64 = rest[..2].parse().ok()?;
    let m: i64 = rest[2..].parse().ok()?;
    Some(sign * (h * 3600 + m * 60))
}

/// ("2026-11-17", "14:03:09", "+02:00") in local time.
pub fn now() -> (String, String, String) {
    let off = *OFFSET.get().unwrap_or(&0);
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
        + off;
    let (y, mo, d) = civil(secs.div_euclid(86_400));
    let t = secs.rem_euclid(86_400);
    let zone = format!(
        "{}{:02}:{:02}",
        if off < 0 { '-' } else { '+' },
        off.abs() / 3600,
        off.abs() % 3600 / 60
    );
    (
        format!("{y:04}-{mo:02}-{d:02}"),
        format!("{:02}:{:02}:{:02}", t / 3600, t % 3600 / 60, t % 60),
        zone,
    )
}

/// Days since 1970-01-01 → (year, month, day) (Howard Hinnant's algorithm).
fn civil(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

#[cfg(test)]
mod tests {
    #[test]
    fn dates_and_offsets() {
        assert_eq!(super::civil(0), (1970, 1, 1));
        assert_eq!(super::civil(20_774), (2026, 11, 17));
        assert_eq!(super::parse_offset("+0200"), Some(7200));
        assert_eq!(super::parse_offset("-0530"), Some(-19_800));
        assert_eq!(
            super::redact("bad https://rr1.googlevideo.com/x?sig=1 here"),
            "bad <url> here"
        );
        assert_eq!(
            super::redact(r#"Err("https://x/v?sig=1"), url=http://a/b (HTTPS://C/d)"#),
            r#"Err("<url>"), url=<url> (<url>)"#
        );
    }
}
