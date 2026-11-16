//! Interface translations. Strings are written in English in the code and
//! looked up by that text: `t("Settings")`, `tf("Resumed at {}", &[&time])`,
//! `tn("{} video", "{} videos", n)`. Each language is a table in its own
//! file; `tests::complete` fails if a language misses a string the code uses.
//!
//! Only the interface is translated. Titles, channel names and categories are
//! whatever YouTube sends (we ask for English data because the parsers read
//! it; ages like "3 hours ago" are re-worded here).

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt::Display;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::OnceLock;

mod de;
mod es;
mod fr;
mod it;
mod ja;
mod ko;
mod pl;
mod pt;
pub mod regions;
mod ru;
mod tr;
mod uk;
mod zh;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Lang {
    En,
    Ru,
    Uk,
    De,
    Fr,
    Es,
    Pt,
    It,
    Pl,
    Tr,
    Ja,
    Zh,
    Ko,
}

impl Lang {
    pub const ALL: [Lang; 13] = [
        Lang::En,
        Lang::Ru,
        Lang::Uk,
        Lang::De,
        Lang::Fr,
        Lang::Es,
        Lang::Pt,
        Lang::It,
        Lang::Pl,
        Lang::Tr,
        Lang::Ja,
        Lang::Zh,
        Lang::Ko,
    ];

    /// ISO 639-1 code (also what macOS's per-app language list uses).
    pub fn code(self) -> &'static str {
        match self {
            Lang::En => "en",
            Lang::Ru => "ru",
            Lang::Uk => "uk",
            Lang::De => "de",
            Lang::Fr => "fr",
            Lang::Es => "es",
            Lang::Pt => "pt",
            Lang::It => "it",
            Lang::Pl => "pl",
            Lang::Tr => "tr",
            Lang::Ja => "ja",
            Lang::Zh => "zh",
            Lang::Ko => "ko",
        }
    }

    /// In its own language, for the picker.
    pub fn native_name(self) -> &'static str {
        match self {
            Lang::En => "English",
            Lang::Ru => "Русский",
            Lang::Uk => "Українська",
            Lang::De => "Deutsch",
            Lang::Fr => "Français",
            Lang::Es => "Español",
            Lang::Pt => "Português",
            Lang::It => "Italiano",
            Lang::Pl => "Polski",
            Lang::Tr => "Türkçe",
            Lang::Ja => "日本語",
            Lang::Zh => "中文（简体）",
            Lang::Ko => "한국어",
        }
    }

    /// "ru-RU", "pt_BR", "zh-Hans-CN" → the language.
    pub fn from_locale(tag: &str) -> Option<Lang> {
        let primary = tag.split(['-', '_']).next()?.to_ascii_lowercase();
        Lang::ALL.into_iter().find(|l| l.code() == primary)
    }

    fn table(self) -> &'static [(&'static str, &'static str)] {
        match self {
            Lang::En => &[],
            Lang::Ru => ru::T,
            Lang::Uk => uk::T,
            Lang::De => de::T,
            Lang::Fr => fr::T,
            Lang::Es => es::T,
            Lang::Pt => pt::T,
            Lang::It => it::T,
            Lang::Pl => pl::T,
            Lang::Tr => tr::T,
            Lang::Ja => ja::T,
            Lang::Zh => zh::T,
            Lang::Ko => ko::T,
        }
    }

    fn index(self) -> usize {
        Lang::ALL.iter().position(|&l| l == self).unwrap_or(0)
    }
}

/// The first of the system's preferred languages that we have (macOS
/// includes the per-app choice from System Settings → Language & Region).
pub fn system() -> Lang {
    sys_locale::get_locales()
        .find_map(|l| Lang::from_locale(&l))
        .unwrap_or(Lang::En)
}

static CURRENT: AtomicU8 = AtomicU8::new(0);

/// `None`: follow the system.
/// (`YTMINI_LANG=ru` overrides both, for screenshots and tests.)
pub fn set(choice: Option<Lang>) {
    let env = std::env::var("YTMINI_LANG")
        .ok()
        .and_then(|c| Lang::from_locale(&c));
    let l = env.or(choice).unwrap_or_else(system);
    CURRENT.store(l.index() as u8, Ordering::Relaxed);
}

