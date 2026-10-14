//! Search query syntax: free words plus filters such as `ext:pdf`, `in:経理`,
//! `after:2026-01-01`, `size>10mb`, `type:folder` and date words like 先月.

use chrono::{Datelike, Duration, Local, NaiveDate, TimeZone};
use serde::{Deserialize, Serialize};

use crate::normalize::{normalize, variants};

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    #[default]
    Any,
    File,
    Folder,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Filter {
    /// Each term is a list of alternative spellings; every term must match.
    pub terms: Vec<Vec<String>>,
    /// Raw (unnormalized) words, used for full-text search.
    pub raw_terms: Vec<String>,
    pub exts: Vec<String>,
    pub path_contains: Vec<String>,
    pub after: Option<i64>,
    pub before: Option<i64>,
    pub min_size: Option<u64>,
    pub max_size: Option<u64>,
    pub kind: Kind,
}

impl Filter {
    pub fn has_terms(&self) -> bool {
        !self.terms.is_empty()
    }

    pub fn is_empty(&self) -> bool {
        self.terms.is_empty()
            && self.exts.is_empty()
            && self.path_contains.is_empty()
            && self.after.is_none()
            && self.before.is_none()
            && self.min_size.is_none()
            && self.max_size.is_none()
            && self.kind == Kind::Any
    }

    pub fn add_term(&mut self, word: &str) {
        let w = word.trim();
        if w.is_empty() {
            return;
        }
        self.raw_terms.push(w.to_string());
        self.terms.push(variants(w));
    }

    /// Checks the non-text filters against one entry.
    pub fn accepts_meta(&self, path_norm: &str, ext: &str, size: u64, mtime: i64, is_dir: bool) -> bool {
        match self.kind {
            Kind::File if is_dir => return false,
            Kind::Folder if !is_dir => return false,
            _ => {}
        }
        if !self.exts.is_empty() && (is_dir || !self.exts.iter().any(|e| e == ext)) {
            return false;
        }
        if self.after.is_some_and(|a| mtime < a) || self.before.is_some_and(|b| mtime >= b) {
            return false;
        }
        if self.min_size.is_some_and(|m| is_dir || size < m) || self.max_size.is_some_and(|m| is_dir || size > m) {
            return false;
        }
        self.path_contains.iter().all(|p| path_norm.contains(p.as_str()))
    }
}

/// File-type groups usable as `ext:` shortcuts and as Japanese words.
pub fn ext_group(word: &str) -> Option<&'static [&'static str]> {
    Some(match word {
        "画像" | "写真" | "image" | "images" | "img" => &["jpg", "jpeg", "png", "gif", "bmp", "webp", "heic", "tif", "tiff", "svg"],
        "動画" | "video" | "movie" => &["mp4", "mov", "avi", "mkv", "wmv", "webm", "m4v"],
        "音楽" | "音声" | "audio" | "music" => &["mp3", "wav", "m4a", "flac", "aac", "ogg", "wma"],
        "文書" | "ワード" | "word" | "doc" => &["doc", "docx", "odt", "rtf"],
        "エクセル" | "表計算" | "excel" | "xls" => &["xls", "xlsx", "xlsm", "csv", "ods"],
        "パワポ" | "パワーポイント" | "スライド" | "ppt" | "powerpoint" => &["ppt", "pptx", "odp"],
        "pdf" => &["pdf"],
        "圧縮" | "zip" | "archive" => &["zip", "7z", "rar", "lzh", "tar", "gz"],
        "テキスト" | "text" | "txt" => &["txt", "md", "log"],
        _ => return None,
    })
}

fn local_ts(d: NaiveDate) -> i64 {
    Local
        .from_local_datetime(&d.and_hms_opt(0, 0, 0).unwrap())
        .earliest()
        .map(|t| t.timestamp())
        .unwrap_or(0)
}

fn month_start(y: i32, m: u32) -> NaiveDate {
    NaiveDate::from_ymd_opt(y, m, 1).unwrap()
}

/// Range [start, end) for a Japanese/English date word, relative to `today`.
pub fn date_word(word: &str, today: NaiveDate) -> Option<(i64, i64)> {
    let wd = today.weekday().num_days_from_monday() as i64;
    let week_start = today - Duration::days(wd);
    let (y, m) = (today.year(), today.month());
    let (s, e) = match word {
        "今日" | "today" => (today, today + Duration::days(1)),
        "昨日" | "yesterday" => (today - Duration::days(1), today),
        "一昨日" | "おととい" => (today - Duration::days(2), today - Duration::days(1)),
        "今週" | "thisweek" => (week_start, today + Duration::days(1)),
        "先週" | "lastweek" => (week_start - Duration::days(7), week_start),
        "今月" | "thismonth" => (month_start(y, m), today + Duration::days(1)),
        "先月" | "lastmonth" => {
            let (py, pm) = if m == 1 { (y - 1, 12) } else { (y, m - 1) };
            (month_start(py, pm), month_start(y, m))
        }
        "今年" | "thisyear" => (month_start(y, 1), today + Duration::days(1)),
        "去年" | "昨年" | "lastyear" => (month_start(y - 1, 1), month_start(y, 1)),
        "最近" | "recent" => (today - Duration::days(14), today + Duration::days(1)),
        _ => return None,
    };
    Some((local_ts(s), local_ts(e)))
}

