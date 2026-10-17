use std::fs::File;
use std::io::Read;
use std::path::Path;

use anyhow::{Context, Result};
use calamine::{Data, Reader, open_workbook_auto};
use quick_xml::events::Event;
use quick_xml::Reader as XmlReader;
use zip::ZipArchive;

/// Upper bound on bytes read from plain-text files.
const MAX_PLAIN_BYTES: u64 = 4 * 1024 * 1024;

/// Upper bound on decompressed content stream bytes per PDF page.
const MAX_PDF_PAGE_BYTES: usize = 32 * 1024 * 1024;

const PLAIN_TEXT_EXTS: &[&str] = &[
    "txt", "md", "markdown", "csv", "tsv", "log", "json", "xml", "html", "htm", "yaml", "yml",
    "toml", "ini", "cfg", "rs", "py", "js", "ts", "tsx", "jsx", "java", "c", "cpp", "h", "hpp",
    "cs", "go", "rb", "php", "sql", "sh", "ps1", "bat", "css", "vue", "svelte", "kt", "swift",
];

const OFFICE_EXTS: &[&str] = &["docx", "pptx", "xlsx", "xlsm", "xls", "ods", "odt", "pdf"];

/// Everyday documents. Source code, logs and config files are only indexed when the user opts in:
/// on a developer's PC they are most of the text and would bury the documents people look for.
const DOCUMENT_EXTS: &[&str] = &["txt", "md", "markdown", "csv", "tsv", "html", "htm"];

/// Plain-text files keep less text than office documents: long ones are usually data dumps.
const MAX_PLAIN_CHARS: usize = 100_000;

/// Whether `ext` should get full-text indexing under the user's settings.
pub fn should_index(ext: &str, include_code: bool) -> bool {
    let ext = ext.to_ascii_lowercase();
    OFFICE_EXTS.contains(&ext.as_str()) || DOCUMENT_EXTS.contains(&ext.as_str()) || (include_code && is_supported(&ext))
}

/// Extensions (lowercase, no dot) whose text content we can index.
pub fn is_supported(ext: &str) -> bool {
    let ext = ext.to_ascii_lowercase();
    PLAIN_TEXT_EXTS.contains(&ext.as_str()) || OFFICE_EXTS.contains(&ext.as_str())
}

/// Extract plain text from a file for full-text indexing.
/// Returns Ok(None) when the extension is not supported.
/// The returned text is truncated to at most `max_chars` characters (char count, not bytes; never split a UTF-8 char).
pub fn extract_text(path: &Path, max_chars: usize) -> Result<Option<String>> {
    let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
        return Ok(None);
    };
    let ext = ext.to_ascii_lowercase();
    if !is_supported(&ext) {
        return Ok(None);
    }

    let raw = match ext.as_str() {
        "docx" => extract_docx(path)?,
        "pptx" => extract_pptx(path)?,
        "xlsx" | "xlsm" | "xls" | "ods" => extract_workbook(path)?,
        "odt" => extract_odt(path)?,
        "pdf" => extract_pdf(path)?,
        "html" | "htm" | "xml" => strip_tags(&read_plain(path)?),
        _ => read_plain(path)?,
    };

    let text = normalize_text(&raw);
    let limit = if OFFICE_EXTS.contains(&ext.as_str()) { max_chars } else { max_chars.min(MAX_PLAIN_CHARS) };
    Ok(Some(truncate_chars(text, limit)))
}

fn read_plain(path: &Path) -> Result<String> {
    let file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut buf = Vec::new();
    file.take(MAX_PLAIN_BYTES)
        .read_to_end(&mut buf)
        .with_context(|| format!("read {}", path.display()))?;
    Ok(decode_bytes(&buf))
}

