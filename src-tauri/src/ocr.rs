use std::cell::{Cell, RefCell};
use std::path::Path;

use anyhow::{Context, Result, bail};
use windows::Graphics::Imaging::{
    BitmapAlphaMode, BitmapDecoder, BitmapPixelFormat, BitmapTransform, ColorManagementMode,
    ExifOrientationMode, SoftwareBitmap,
};
use windows::Media::Ocr::OcrEngine;
use windows::Storage::{FileAccessMode, StorageFile};
use windows::Win32::System::WinRT::{RO_INIT_MULTITHREADED, RoInitialize};
use windows::core::HSTRING;

thread_local! {
    static WINRT_READY: Cell<bool> = const { Cell::new(false) };
    static ENGINE: RefCell<Option<OcrEngine>> = const { RefCell::new(None) };
}

const IMAGE_EXTENSIONS: [&str; 7] = ["png", "jpg", "jpeg", "bmp", "gif", "tif", "tiff"];

/// True when `ext` (without dot) is an image format Windows OCR can read.
pub fn is_image(ext: &str) -> bool {
    IMAGE_EXTENSIONS.iter().any(|e| e.eq_ignore_ascii_case(ext))
}

/// True when Windows OCR is usable for the user's languages.
pub fn available() -> bool {
    engine().is_ok()
}

/// Recognizes text in an image file. Returns `Ok(None)` when the extension is not an image.
/// Images larger than `OcrEngine::MaxImageDimension` are scaled down before recognition.
/// The result is truncated to `max_chars` characters.
pub fn ocr_image(path: &Path, max_chars: usize) -> Result<Option<String>> {
    let is_img = path
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(is_image);
    if !is_img {
        return Ok(None);
    }
    let engine = engine()?;
    let text = recognize(&engine, path)?;
    Ok(Some(text.chars().take(max_chars).collect()))
}

fn init_winrt() {
    WINRT_READY.with(|ready| {
        if !ready.get() {
            // SAFETY: plain WinRT runtime initialization. "Already initialized" results are ignored.
            let _ = unsafe { RoInitialize(RO_INIT_MULTITHREADED) };
            ready.set(true);
        }
    });
}

fn engine() -> Result<OcrEngine> {
    init_winrt();
    ENGINE.with(|cell| {
        let mut slot = cell.borrow_mut();
        if let Some(engine) = slot.as_ref() {
            return Ok(engine.clone());
        }
        let engine = OcrEngine::TryCreateFromUserProfileLanguages()
            .context("Windows OCR is not available for the user's languages")?;
        *slot = Some(engine.clone());
        Ok(engine)
    })
}

fn recognize(engine: &OcrEngine, path: &Path) -> Result<String> {
    let abs = std::path::absolute(path)
        .with_context(|| format!("cannot resolve path {}", path.display()))?;
    let abs_str = abs.to_str().context("path is not valid Unicode")?;

    let file = StorageFile::GetFileFromPathAsync(&HSTRING::from(abs_str))?.join()?;
    let stream = file.OpenAsync(FileAccessMode::Read)?.join()?;
    let decoder = BitmapDecoder::CreateAsync(&stream)?.join()?;
    let bitmap = decode_for_ocr(&decoder)?;
    let result = engine.RecognizeAsync(&bitmap)?.join()?;

    let lines = result.Lines()?;
    let mut out = Vec::new();
    for i in 0..lines.Size()? {
        let text = join_cjk_spaces(&lines.GetAt(i)?.Text()?.to_string());
        if !text.trim().is_empty() {
            out.push(text);
        }
    }
    Ok(out.join("\n"))
}

fn decode_for_ocr(decoder: &BitmapDecoder) -> Result<SoftwareBitmap> {
    let width = decoder.OrientedPixelWidth()?;
    let height = decoder.OrientedPixelHeight()?;
    if width == 0 || height == 0 {
        bail!("image has no pixels");
    }
    let max = OcrEngine::MaxImageDimension()?.max(1);
    let longest = width.max(height);
    let (w, h) = if longest > max {
        let scale =
            |side: u32| (u64::from(side) * u64::from(max) / u64::from(longest)).max(1) as u32;
        (scale(width), scale(height))
    } else {
        (width, height)
    };

    let transform = BitmapTransform::new()?;
    transform.SetScaledWidth(w)?;
    transform.SetScaledHeight(h)?;
    let bitmap = decoder
        .GetSoftwareBitmapTransformedAsync(
            BitmapPixelFormat::Bgra8,
            BitmapAlphaMode::Premultiplied,
            &transform,
            ExifOrientationMode::RespectExifOrientation,
            ColorManagementMode::DoNotColorManage,
        )?
        .join()?;
    Ok(bitmap)
}

fn is_cjk(c: char) -> bool {
    matches!(c as u32,
        0x3000..=0x303F   // CJK symbols and punctuation
        | 0x3040..=0x30FF // hiragana, katakana
        | 0x31F0..=0x31FF // katakana phonetic extensions
        | 0x3400..=0x4DBF // CJK extension A
        | 0x4E00..=0x9FFF // CJK unified ideographs
        | 0xF900..=0xFAFF // CJK compatibility ideographs
        | 0xFF00..=0xFFEF // fullwidth and halfwidth forms
    )
}

