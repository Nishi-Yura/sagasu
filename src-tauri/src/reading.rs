//! Hiragana readings of Japanese text via the Microsoft IME (MS-IME).

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

type Request = (Vec<String>, Sender<Vec<Option<String>>>);

/// Converts names to hiragana readings with the Windows Japanese IME.
/// Owns a dedicated background thread (COM objects are apartment-bound), so it is Send + Sync and cheap to call from any thread.
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
        let (reply_tx, reply_rx) = mpsc::channel();
        if self.tx.send((texts.to_vec(), reply_tx)).is_err() {
            return vec![None; texts.len()];
        }
        match reply_rx.recv() {
            Ok(out) if out.len() == texts.len() => out,
            _ => vec![None; texts.len()],
        }
    }
}

/// True when the string contains at least one CJK ideograph (U+4E00..U+9FFF, U+3400..U+4DBF, 々).
pub fn has_kanji(s: &str) -> bool {
    s.chars()
        .any(|c| matches!(c, '\u{4E00}'..='\u{9FFF}' | '\u{3400}'..='\u{4DBF}' | '々'))
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

    while let Ok((texts, reply)) = rx.recv() {
        let out = texts.iter().map(|t| ime.reading(t)).collect();
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
    #[ignore]
    fn throughput_2000_names() {
        let Some(r) = Readings::new() else {
            println!("MS-IME not available; skipping");
            return;
        };
        let base = [
            "経理_2026年度_見積書",
            "東京都_契約書",
            "営業報告書_第3四半期",
            "人事評価_山田太郎",
            "請求書_株式会社東京商事",
        ];
        let names: Vec<String> = (0..2000)
            .map(|i| format!("{}_{i}", base[i % base.len()]))
            .collect();
        let start = std::time::Instant::now();
        let out = r.readings(&names);
        let elapsed = start.elapsed();
        let ok = out.iter().filter(|o| o.is_some()).count();
        println!(
            "converted {} names in {:?} ({:.2} ms/name, {} ok)",
            names.len(),
            elapsed,
            elapsed.as_secs_f64() * 1000.0 / names.len() as f64,
            ok
        );
        assert_eq!(out.len(), names.len());
    }
}