pub fn current() -> Lang {
    Lang::ALL[CURRENT.load(Ordering::Relaxed) as usize % Lang::ALL.len()]
}

type Map = HashMap<&'static str, &'static str>;

fn map(l: Lang) -> &'static Map {
    static MAPS: OnceLock<Vec<Map>> = OnceLock::new();
    &MAPS.get_or_init(|| {
        Lang::ALL
            .iter()
            .map(|l| l.table().iter().copied().collect())
            .collect()
    })[l.index()]
}

// ---- what we tell YouTube: interface language (hl) and region (gl) ----

static REGION: std::sync::RwLock<String> = std::sync::RwLock::new(String::new());

/// The system's region ("UA" from "uk-UA"/"en_UA"), else "US".
pub fn system_region() -> String {
    sys_locale::get_locales()
        .find_map(|l| {
            l.split(['-', '_'])
                .skip(1)
                .find(|p| p.len() == 2 && p.chars().all(|c| c.is_ascii_alphabetic()))
                .map(str::to_ascii_uppercase)
        })
        .unwrap_or_else(|| "US".into())
}

/// `None`: follow the system's region.
pub fn set_region(choice: Option<String>) {
    let r = choice.unwrap_or_else(system_region);
    *REGION.write().unwrap_or_else(|e| e.into_inner()) = r;
}

/// (hl, gl) for YouTube requests: ytmini's interface language and the
/// region setting. `YTMINI_HL` / `YTMINI_GL` override them (tests).
pub fn youtube_locale() -> (String, String) {
    let hl = std::env::var("YTMINI_HL").unwrap_or_else(|_| match current() {
        Lang::Zh => "zh-CN".into(),
        Lang::Pt => "pt-BR".into(),
        l => l.code().into(),
    });
    let gl = std::env::var("YTMINI_GL").ok().unwrap_or_else(|| {
        let r = REGION.read().unwrap_or_else(|e| e.into_inner()).clone();
        if r.is_empty() {
            system_region()
        } else {
            r
        }
    });
    (hl, gl)
}