/// Decodes text bytes: UTF-8 (BOM stripped), UTF-16 with BOM, otherwise Shift_JIS.
fn decode_bytes(bytes: &[u8]) -> String {
    if let Some(rest) = bytes.strip_prefix(&[0xFF, 0xFE]) {
        return decode_utf16(rest, true);
    }
    if let Some(rest) = bytes.strip_prefix(&[0xFE, 0xFF]) {
        return decode_utf16(rest, false);
    }
    let bytes = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes);
    match std::str::from_utf8(bytes) {
        Ok(s) => s.to_owned(),
        // The read may have cut a multi-byte char at the end; keep the valid prefix.
        Err(e) if e.error_len().is_none() => {
            String::from_utf8_lossy(&bytes[..e.valid_up_to()]).into_owned()
        }
        // Not UTF-8: on Japanese Windows that is almost always Shift_JIS (CP932).
        Err(_) => encoding_rs::SHIFT_JIS.decode(bytes).0.into_owned(),
    }
}

fn decode_utf16(bytes: &[u8], little_endian: bool) -> String {
    let (pairs, _rest) = bytes.as_chunks::<2>();
    let units: Vec<u16> = pairs
        .iter()
        .map(|&pair| {
            if little_endian {
                u16::from_le_bytes(pair)
            } else {
                u16::from_be_bytes(pair)
            }
        })
        .collect();
    String::from_utf16_lossy(&units)
}

/// Removes `<...>` sequences, leaving a space in their place, and unescapes entities.
fn strip_tags(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut in_tag = false;
    for c in input.chars() {
        match c {
            '<' => {
                in_tag = true;
                out.push(' ');
            }
            '>' if in_tag => in_tag = false,
            _ if in_tag => {}
            _ => out.push(c),
        }
    }
    let unescaped = quick_xml::escape::unescape(&out).map(|c| c.into_owned());
    unescaped.unwrap_or(out)
}

fn open_zip(path: &Path) -> Result<ZipArchive<File>> {
    let file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    ZipArchive::new(file).with_context(|| format!("not a valid zip: {}", path.display()))
}

fn zip_entry_names(zip: &ZipArchive<File>) -> Vec<String> {
    zip.file_names()
        .filter_map(|n| n.ok())
        .map(|n| n.into_owned())
        .collect()
}