/// Removes spaces that sit between CJK text and CJK or digit characters.
/// Spaces between Latin words are kept.
fn join_cjk_spaces(line: &str) -> String {
    let chars: Vec<char> = line.chars().collect();
    let joinable = |c: char| is_cjk(c) || c.is_ascii_digit();
    let mut out = String::with_capacity(line.len());
    for (i, &c) in chars.iter().enumerate() {
        if c == ' ' && i > 0 && i + 1 < chars.len() {
            let (prev, next) = (chars[i - 1], chars[i + 1]);
            if joinable(prev) && joinable(next) && (is_cjk(prev) || is_cjk(next)) {
                continue;
            }
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    /// Draws `text` with a 48px Yu Gothic font on a white image using PowerShell + System.Drawing.
    fn make_test_image(
        name: &str,
        text: &str,
        width: u32,
        height: u32,
        font_px: u32,
    ) -> std::path::PathBuf {
        let out =
            std::env::temp_dir().join(format!("sagasu_ocr_{}_{}.png", name, std::process::id()));
        let script = format!(
            "Add-Type -AssemblyName System.Drawing; \
             $bmp = New-Object System.Drawing.Bitmap {width}, {height}; \
             $g = [System.Drawing.Graphics]::FromImage($bmp); \
             $g.Clear([System.Drawing.Color]::White); \
             $g.TextRenderingHint = [System.Drawing.Text.TextRenderingHint]::AntiAlias; \
             $font = New-Object System.Drawing.Font('Yu Gothic', {font_px}, [System.Drawing.FontStyle]::Regular, [System.Drawing.GraphicsUnit]::Pixel); \
             $g.DrawString($env:SAGASU_OCR_TEXT, $font, [System.Drawing.Brushes]::Black, 20, 20); \
             $bmp.Save($env:SAGASU_OCR_OUT, [System.Drawing.Imaging.ImageFormat]::Png); \
             $g.Dispose(); $bmp.Dispose()"
        );
        let output = std::process::Command::new("powershell")
            .args(["-NoProfile", "-NonInteractive", "-Command", &script])
            .env("SAGASU_OCR_TEXT", text)
            .env("SAGASU_OCR_OUT", &out)
            .output()
            .expect("failed to run powershell");
        assert!(
            output.status.success(),
            "image generation failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        out
    }

    #[test]
    fn is_image_cases() {
        for ext in ["png", "jpg", "jpeg", "bmp", "gif", "tif", "tiff", "PNG"] {
            assert!(is_image(ext), "{ext} should be an image");
        }
        for ext in ["pdf", "webp", "txt", "", "docx"] {
            assert!(!is_image(ext), "{ext} should not be an image");
        }
    }

    #[test]
    fn join_cjk_spaces_cases() {
        assert_eq!(join_cjk_spaces("見 積 書 2026 年"), "見積書2026年");
        assert_eq!(join_cjk_spaces("hello world"), "hello world");
        assert_eq!(join_cjk_spaces("見積書 ABC 123"), "見積書 ABC 123");
        assert_eq!(join_cjk_spaces("ABC 見積"), "ABC 見積");
    }

    #[test]
    fn non_image_returns_none() {
        let r = ocr_image(Path::new("C:\\does\\not\\matter.txt"), 100).unwrap();
        assert!(r.is_none());
    }

    #[test]
    fn missing_image_returns_err() {
        let r = ocr_image(Path::new("C:\\sagasu_no_such_dir\\missing.png"), 100);
        assert!(r.is_err());
    }

    #[test]
    fn ocr_reads_japanese_and_latin_text() {
        if !available() {
            eprintln!("Windows OCR not available; skipping");
            return;
        }
        let path = make_test_image("basic", "見積書 ABC 123", 800, 200, 48);
        let text = ocr_image(&path, 1000);
        let truncated = ocr_image(&path, 3);
        let _ = std::fs::remove_file(&path);
        let text = text.unwrap().unwrap();
        println!("OCR text: {text:?}");
        assert!(text.contains("見積"), "got: {text:?}");
        assert!(text.contains("ABC"), "got: {text:?}");
        let truncated = truncated.unwrap().unwrap();
        assert!(truncated.chars().count() <= 3, "got: {truncated:?}");
    }

    #[test]
    fn ocr_scales_large_images() {
        if !available() {
            eprintln!("Windows OCR not available; skipping");
            return;
        }
        let path = make_test_image("large", "見積書 ABC 123", 4000, 800, 96);
        let text = ocr_image(&path, 1000);
        let _ = std::fs::remove_file(&path);
        let text = text.unwrap().unwrap();
        println!("OCR text (large): {text:?}");
        assert!(text.contains("ABC"), "got: {text:?}");
    }

    #[test]
    #[ignore]
    fn ocr_timing() {
        if !available() {
            eprintln!("Windows OCR not available; skipping");
            return;
        }
        let path = make_test_image("timing", "見積書 ABC 123", 800, 200, 48);
        let mut total = 0.0;
        for i in 1..=20 {
            let start = Instant::now();
            let text = ocr_image(&path, 1000).unwrap().unwrap();
            let ms = start.elapsed().as_secs_f64() * 1000.0;
            total += ms;
            println!("run {i:2}: {ms:8.1} ms  ({} chars)", text.chars().count());
        }
        let _ = std::fs::remove_file(&path);
        println!("average: {:.1} ms per image over 20 runs", total / 20.0);
    }
}
