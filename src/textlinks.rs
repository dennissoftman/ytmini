//! Splits description / comment text into plain text, timestamps
//! ("4:36", "1:02:03") and links, so they can be clicked.

#[derive(Debug, PartialEq)]
pub enum Seg<'a> {
    Text(&'a str),
    /// The written timestamp and its position in seconds.
    Time(&'a str, f64),
    Url(&'a str),
}

/// "4:36" → 276, "1:02:03" → 3723. Minutes/seconds must be two digits
/// after the first part; no more than three parts.
pub fn parse_time(s: &str) -> Option<f64> {
    let parts: Vec<&str> = s.split(':').collect();
    if !(2..=3).contains(&parts.len())
        || parts
            .iter()
            .any(|p| p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit()))
    {
        return None;
    }
    if parts[0].len() > 3 || parts[1..].iter().any(|p| p.len() != 2) {
        return None;
    }
    let n: Vec<f64> = parts.iter().map(|p| p.parse().unwrap_or(0.0)).collect();
    if n[1..].iter().any(|&x| x >= 60.0) {
        return None;
    }
    Some(n.iter().fold(0.0, |acc, x| acc * 60.0 + x))
}

/// One line of text into segments (whitespace stays in the text parts).
pub fn segments(line: &str) -> Vec<Seg<'_>> {
    let mut out = Vec::new();
    let mut text_start = 0;
    let mut i = 0;
    let bytes = line.as_bytes();
    while i < line.len() {
        // start of a word?
        if !bytes[i].is_ascii_whitespace()
            && (i == 0 || bytes[i - 1].is_ascii_whitespace() || b"([\"'".contains(&bytes[i - 1]))
        {
            // opening punctuation stays text: "(4:36)"
            if b"([\"'".contains(&bytes[i]) {
                i += 1;
                continue;
            }
            let end = line[i..]
                .find(char::is_whitespace)
                .map_or(line.len(), |e| i + e);
            let word = &line[i..end];
            // trailing punctuation isn't part of a timestamp / link
            let core = word.trim_end_matches(|c: char| ",.;:!?)]»\"'".contains(c));
            let core_end = i + core.len();
            let hit = if core.starts_with("http://") || core.starts_with("https://") {
                Some(Seg::Url(core))
            } else {
                parse_time(core).map(|t| Seg::Time(core, t))
            };
            if let Some(seg) = hit {
                if text_start < i {
                    out.push(Seg::Text(&line[text_start..i]));
                }
                out.push(seg);
                text_start = core_end;
                i = core_end;
                continue;
            }
            i = end;
            continue;
        }
        i += line[i..].chars().next().map_or(1, char::len_utf8);
    }
    if text_start < line.len() {
        out.push(Seg::Text(&line[text_start..]));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn times() {
        assert_eq!(parse_time("04:36"), Some(276.0));
        assert_eq!(parse_time("1:02:03"), Some(3723.0));
        assert_eq!(parse_time("41:08"), Some(2468.0));
        assert_eq!(parse_time("4:6"), None);
        assert_eq!(parse_time("12:75"), None);
        assert_eq!(parse_time("2024"), None);
        assert_eq!(parse_time("1:2:3:4"), None);
    }

    #[test]
    fn split() {
        assert_eq!(
            segments("04:36 Как Китай и СССР стали врагами"),
            vec![
                Seg::Time("04:36", 276.0),
                Seg::Text(" Как Китай и СССР стали врагами")
            ]
        );
        assert_eq!(
            segments("see (1:02:03), and https://example.com/x."),
            vec![
                Seg::Text("see ("),
                Seg::Time("1:02:03", 3723.0),
                Seg::Text("), and "),
                Seg::Url("https://example.com/x"),
                Seg::Text("."),
            ]
        );
        assert_eq!(segments("plain text"), vec![Seg::Text("plain text")]);
    }
}
