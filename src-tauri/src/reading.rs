//! Hiragana readings of Japanese text via the Microsoft IME (MS-IME).

use std::collections::HashMap;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;

use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, CLSIDFromProgID, COINIT_APARTMENTTHREADED, CoCreateInstance,
    CoInitializeEx, CoUninitialize,
};
use windows::Win32::UI::Input::Ime::IFELanguage;
use windows::core::{BSTR, IUnknown, w};

/// Longest input (in chars) that is sent to the IME; longer ones yield None.
const MAX_CHARS: usize = 200;
/// Readings cached on the worker; the cache is dropped when it grows past this.
const CACHE_LIMIT: usize = 100_000;

type Request = (Vec<String>, Sender<Vec<Option<String>>>);

/// Converts names to hiragana readings with the Windows Japanese IME.
/// Owns a dedicated background thread (COM objects are apartment-bound), so it is Send + Sync and cheap to call from any thread.
/// (Several IME instances do not run in parallel, so one worker is all that helps.)
pub struct Readings {
    tx: Sender<Request>,
}

impl Readings {
    /// Starts the worker. Returns None when MS-IME is not available (non-Japanese Windows, IME removed).
    pub fn new() -> Option<Self> {
        let (tx, rx) = mpsc::channel::<Request>();
        let (ready_tx, ready_rx) = mpsc::channel::<bool>();
        thread::Builder::new()
            .name("ms-ime-reading".into())
            .spawn(move || worker(rx, ready_tx))
            .ok()?;
        if ready_rx.recv().ok()? {
            Some(Self { tx })
        } else {
            None
        }
    }

    /// Reading of `text` in hiragana, or None when the IME fails. Only call it for text that contains kanji (see has_kanji).
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn reading(&self, text: &str) -> Option<String> {
        self.readings(&[text.to_owned()]).pop().flatten()
    }

    /// Batch version: same order as input.
    pub fn readings(&self, texts: &[String]) -> Vec<Option<String>> {
        // Split each text at ASCII punctuation; identical IME units are converted once per batch.
        let mut ids: HashMap<String, usize> = HashMap::new();
        let mut units: Vec<String> = Vec::new();
        let mut intern = |s: &str| -> usize {
            if let Some(&i) = ids.get(s) {
                return i;
            }
            let i = units.len();
            units.push(s.to_owned());
            ids.insert(s.to_owned(), i);
            i
        };
        let plans: Vec<Option<Vec<Seg>>> = texts
            .iter()
            .map(|t| {
                if t.is_empty() || t.chars().count() > MAX_CHARS {
                    return None;
                }
                Some(
                    segments(t)
                        .into_iter()
                        .map(|(is_punct, s)| {
                            if is_punct || s.is_ascii() {
                                Seg::Lit(ime_ascii(s))
                            } else {
                                Seg::Ime(intern(s))
                            }
                        })
                        .collect(),
                )
            })
            .collect();

        let n = units.len();
        let (reply_tx, reply_rx) = mpsc::channel();
        let results: Vec<Option<String>> = if self.tx.send((units, reply_tx)).is_err() {
            vec![None; n]
        } else {
            match reply_rx.recv() {
                Ok(out) if out.len() == n => out,
                _ => vec![None; n],
            }
        };

        plans
            .into_iter()
            .map(|plan| {
                let mut out = String::new();
                for seg in plan? {
                    match seg {
                        Seg::Ime(i) => out.push_str(results[i].as_deref()?),
                        Seg::Lit(s) => out.push_str(&s),
                    }
                }
                Some(out)
            })
            .collect()
    }
}

enum Seg {
    Ime(usize),
    Lit(String),
}

fn is_kanji_char(c: char) -> bool {
    matches!(c, '\u{4E00}'..='\u{9FFF}' | '\u{3400}'..='\u{4DBF}' | '々')
}

/// Splits at ASCII punctuation and spaces (flag true). Words keep their ASCII letters and digits
/// so the IME still sees the context around numerals and counters.
fn segments(s: &str) -> Vec<(bool, &str)> {
    let mut out: Vec<(bool, &str)> = Vec::new();
    let mut start = 0;
    for (i, c) in s.char_indices() {
        if c.is_ascii() && !c.is_ascii_alphanumeric() {
            if start < i {
                out.push((false, &s[start..i]));
            }
            out.push((true, &s[i..i + 1]));
            start = i + 1;
        }
    }
    if start < s.len() {
        out.push((false, &s[start..]));
    }
    out
}