/// Translate an interface string (English when there's no translation).
pub fn t(en: &'static str) -> &'static str {
    map(current()).get(en).copied().unwrap_or(en)
}

/// Translate and fill in `{}` placeholders in order (a translation may use
/// `{0}`, `{1}`… to reorder them).
pub fn tf(en: &'static str, args: &[&dyn Display]) -> String {
    fill(t(en), args)
}

fn fill(pattern: &str, args: &[&dyn Display]) -> String {
    let mut out = String::with_capacity(pattern.len() + 16);
    let mut next = 0;
    let mut rest = pattern;
    while let Some(i) = rest.find('{') {
        out.push_str(&rest[..i]);
        let after = &rest[i + 1..];
        let Some(j) = after.find('}') else {
            out.push_str(&rest[i..]);
            rest = "";
            break;
        };
        let inner = &after[..j];
        let idx = if inner.is_empty() {
            next += 1;
            Some(next - 1)
        } else {
            inner.parse::<usize>().ok()
        };
        match idx.and_then(|k| args.get(k)) {
            Some(a) => out.push_str(&a.to_string()),
            None => {
                out.push('{');
                out.push_str(inner);
                out.push('}');
            }
        }
        rest = &after[j + 1..];
    }
    out.push_str(rest);
    out
}

/// Which plural form `n` takes: an index into the translation's
/// `|`-separated forms (English: "one|other").
fn plural_index(l: Lang, n: u64) -> usize {
    match l {
        // one / few / many
        Lang::Ru | Lang::Uk => {
            let (m10, m100) = (n % 10, n % 100);
            if m10 == 1 && m100 != 11 {
                0
            } else if (2..=4).contains(&m10) && !(12..=14).contains(&m100) {
                1
            } else {
                2
            }
        }
        Lang::Pl => {
            let (m10, m100) = (n % 10, n % 100);
            if n == 1 {
                0
            } else if (2..=4).contains(&m10) && !(12..=14).contains(&m100) {
                1
            } else {
                2
            }
        }
        // no plural forms
        Lang::Ja | Lang::Zh | Lang::Ko => 0,
        Lang::Fr => usize::from(n > 1),
        _ => usize::from(n != 1),
    }
}

/// A counted phrase: `tn("{} video", "{} videos", n)`. The translation is
/// keyed by the plural English text and lists its forms separated by `|`.
pub fn tn(one: &'static str, other: &'static str, n: u64) -> String {
    let l = current();
    let forms: Vec<&str> = match map(l).get(other) {
        Some(tr) => tr.split('|').collect(),
        None => vec![one, other],
    };
    let i = if map(l).contains_key(other) {
        plural_index(l, n)
    } else {
        plural_index(Lang::En, n)
    };
    let f = forms.get(i).or(forms.last()).copied().unwrap_or(other);
    fill(f, &[&n])
}

/// "1.2M" in the current language's style (`1,2 млн`, `120万`…).
pub fn compact(n: u64) -> String {
    let n = n as f64;
    let (v, unit) = if n >= 1e9 {
        (n / 1e9, t("{}B"))
    } else if n >= 1e6 {
        (n / 1e6, t("{}M"))
    } else if n >= 1e3 {
        (n / 1e3, t("{}K"))
    } else {
        return format!("{n}");
    };
    let num = if v >= 100.0 || (v - v.round()).abs() < 0.05 {
        format!("{:.0}", v)
    } else {
        format!("{:.1}", v)
    };
    let num = if matches!(
        current(),
        Lang::Ru
            | Lang::Uk
            | Lang::De
            | Lang::Fr
            | Lang::Es
            | Lang::Pt
            | Lang::It
            | Lang::Pl
            | Lang::Tr
    ) {
        num.replace('.', ",")
    } else {
        num
    };
    fill(unit, &[&num])
}

/// YouTube's English "3 hours ago" / "Streamed 2 days ago" / "Premiered …"
/// in the current language; anything else is passed through.
pub fn age(en: &str) -> String {
    let (prefix, rest) = if let Some(r) = en.strip_prefix("Streamed ") {
        (Some(t("Streamed {}")), r)
    } else if let Some(r) = en.strip_prefix("Premiered ") {
        (Some(t("Premiered {}")), r)
    } else {
        (None, en)
    };
    let Some(core) = rest.strip_suffix(" ago") else {
        return en.to_string();
    };
    let mut it = core.split_whitespace();
    let (Some(n), Some(unit), None) = (it.next(), it.next(), it.next()) else {
        return en.to_string();
    };
    let Ok(n) = n.parse::<u64>() else {
        return en.to_string();
    };
    let s = match unit.trim_end_matches('s') {
        "second" => tn("{} second ago", "{} seconds ago", n),
        "minute" => tn("{} minute ago", "{} minutes ago", n),
        "hour" => tn("{} hour ago", "{} hours ago", n),
        "day" => tn("{} day ago", "{} days ago", n),
        "week" => tn("{} week ago", "{} weeks ago", n),
        "month" => tn("{} month ago", "{} months ago", n),
        "year" => tn("{} year ago", "{} years ago", n),
        _ => return en.to_string(),
    };
    match prefix {
        Some(p) => fill(p, &[&s]),
        None => s,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plural_rules() {
        assert_eq!(
            [1, 2, 5, 11, 21, 22, 25, 112].map(|n| plural_index(Lang::Ru, n)),
            [0, 1, 2, 2, 0, 1, 2, 2]
        );
        assert_eq!(
            [1, 2, 5, 22].map(|n| plural_index(Lang::Pl, n)),
            [0, 1, 2, 1]
        );
        assert_eq!([0, 1, 2].map(|n| plural_index(Lang::En, n)), [1, 0, 1]);
    }

    #[test]
    fn fill_placeholders() {
        assert_eq!(fill("{} of {}", &[&1, &2]), "1 of 2");
        assert_eq!(fill("{1} / {0}", &[&"a", &"b"]), "b / a");
    }

    #[test]
    fn locale_tags() {
        assert_eq!(Lang::from_locale("ru-RU"), Some(Lang::Ru));
        assert_eq!(Lang::from_locale("zh-Hans-CN"), Some(Lang::Zh));
        assert_eq!(Lang::from_locale("pt_BR"), Some(Lang::Pt));
        assert_eq!(Lang::from_locale("xx"), None);
    }

    /// Every string the code passes to t/tf/tn has a translation in every
    /// language, and translations keep their placeholders.
    #[test]
    fn complete() {
        let keys = used_keys();
        assert!(keys.len() > 50, "found only {} keys", keys.len());
        let mut missing = Vec::new();
        for l in Lang::ALL.into_iter().filter(|&l| l != Lang::En) {
            let m = map(l);
            for k in &keys {
                match m.get(k.as_str()) {
                    None => missing.push(format!("{}: {k}", l.code())),
                    Some(tr) => {
                        let holes = |s: &str| s.matches('{').count();
                        let ok = tr.split('|').all(|f| {
                            holes(f) == holes(k)
                                || (holes(k) == 1 && holes(f) == 0 && k.starts_with("{}"))
                        });
                        if !ok {
                            missing.push(format!(
                                "{}: placeholders differ in {k:?} → {tr:?}",
                                l.code()
                            ));
                        }
                    }
                }
            }
        }
        assert!(
            missing.is_empty(),
            "{} problems:\n{}",
            missing.len(),
            missing.join("\n")
        );
    }

    /// Writes every key (JSON list) to YTMINI_KEYS, for translators.
    #[test]
    #[ignore]
    fn dump_keys() {
        let path = std::env::var("YTMINI_KEYS").unwrap();
        std::fs::write(path, serde_json::to_string_pretty(&used_keys()).unwrap()).unwrap();
    }

    /// String literals passed to t( / tf( / tn( in src/**.rs.
    pub fn used_keys() -> Vec<String> {
        let mut keys = std::collections::BTreeSet::new();
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/src");
        let mut stack = vec![std::path::PathBuf::from(dir)];
        while let Some(d) = stack.pop() {
            for e in std::fs::read_dir(&d).unwrap().flatten() {
                let p = e.path();
                if p.is_dir() {
                    if !p.ends_with("i18n") {
                        stack.push(p);
                    }
                } else if p.extension().is_some_and(|x| x == "rs") {
                    let src = std::fs::read_to_string(&p).unwrap();
                    collect_keys(&src, &mut keys);
                }
            }
        }
        // used inside this module
        for k in [
            "{}B",
            "{}M",
            "{}K",
            "Streamed {}",
            "Premiered {}",
            "{} seconds ago",
            "{} minutes ago",
            "{} hours ago",
            "{} days ago",
            "{} weeks ago",
            "{} months ago",
            "{} years ago",
        ] {
            keys.insert(k.to_string());
        }
        keys.into_iter().collect()
    }

    fn collect_keys(src: &str, keys: &mut std::collections::BTreeSet<String>) {
        // `t(` / `tf(` / `tn(` followed (after any whitespace) by string literals
        for (call, nth) in [("t(", 0), ("tf(", 0), ("tn(", 1)] {
            let mut rest = src;
            while let Some(i) = rest.find(call) {
                let before = rest[..i].chars().last();
                let real = !before.is_some_and(|c| c.is_alphanumeric() || c == '_');
                let mut s = &rest[i + call.len()..];
                rest = s;
                if !real || !s.trim_start().starts_with('"') {
                    continue;
                }
                let mut lit = None;
                for k in 0..=nth {
                    let t = s.trim_start();
                    let t = if k > 0 {
                        t.strip_prefix(',').unwrap_or(t).trim_start()
                    } else {
                        t
                    };
                    let Some(body) = t.strip_prefix('"') else {
                        break;
                    };
                    let b = body.as_bytes();
                    let mut end = 0;
                    while end < b.len() && !(b[end] == b'"' && (end == 0 || b[end - 1] != b'\\')) {
                        end += 1;
                    }
                    lit = Some(body[..end].replace("\\\"", "\"").replace("\\n", "\n"));
                    s = &body[(end + 1).min(body.len())..];
                }
                if let Some(l) = lit {
                    keys.insert(l);
                }
            }
        }
    }
}