fn read_zip_entry(zip: &mut ZipArchive<File>, name: &str) -> Result<String> {
    let mut entry = zip
        .by_name(name)
        .with_context(|| format!("missing entry {name}"))?;
    let mut buf = Vec::new();
    entry
        .read_to_end(&mut buf)
        .with_context(|| format!("read entry {name}"))?;
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

fn extract_docx(path: &Path) -> Result<String> {
    let mut zip = open_zip(path)?;
    let names = zip_entry_names(&zip);

    let mut headers: Vec<&String> = names.iter().filter(|n| is_word_part(n, "header")).collect();
    let mut footers: Vec<&String> = names.iter().filter(|n| is_word_part(n, "footer")).collect();
    headers.sort();
    footers.sort();

    let mut parts = Vec::new();
    for name in headers {
        let xml = read_zip_entry(&mut zip, name)?;
        parts.push(ooxml_text(&xml, b"t", &[b"p"], b"tab")?);
    }
    let body = read_zip_entry(&mut zip, "word/document.xml")?;
    parts.push(ooxml_text(&body, b"t", &[b"p"], b"tab")?);
    for name in footers {
        let xml = read_zip_entry(&mut zip, name)?;
        parts.push(ooxml_text(&xml, b"t", &[b"p"], b"tab")?);
    }

    Ok(parts.join("\n"))
}

fn is_word_part(name: &str, kind: &str) -> bool {
    let Some(rest) = name.strip_prefix("word/") else {
        return false;
    };
    rest.starts_with(kind) && rest.ends_with(".xml")
}

fn extract_pptx(path: &Path) -> Result<String> {
    let mut zip = open_zip(path)?;
    let names = zip_entry_names(&zip);

    let mut slides: Vec<(u64, String)> = names
        .into_iter()
        .filter_map(|name| {
            let num = name
                .strip_prefix("ppt/slides/slide")?
                .strip_suffix(".xml")?
                .parse::<u64>()
                .ok()?;
            Some((num, name))
        })
        .collect();
    slides.sort_by_key(|(num, _)| *num);

    let mut parts = Vec::with_capacity(slides.len());
    for (_, name) in &slides {
        let xml = read_zip_entry(&mut zip, name)?;
        parts.push(ooxml_text(&xml, b"t", &[b"p"], b"tab")?);
    }
    Ok(parts.join("\n"))
}

fn extract_odt(path: &Path) -> Result<String> {
    let mut zip = open_zip(path)?;
    let xml = read_zip_entry(&mut zip, "content.xml")?;
    ooxml_text(&xml, b"", &[b"p", b"h"], b"tab")
}

/// Collects text from an OOXML/ODF part.
///
/// When `text_tag` is non-empty, only text inside elements with that local name is
/// collected; otherwise every text node is used. Elements named in `paragraph_tags`
/// end a line, and `tab_tag` elements become a tab.
fn ooxml_text(xml: &str, text_tag: &[u8], paragraph_tags: &[&[u8]], tab_tag: &[u8]) -> Result<String> {
    let all_text = text_tag.is_empty();
    let mut reader = XmlReader::from_str(xml);
    let mut out = String::new();
    let mut in_text = all_text;

    loop {
        match reader.read_event().context("malformed XML")? {
            Event::Start(e) => {
                let local = e.local_name();
                let local = local.as_ref().as_bytes();
                if !all_text && local == text_tag {
                    in_text = true;
                }
            }
            Event::Empty(e) => {
                let local = e.local_name();
                let local = local.as_ref().as_bytes();
                if local == tab_tag {
                    out.push('\t');
                } else if paragraph_tags.contains(&local) {
                    out.push('\n');
                }
            }
            Event::End(e) => {
                let local = e.local_name();
                let local = local.as_ref().as_bytes();
                if !all_text && local == text_tag {
                    in_text = false;
                } else if paragraph_tags.contains(&local) {
                    out.push('\n');
                }
            }
            Event::Text(t) if in_text => out.push_str(&t.xml10_content()),
            Event::GeneralRef(r) if in_text => {
                let entity = format!("&{};", r.xml10_content());
                match quick_xml::escape::unescape(&entity) {
                    Ok(s) => out.push_str(&s),
                    Err(_) => out.push_str(&entity),
                }
            }
            Event::CData(c) if in_text => {
                out.push_str(&c.into_inner());
            }
            Event::Eof => break,
            _ => {}
        }
    }
    Ok(out)
}

fn extract_workbook(path: &Path) -> Result<String> {
    let mut workbook = open_workbook_auto(path)
        .with_context(|| format!("open workbook {}", path.display()))?;
    let mut out = String::new();
    for name in workbook.sheet_names() {
        let range = workbook
            .worksheet_range(&name)
            .with_context(|| format!("read sheet {name}"))?;
        out.push_str(&name);
        out.push('\n');
        for row in range.rows() {
            let cells: Vec<String> = row
                .iter()
                .filter(|c| !matches!(c, Data::Empty))
                .map(|c| c.to_string())
                .filter(|s| !s.is_empty())
                .collect();
            if cells.is_empty() {
                continue;
            }
            out.push_str(&cells.join("\t"));
            out.push('\n');
        }
    }
    Ok(out)
}

/// Extracts text from a PDF. Encrypted, broken or panicking documents yield an empty string.
/// Only I/O errors reading the file are returned as errors.
fn extract_pdf(path: &Path) -> Result<String> {
    let bytes = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
    let result = std::panic::catch_unwind(|| pdf_text_from_bytes(&bytes));
    Ok(result.unwrap_or_default())
}

fn pdf_text_from_bytes(bytes: &[u8]) -> String {
    let Ok(doc) = lopdf::Document::load_mem(bytes) else {
        return String::new();
    };
    let mut out = String::new();
    for page in doc.get_pages().keys() {
        if let Ok(text) = doc.extract_text_with_limit(&[*page], MAX_PDF_PAGE_BYTES) {
            out.push_str(&text);
            out.push('\n');
        }
    }
    out
}

/// Collapses runs of 3+ newlines into two and trims the result.
fn normalize_text(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut newlines = 0usize;
    for c in input.chars() {
        match c {
            '\r' => {}
            '\n' => {
                newlines += 1;
                if newlines <= 2 {
                    out.push('\n');
                }
            }
            _ => {
                newlines = 0;
                out.push(c);
            }
        }
    }
    out.trim().to_owned()
}

fn truncate_chars(mut text: String, max_chars: usize) -> String {
    if let Some((idx, _)) = text.char_indices().nth(max_chars) {
        text.truncate(idx);
        let trimmed_len = text.trim_end().len();
        text.truncate(trimmed_len);
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Cursor, Write};

    fn write_file(dir: &Path, name: &str, bytes: &[u8]) -> std::path::PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, bytes).unwrap();
        path
    }

    fn make_zip(entries: &[(&str, &str)]) -> Vec<u8> {
        let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let options = zip::write::SimpleFileOptions::default();
        for (name, content) in entries {
            writer.start_file(*name, options).unwrap();
            writer.write_all(content.as_bytes()).unwrap();
        }
        writer.finish().unwrap().into_inner()
    }

    const DOCX_DOCUMENT: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