/// What the IME returns for ASCII text: full-width forms, with a few symbols read out.
fn ime_ascii(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3);
    for c in s.chars() {
        match c {
            ' ' => out.push('\u{3000}'),
            '#' => out.push_str("しゃーぷ"),
            '$' => out.push_str("どる"),
            '@' => out.push_str("たんか"),
            '"' => out.push('”'),
            '\'' => out.push('’'),
            '`' => out.push('‘'),
            '\\' => out.push('￥'),
            '!'..='~' => out.push(char::from_u32(c as u32 + 0xFEE0).unwrap_or(c)),
            _ => out.push(c),
        }
    }
    out
}

/// True when the string contains at least one CJK ideograph (U+4E00..U+9FFF, U+3400..U+4DBF, 々).
pub fn has_kanji(s: &str) -> bool {
    s.chars().any(is_kanji_char)
}

/// Worker thread: owns COM initialization and the IME object.
fn worker(rx: Receiver<Request>, ready: Sender<bool>) {
    // S_OK and S_FALSE both succeed and must be balanced by CoUninitialize.
    // SAFETY: plain COM initialization on this thread.
    let hr = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
    if hr.is_err() {
        let _ = ready.send(false);
        return;
    }
    let _com = ComGuard;

    let ime = Ime::open();
    let _ = ready.send(ime.is_some());
    let Some(ime) = ime else { return };

    let mut cache: HashMap<String, Option<String>> = HashMap::new();
    while let Ok((texts, reply)) = rx.recv() {
        if cache.len() > CACHE_LIMIT {
            cache.clear();
        }
        let out = texts
            .into_iter()
            .map(|t| match cache.get(&t) {
                Some(hit) => hit.clone(),
                None => {
                    let r = ime.reading(&t);
                    cache.insert(t, r.clone());
                    r
                }
            })
            .collect();
        let _ = reply.send(out);
    }
    // `ime` drops before `_com` (reverse declaration order), so Close runs before CoUninitialize.
}

struct ComGuard;

impl Drop for ComGuard {
    fn drop(&mut self) {
        // SAFETY: matches the successful CoInitializeEx on this thread.
        unsafe { CoUninitialize() };
    }
}

struct Ime {
    obj: IFELanguage,
}

impl Ime {
    fn open() -> Option<Self> {
        // SAFETY: COM calls on an initialized apartment thread; results are checked.
        unsafe {
            let clsid = CLSIDFromProgID(w!("MSIME.Japan")).ok()?;
            let obj: IFELanguage =
                CoCreateInstance(&clsid, None::<&IUnknown>, CLSCTX_INPROC_SERVER).ok()?;
            obj.Open().ok()?;
            Some(Self { obj })
        }
    }

    fn reading(&self, text: &str) -> Option<String> {
        if text.is_empty() || text.chars().count() > MAX_CHARS {
            return None;
        }
        let input = BSTR::from(text);
        let mut phonetic = BSTR::new();
        // SAFETY: `input` and `phonetic` are valid BSTRs; start is 1-based, -1 means "to end".
        unsafe {
            self.obj.GetPhonetic(&input, 1, -1, &mut phonetic).ok()?;
        }
        let raw = phonetic.to_string();
        if raw.is_empty() {
            return None;
        }
        Some(katakana_to_hiragana(&raw))
    }
}

impl Drop for Ime {
    fn drop(&mut self) {
        // SAFETY: Open succeeded on this object; Close is its counterpart.
        unsafe {
            let _ = self.obj.Close();
        }
    }
}

