//! Text normalization so that 「ミツモリ」「みつもり」「ﾐﾂﾓﾘ」「mitsumori」 all match.

use unicode_normalization::UnicodeNormalization;

/// NFKC + lowercase + katakana→hiragana. Used for both indexed names and queries.
pub fn normalize(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.nfkc().flat_map(char::to_lowercase) {
        out.push(kata_to_hira(c));
    }
    out
}

fn kata_to_hira(c: char) -> char {
    match c {
        // ァ..ヶ map onto ぁ..ゖ
        '\u{30A1}'..='\u{30F6}' => char::from_u32(c as u32 - 0x60).unwrap_or(c),
        _ => c,
    }
}

/// Alternative spellings of one query token: the normalized token itself and,
/// for ASCII romaji, its hiragana reading (e.g. "mitsumori" → "みつもり").
pub fn variants(token: &str) -> Vec<String> {
    let base = normalize(token);
    let mut v = vec![base.clone()];
    if base.len() >= 2 && base.chars().all(|c| c.is_ascii_alphabetic() || c == '-') {
        let kana = romaji_to_hiragana(&base);
        if !kana.is_empty() && kana != base && !kana.chars().any(|c| c.is_ascii_alphabetic()) {
            v.push(kana);
        }
    }
    v
}

fn romaji_to_hiragana(s: &str) -> String {
    use wana_kana::ConvertJapanese;
    normalize(&s.to_hiragana())
}

/// Check whether `needle` appears in `hay` as an in-order subsequence of chars,
/// returning a tightness score (higher is better) when it does.
pub fn subsequence_score(hay: &str, needle: &str) -> Option<u32> {
    let mut it = hay.char_indices();
    let mut first = None;
    let mut last = 0;
    for nc in needle.chars() {
        loop {
            let (i, hc) = it.next()?;
            if hc == nc {
                first.get_or_insert(i);
                last = i;
                break;
            }
        }
    }
    let span = (last - first.unwrap_or(0)) as u32 + 1;
    let n = needle.len() as u32;
    Some((n * 100) / span.max(n))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kana_width_case() {
        assert_eq!(normalize("ミツモリ"), "みつもり");
        assert_eq!(normalize("ﾐﾂﾓﾘ"), "みつもり");
        assert_eq!(normalize("ＡＢＣ.PDF"), "abc.pdf");
        assert_eq!(normalize("見積書_Ａ社"), "見積書_a社");
    }

    #[test]
    fn romaji_variant() {
        let v = variants("mitsumori");
        assert!(v.contains(&"みつもり".to_string()), "{v:?}");
        assert_eq!(variants("pdf")[0], "pdf");
        assert_eq!(variants("見積"), vec!["見積".to_string()]);
    }

    #[test]
    fn subsequence() {
        assert!(subsequence_score("estimate_2026.xlsx", "est26").is_some());
        assert!(subsequence_score("abc", "acb").is_none());
        let tight = subsequence_score("report", "rep").unwrap();
        let loose = subsequence_score("r_e_p_ort", "rep").unwrap();
        assert!(tight > loose);
    }
}