fn parse_date(s: &str) -> Option<NaiveDate> {
    let s = s.replace(['/', '.'], "-");
    NaiveDate::parse_from_str(&s, "%Y-%m-%d")
        .ok()
        .or_else(|| NaiveDate::parse_from_str(&format!("{s}-01"), "%Y-%m-%d").ok())
        .or_else(|| NaiveDate::parse_from_str(&format!("{s}-01-01"), "%Y-%m-%d").ok())
}

pub fn parse_size(s: &str) -> Option<u64> {
    let s = s.trim().to_ascii_lowercase();
    let idx = s.find(|c: char| !(c.is_ascii_digit() || c == '.')).unwrap_or(s.len());
    let (num, unit) = s.split_at(idx);
    let n: f64 = num.parse().ok()?;
    let mul = match unit.trim() {
        "" | "b" => 1.0,
        "k" | "kb" => 1024.0,
        "m" | "mb" => 1024.0 * 1024.0,
        "g" | "gb" => 1024.0 * 1024.0 * 1024.0,
        _ => return None,
    };
    Some((n * mul) as u64)
}

pub fn parse(input: &str) -> Filter {
    parse_at(input, Local::now().date_naive())
}

pub fn parse_at(input: &str, today: NaiveDate) -> Filter {
    let mut f = Filter::default();
    // Full-width spaces and colons are common when typing with an IME.
    let input = input.replace('\u{3000}', " ").replace('：', ":");
    for tok in input.split_whitespace() {
        let lower = tok.to_lowercase();
        if let Some(v) = lower.strip_prefix("ext:").or_else(|| lower.strip_prefix("type:").filter(|v| ext_group(v).is_some())) {
            for e in v.split(',').filter(|e| !e.is_empty()) {
                let e = e.trim_start_matches('.');
                match ext_group(e) {
                    Some(g) => f.exts.extend(g.iter().map(|s| s.to_string())),
                    None => f.exts.push(e.to_string()),
                }
            }
        } else if let Some(v) = lower.strip_prefix("type:").or_else(|| lower.strip_prefix("kind:")) {
            f.kind = match v {
                "folder" | "dir" | "フォルダ" => Kind::Folder,
                "file" | "ファイル" => Kind::File,
                _ => Kind::Any,
            };
        } else if let Some(v) = tok.strip_prefix("in:").or_else(|| tok.strip_prefix("path:")) {
            if !v.is_empty() {
                f.path_contains.push(normalize(v));
            }
        } else if let Some(v) = lower.strip_prefix("after:") {
            f.after = parse_date(v).map(local_ts);
        } else if let Some(v) = lower.strip_prefix("before:") {
            f.before = parse_date(v).map(local_ts);
        } else if let Some(v) = lower.strip_prefix("size>") {
            f.min_size = parse_size(v);
        } else if let Some(v) = lower.strip_prefix("size<") {
            f.max_size = parse_size(v);
        } else if let Some((s, e)) = date_word(tok, today) {
            f.after = Some(s);
            f.before = Some(e);
        } else if tok == "フォルダ" {
            f.kind = Kind::Folder;
        } else if let Some(g) = ext_group(tok).filter(|_| !tok.is_ascii()) {
            f.exts.extend(g.iter().map(|s| s.to_string()));
        } else {
            f.add_term(tok);
        }
    }
    f.exts.sort();
    f.exts.dedup();
    f
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(s: &str) -> NaiveDate {
        NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap()
    }

    #[test]
    fn filters() {
        let f = parse_at("見積 ext:pdf,xlsx in:経理 size>1mb", d("2026-10-14"));
        assert_eq!(f.raw_terms, vec!["見積"]);
        assert_eq!(f.exts, vec!["pdf", "xlsx"]);
        assert_eq!(f.path_contains, vec!["経理"]);
        assert_eq!(f.min_size, Some(1024 * 1024));
    }

    #[test]
    fn japanese_words() {
        let f = parse_at("先月 請求書 画像", d("2026-10-14"));
        assert_eq!(f.raw_terms, vec!["請求書"]);
        assert!(f.exts.contains(&"png".to_string()));
        let (s, e) = date_word("先月", d("2026-01-10")).unwrap();
        assert_eq!(s, local_ts(d("2025-12-01")));
        assert_eq!(e, local_ts(d("2026-01-01")));
    }

    #[test]
    fn fullwidth_space_and_colon() {
        let f = parse_at("報告書\u{3000}ext：docx", d("2026-10-14"));
        assert_eq!(f.raw_terms, vec!["報告書"]);
        assert_eq!(f.exts, vec!["docx"]);
    }

    #[test]
    fn meta() {
        let f = parse_at("type:folder", d("2026-10-14"));
        assert!(f.accepts_meta("c:/a", "", 0, 0, true));
        assert!(!f.accepts_meta("c:/a.txt", "txt", 0, 0, false));
        assert_eq!(parse_size("1.5kb"), Some(1536));
    }
}