/// Maps katakana (ァ..ヶ, ヽ, ヾ) to hiragana by shifting code points.
fn katakana_to_hiragana(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '\u{30A1}'..='\u{30F6}' | '\u{30FD}'..='\u{30FE}' => {
                char::from_u32(c as u32 - 0x60).unwrap_or(c)
            }
            _ => c,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn has_kanji_cases() {
        assert!(has_kanji("経理"));
        assert!(has_kanji("経理_2026年度"));
        assert!(has_kanji("東京都"));
        assert!(has_kanji("々"));
        assert!(has_kanji("\u{3400}"));
        assert!(has_kanji("\u{4DBF}"));
        assert!(!has_kanji(""));
        assert!(!has_kanji("けいり"));
        assert!(!has_kanji("ケイリ"));
        assert!(!has_kanji("abc_2026-01.pdf"));
        assert!(!has_kanji("\u{3099}"));
        assert!(!has_kanji("\u{4DFF}"));
    }

    #[test]
    fn katakana_mapping() {
        assert_eq!(katakana_to_hiragana("ケイリ"), "けいり");
        assert_eq!(katakana_to_hiragana("ヴァ"), "ゔぁ");
        assert_eq!(katakana_to_hiragana("ヶ"), "ゖ");
        assert_eq!(katakana_to_hiragana("ー"), "ー");
        assert_eq!(katakana_to_hiragana("abc123"), "abc123");
    }

    #[test]
    fn ime_readings() {
        let Some(r) = Readings::new() else {
            println!("MS-IME not available; skipping");
            return;
        };
        assert_eq!(r.reading("経理").as_deref(), Some("けいり"));
        assert!(r.reading("見積書").is_some_and(|s| s.contains("みつもり")));
        assert_eq!(r.reading("東京都").as_deref(), Some("とうきょうと"));

        let inputs: Vec<String> = ["経理", "東京都", "見積書", "abc", "経理_2026年度"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let batch = r.readings(&inputs);
        assert_eq!(batch.len(), inputs.len());
        assert_eq!(batch[0].as_deref(), Some("けいり"));
        assert_eq!(batch[1].as_deref(), Some("とうきょうと"));
        assert!(batch[2].as_deref().is_some_and(|s| s.contains("みつもり")));
        assert!(batch[4].as_deref().is_some_and(|s| s.starts_with("けいり")));

        let long = "経".repeat(MAX_CHARS + 1);
        assert_eq!(r.reading(&long), None);
    }

    #[test]
    fn readings_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Readings>();
    }

    #[test]
    fn segments_split() {
        assert_eq!(
            segments("2026_経理_見積書"),
            vec![(false, "2026"), (true, "_"), (false, "経理"), (true, "_"), (false, "見積書")]
        );
        assert_eq!(segments("けいり"), vec![(false, "けいり")]);
        assert!(segments("").is_empty());
    }

    fn make_names(n: usize) -> Vec<String> {
        let words = [
            "見積書", "請求書", "議事録", "経理", "報告書", "会議", "資料", "東京", "大阪",
            "契約書", "年度", "第1四半期", "社内", "提案", "企画書", "打ち合わせ", "名簿",
        ];
        // Small deterministic LCG, no extra dependencies.
        let mut s: u64 = 0x1234_5678;
        let mut rnd = |m: usize| {
            s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            ((s >> 33) as usize) % m
        };
        (0..n)
            .map(|_| {
                let parts = 1 + rnd(3);
                let mut name = String::new();
                match rnd(4) {
                    0 => name.push_str(&format!("{}_", 2020 + rnd(7))),
                    1 => name.push_str(&format!("{:04}{:02}{:02}_", 2020 + rnd(7), 1 + rnd(12), 1 + rnd(28))),
                    2 => name.push_str("IMG_"),
                    _ => {}
                }
                for p in 0..parts {
                    if p > 0 {
                        name.push(['_', '-', ' ', '（', '・', '.', '#', '$', '@', '\'', '~', '&', '(', ')', '+', ','][rnd(16)]);
                    }
                    name.push_str(words[rnd(words.len())]);
                    if rnd(3) == 0 {
                        name.push_str(&format!("{}", rnd(30)));
                    }
                }
                match rnd(4) {
                    0 => name.push_str(&format!("_v{}", rnd(10))),
                    1 => name.push_str(&format!("({})", 1 + rnd(5))),
                    2 => name.push_str("_最終"),
                    _ => {}
                }
                name
            })
            .collect()
    }

    #[test]
    #[ignore]
    fn throughput_3000_names() {
        let Some(r) = Readings::new() else {
            println!("MS-IME not available; skipping");
            return;
        };
        let names = make_names(3000);
        let start = std::time::Instant::now();
        let out = r.readings(&names);
        let secs = start.elapsed().as_secs_f64();
        println!("{:.0} names/s ({secs:.2}s)", names.len() as f64 / secs);
        assert_eq!(out.len(), names.len());
    }
}