<w:body>
<w:p><w:r><w:t>見積書 A社 保守費用</w:t></w:r></w:p>
<w:p><w:r><w:t>項目</w:t></w:r><w:r><w:tab/></w:r><w:r><w:t>金額 &amp; 税</w:t></w:r></w:p>
</w:body>
</w:document>"#;

    #[test]
    fn supported_extensions() {
        assert!(is_supported("txt"));
        assert!(is_supported("rs"));
        assert!(is_supported("docx"));
        assert!(is_supported("pdf"));
        assert!(is_supported("xlsm"));
        assert!(!is_supported("png"));
        assert!(!is_supported(""));
    }

    #[test]
    fn unsupported_extension_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_file(dir.path(), "image.png", b"\x89PNG");
        assert_eq!(extract_text(&path, 100).unwrap(), None);
        let no_ext = write_file(dir.path(), "README", b"hello");
        assert_eq!(extract_text(&no_ext, 100).unwrap(), None);
    }

    #[test]
    fn docx_text_paragraphs_and_tabs() {
        let dir = tempfile::tempdir().unwrap();
        let bytes = make_zip(&[("word/document.xml", DOCX_DOCUMENT)]);
        let path = write_file(dir.path(), "quote.docx", &bytes);
        let text = extract_text(&path, 1000).unwrap().unwrap();
        assert_eq!(text, "見積書 A社 保守費用\n項目\t金額 & 税");
    }

    #[test]
    fn docx_includes_header_and_footer() {
        let dir = tempfile::tempdir().unwrap();
        let header = r#"<w:hdr xmlns:w="x"><w:p><w:r><w:t>社外秘</w:t></w:r></w:p></w:hdr>"#;
        let footer = r#"<w:ftr xmlns:w="x"><w:p><w:r><w:t>ページ末尾</w:t></w:r></w:p></w:ftr>"#;
        let bytes = make_zip(&[
            ("word/document.xml", DOCX_DOCUMENT),
            ("word/header1.xml", header),
            ("word/footer1.xml", footer),
        ]);
        let path = write_file(dir.path(), "with_hf.docx", &bytes);
        let text = extract_text(&path, 1000).unwrap().unwrap();
        assert!(text.starts_with("社外秘"));
        assert!(text.contains("見積書 A社 保守費用"));
        assert!(text.contains("ページ末尾"));
    }

    #[test]
    fn pptx_slides_in_numeric_order() {
        fn slide(text: &str) -> String {
            format!(
                r#"<p:sld xmlns:a="a" xmlns:p="p"><p:cSld><p:spTree><p:sp><p:txBody>
<a:p><a:r><a:t>{text}</a:t></a:r></a:p><a:p><a:r><a:t>二行目</a:t></a:r></a:p>
</p:txBody></p:sp></p:spTree></p:cSld></p:sld>"#
            )
        }
        let dir = tempfile::tempdir().unwrap();
        let s1 = slide("一枚目");
        let s2 = slide("二枚目");
        let s10 = slide("十枚目");
        let bytes = make_zip(&[
            ("ppt/slides/slide10.xml", &s10),
            ("ppt/slides/slide2.xml", &s2),
            ("ppt/slides/slide1.xml", &s1),
        ]);
        let path = write_file(dir.path(), "deck.pptx", &bytes);
        let text = extract_text(&path, 1000).unwrap().unwrap();
        let first = text.find("一枚目").unwrap();
        let second = text.find("二枚目").unwrap();
        let tenth = text.find("十枚目").unwrap();
        assert!(first < second && second < tenth);
        assert!(text.contains("一枚目\n二行目"));
    }

    #[test]
    fn odt_content_text() {
        let dir = tempfile::tempdir().unwrap();
        let content = r#"<office:document-content xmlns:office="o" xmlns:text="t">
<office:body><office:text><text:h>見出し</text:h><text:p>本文です</text:p></office:text></office:body>
</office:document-content>"#;
        let bytes = make_zip(&[("content.xml", content)]);
        let path = write_file(dir.path(), "memo.odt", &bytes);
        let text = extract_text(&path, 1000).unwrap().unwrap();
        assert_eq!(text, "見出し\n本文です");
    }

    #[test]
    fn utf8_with_bom_is_decoded_without_bom() {
        let dir = tempfile::tempdir().unwrap();
        let mut bytes = vec![0xEF, 0xBB, 0xBF];
        bytes.extend_from_slice("こんにちは".as_bytes());
        let path = write_file(dir.path(), "bom.txt", &bytes);
        let text = extract_text(&path, 100).unwrap().unwrap();
        assert_eq!(text, "こんにちは");
    }

    #[test]
    fn utf16le_with_bom_is_decoded() {
        let dir = tempfile::tempdir().unwrap();
        let mut bytes = vec![0xFF, 0xFE];
        for unit in "見積書 utf16".encode_utf16() {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
        let path = write_file(dir.path(), "wide.txt", &bytes);
        let text = extract_text(&path, 100).unwrap().unwrap();
        assert_eq!(text, "見積書 utf16");
    }

    #[test]
    fn utf16be_with_bom_is_decoded() {
        let dir = tempfile::tempdir().unwrap();
        let mut bytes = vec![0xFE, 0xFF];
        for unit in "保守".encode_utf16() {
            bytes.extend_from_slice(&unit.to_be_bytes());
        }
        let path = write_file(dir.path(), "wide_be.txt", &bytes);
        let text = extract_text(&path, 100).unwrap().unwrap();
        assert_eq!(text, "保守");
    }

    #[test]
    fn html_tags_are_stripped() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_file(
            dir.path(),
            "page.html",
            "<html><body><p>Hello &amp; bye</p></body></html>".as_bytes(),
        );
        let text = extract_text(&path, 100).unwrap().unwrap();
        assert!(text.contains("Hello & bye"));
        assert!(!text.contains('<'));
    }

    #[test]
    fn truncation_counts_chars_not_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_file(dir.path(), "long.txt", "あいうえおかきくけこ".as_bytes());
        assert_eq!(extract_text(&path, 3).unwrap().unwrap(), "あいう");
        assert_eq!(extract_text(&path, 0).unwrap().unwrap(), "");
        assert_eq!(
            extract_text(&path, 1000).unwrap().unwrap(),
            "あいうえおかきくけこ"
        );
    }

    #[test]
    fn blank_lines_are_collapsed() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_file(dir.path(), "blank.txt", b"\n\na\n\n\n\n\nb\r\n\r\n\r\n");
        let text = extract_text(&path, 100).unwrap().unwrap();
        assert_eq!(text, "a\n\nb");
    }

    #[test]
    fn invalid_utf8_does_not_fail() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_file(dir.path(), "raw.txt", &[b'a', 0xFF, 0xFE, b'b']);
        let text = extract_text(&path, 100).unwrap().unwrap();
        assert!(text.starts_with('a') && text.ends_with('b'));
    }

    #[test]
    fn documents_only_by_default() {
        for e in ["docx", "PDF", "xlsx", "txt", "md", "csv"] {
            assert!(should_index(e, false), "{e}");
        }
        for e in ["cs", "log", "json", "js"] {
            assert!(!should_index(e, false), "{e} is off by default");
            assert!(should_index(e, true), "{e} with code on");
        }
        assert!(!should_index("exe", true));
    }

    #[test]
    fn plain_text_is_capped() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_file(dir.path(), "big.txt", "あ".repeat(150_000).as_bytes());
        assert_eq!(extract_text(&path, 300_000).unwrap().unwrap().chars().count(), MAX_PLAIN_CHARS);
    }

    #[test]
    fn shift_jis_text() {
        let dir = tempfile::tempdir().unwrap();
        let (bytes, _, _) = encoding_rs::SHIFT_JIS.encode("見積書 A社");
        let path = write_file(dir.path(), "sjis.txt", &bytes);
        assert_eq!(extract_text(&path, 100).unwrap().unwrap(), "見積書 A社");
    }

    #[test]
    fn missing_xlsx_is_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("does_not_exist.xlsx");
        assert!(extract_text(&path, 100).is_err());
    }

    #[test]
    fn corrupt_xlsx_is_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_file(dir.path(), "broken.xlsx", b"not a zip");
        assert!(extract_text(&path, 100).is_err());
    }

    #[test]
    fn broken_pdf_yields_empty_text() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_file(dir.path(), "broken.pdf", b"%PDF-1.4 garbage");
        assert_eq!(extract_text(&path, 100).unwrap(), Some(String::new()));
    }

    #[test]
    fn pdf_text_is_extracted() {
        use lopdf::content::{Content, Operation};
        use lopdf::{Document, Object, Stream, dictionary};

        let mut doc = Document::with_version("1.5");
        let pages_id = doc.new_object_id();
        let font_id = doc.add_object(dictionary! {
            "Type" => "Font",
            "Subtype" => "Type1",
            "BaseFont" => "Helvetica",
        });
        let resources_id = doc.add_object(dictionary! {
            "Font" => dictionary! { "F1" => font_id },
        });
        let content = Content {
            operations: vec![
                Operation::new("BT", vec![]),
                Operation::new("Tf", vec!["F1".into(), 24.into()]),
                Operation::new("Td", vec![72.into(), 700.into()]),
                Operation::new("Tj", vec![Object::string_literal("Hello PDF")]),
                Operation::new("ET", vec![]),
            ],
        };
        let content_id = doc.add_object(Stream::new(dictionary! {}, content.encode().unwrap()));
        let page_id = doc.add_object(dictionary! {
            "Type" => "Page",
            "Parent" => pages_id,
            "Contents" => content_id,
        });
        doc.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! {
                "Type" => "Pages",
                "Kids" => vec![page_id.into()],
                "Count" => 1,
                "Resources" => resources_id,
                "MediaBox" => vec![0.into(), 0.into(), 595.into(), 842.into()],
            }),
        );
        let catalog_id = doc.add_object(dictionary! {
            "Type" => "Catalog",
            "Pages" => pages_id,
        });
        doc.trailer.set("Root", catalog_id);

        let mut bytes = Vec::new();
        doc.save_to(&mut bytes).unwrap();

        let dir = tempfile::tempdir().unwrap();
        let path = write_file(dir.path(), "hello.pdf", &bytes);
        let text = extract_text(&path, 1000).unwrap().unwrap();
        assert!(text.contains("Hello PDF"), "got: {text:?}");
    }
}
