//! Chapters, read from the video description the way YouTube does it:
//! a list of timestamps starting at 0:00, at least three, in order.

#[derive(Clone, Debug, PartialEq)]
pub struct Chapter {
    pub start: f64,
    pub title: String,
}

/// "1:02:03" / "12:34" / "0:05" → seconds.
fn parse_ts(s: &str) -> Option<f64> {
    let parts: Vec<&str> = s.split(':').collect();
    if !(2..=3).contains(&parts.len()) || parts.iter().any(|p| p.is_empty() || p.len() > 2) {
        return None;
    }
    let n: Vec<u64> = parts
        .iter()
        .map(|p| p.parse().ok())
        .collect::<Option<_>>()?;
    let (h, m, sec) = match n[..] {
        [m, s] => (0, m, s),
        [h, m, s] => (h, m, s),
        _ => return None,
    };
    (sec < 60 && (parts.len() == 2 || m < 60)).then_some((h * 3600 + m * 60 + sec) as f64)
}

/// The first timestamp-looking token on a line, with its byte range.
fn find_ts(line: &str) -> Option<(f64, usize, usize)> {
    let b = line.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i].is_ascii_digit() && (i == 0 || !(b[i - 1].is_ascii_digit() || b[i - 1] == b':')) {
            let mut j = i;
            while j < b.len() && (b[j].is_ascii_digit() || b[j] == b':') {
                j += 1;
            }
            let tok = line[i..j].trim_end_matches(':');
            if let Some(t) = parse_ts(tok) {
                return Some((t, i, i + tok.len()));
            }
            i = j;
        } else {
            i += 1;
        }
    }
    None
}

const SEPARATORS: &[char] = &[
    ' ', '-', '–', '—', '|', ':', '•', '·', '(', ')', '[', ']', '.', '\t', '>', '»',
];

/// Chapters from a description; empty when it doesn't define any.
pub fn parse(description: &str, duration: Option<f64>) -> Vec<Chapter> {
    let mut out: Vec<Chapter> = Vec::new();
    for line in description.lines() {
        let Some((t, a, b)) = find_ts(line) else {
            continue;
        };
        let title = format!("{} {}", &line[..a], &line[b..]);
        let title = title.trim_matches(SEPARATORS).trim().to_string();
        if out.is_empty() {
            if t != 0.0 {
                continue; // the list has to start at 0:00
            }
        } else if t <= out.last().map_or(0.0, |c| c.start) {
            break; // out of order: a different list (e.g. "songs I mention")
        }
        if duration.is_some_and(|d| d > 0.0 && t >= d) {
            break;
        }
        out.push(Chapter {
            start: t,
            title: if title.is_empty() {
                format!("Chapter {}", out.len() + 1)
            } else {
                title
            },
        });
    }
    if out.len() < 3 {
        return Vec::new();
    }
    out
}

/// Index of the chapter containing `t`.
pub fn at(chapters: &[Chapter], t: f64) -> Option<usize> {
    chapters.iter().rposition(|c| c.start <= t + 0.01)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typical() {
        let d = "Great video\n\nChapters:\n0:00 Intro\n1:30 - Setup\n12:05 – The fix\n1:02:03 Outro\n\nLinks: https://x.y/1:2";
        let c = parse(d, Some(4000.0));
        assert_eq!(
            c.iter()
                .map(|c| (c.start, c.title.as_str()))
                .collect::<Vec<_>>(),
            vec![
                (0.0, "Intro"),
                (90.0, "Setup"),
                (725.0, "The fix"),
                (3723.0, "Outro")
            ]
        );
        assert_eq!(at(&c, 100.0), Some(1));
        assert_eq!(at(&c, 0.0), Some(0));
    }

    #[test]
    fn trailing_and_bracketed() {
        let d = "Intro (00:00)\nPart one [02:10]\nPart two — 5:00";
        let c = parse(d, None);
        assert_eq!(c.len(), 3);
        assert_eq!(c[1].title, "Part one");
        assert_eq!(c[2].start, 300.0);
    }

    #[test]
    fn rejects_non_lists() {
        assert!(parse("at 1:30 he says hi\nand 2:00 again", None).is_empty());
        assert!(parse("0:00 a\n0:30 b", None).is_empty()); // fewer than three
        assert!(parse("Recorded 12:30 2024\n0:00 a", None).is_empty());
    }
}
