use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::{sync::Mutex, thread, time::Duration};
use windows::Win32::{
    Foundation::{GlobalFree, HANDLE, HGLOBAL, HWND},
    System::{
        DataExchange::{
            CloseClipboard, EmptyClipboard, GetClipboardData, IsClipboardFormatAvailable,
            OpenClipboard, RegisterClipboardFormatW, SetClipboardData,
        },
        Memory::{GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock, GMEM_MOVEABLE},
    },
};

// CF_UNICODETEXT from winuser.h. No browser clipboard permission is needed.
const UNICODE_TEXT: u32 = 13;
const DIB: u32 = 8;
const DIB_V5: u32 = 17;
static CLIPBOARD: Mutex<()> = Mutex::new(());

struct OpenClipboardGuard;
impl Drop for OpenClipboardGuard {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseClipboard();
        }
    }
}

struct OwnedMemory(HGLOBAL);
impl Drop for OwnedMemory {
    fn drop(&mut self) {
        unsafe {
            let _ = GlobalFree(Some(self.0));
        }
    }
}

pub fn dispatch(
    core: &crate::core::Core,
    owner: Option<HWND>,
    action: &str,
    args: &Value,
) -> Result<Value> {
    if !matches!(action, "read" | "write" | "read_attachment") {
        bail!("Pano işlemi bulunamadı");
    }
    let _lock = CLIPBOARD.lock().unwrap();
    // Remote clipboard redirection can hold the clipboard briefly after a copy.
    for attempt in 0..=20 {
        if unsafe { OpenClipboard(owner) }.is_ok() {
            break;
        }
        if attempt == 20 {
            bail!("Pano şu anda kullanılıyor. Tekrar dene.");
        }
        thread::sleep(Duration::from_millis(20));
    }
    let _clipboard = OpenClipboardGuard;
    match action {
        "read_attachment" => {
            // Copy data while its handles are valid. Release the clipboard before PNG encoding.
            let png = unsafe { RegisterClipboardFormatW(windows::core::w!("PNG")) };
            let formats = [png, DIB_V5, DIB];
            let images = formats
                .into_iter()
                .filter(|format| *format != 0)
                .filter_map(|format| clipboard_bytes(format).map(|bytes| (format, bytes)))
                .collect::<Vec<_>>();
            drop(_clipboard);
            drop(_lock);
            let id = crate::core::string(args, "id")?;
            for (format, bytes) in images {
                if format == png && crate::chat::validate_png(&bytes).is_ok() {
                    return crate::chat::save_screenshot(core, id, &bytes);
                }
                if format != png {
                    if let Some(bytes) = dib_to_png(&bytes)? {
                        return crate::chat::save_screenshot(core, id, &bytes);
                    }
                }
            }
            Ok(Value::Null)
        }
        "read" => unsafe {
            if IsClipboardFormatAvailable(UNICODE_TEXT).is_err() {
                return Ok(json!(""));
            }
            let handle = HGLOBAL(
                GetClipboardData(UNICODE_TEXT)
                    .context("Panodaki metin okunamadı")?
                    .0,
            );
            let count = GlobalSize(handle) / size_of::<u16>();
            if count == 0 {
                return Ok(json!(""));
            }
            let pointer = GlobalLock(handle).cast::<u16>();
            if pointer.is_null() {
                bail!("Panodaki metin okunamadı");
            }
            // Clipboard data is external: never scan past the allocation for NUL.
            let units = std::slice::from_raw_parts(pointer, count);
            let end = units.iter().position(|&unit| unit == 0).unwrap_or(count);
            let text = String::from_utf16_lossy(&units[..end]);
            let _ = GlobalUnlock(handle);
            Ok(json!(text))
        },
        "write" => unsafe {
            let text: Vec<u16> = args["text"]
                .as_str()
                .context("Metin gerekli")?
                .encode_utf16()
                .chain(Some(0))
                .collect();
            let memory = OwnedMemory(GlobalAlloc(GMEM_MOVEABLE, text.len() * size_of::<u16>())?);
            let pointer = GlobalLock(memory.0).cast::<u16>();
            if pointer.is_null() {
                bail!("Metin panoya kopyalanamadı");
            }
            std::ptr::copy_nonoverlapping(text.as_ptr(), pointer, text.len());
            let _ = GlobalUnlock(memory.0);
            // Allocate and fill first, so an allocation failure preserves the clipboard.
            EmptyClipboard().context("Metin panoya kopyalanamadı")?;
            SetClipboardData(UNICODE_TEXT, Some(HANDLE(memory.0 .0)))
                .context("Metin panoya kopyalanamadı")?;
            std::mem::forget(memory); // Windows owns the allocation after success.
            Ok(json!(true))
        },
        _ => unreachable!(),
    }
}

fn clipboard_bytes(format: u32) -> Option<Vec<u8>> {
    // The caller holds the clipboard open for the lifetime of this allocation.
    unsafe {
        IsClipboardFormatAvailable(format).ok()?;
        let handle = HGLOBAL(GetClipboardData(format).ok()?.0);
        let size = GlobalSize(handle);
        if size == 0 || size > crate::chat::MAX_SCREENSHOT_BYTES {
            return None;
        }
        let pointer = GlobalLock(handle).cast::<u8>();
        if pointer.is_null() {
            return None;
        }
        let bytes = std::slice::from_raw_parts(pointer, size).to_vec();
        let _ = GlobalUnlock(handle);
        Some(bytes)
    }
}

fn dib_to_png(bytes: &[u8]) -> Result<Option<Vec<u8>>> {
    if bytes.len() < 40 {
        return Ok(None);
    }
    let field = |at| u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap());
    let header = field(0) as usize;
    let width = field(4) as i32;
    let height = field(8) as i32;
    let planes = u16::from_le_bytes([bytes[12], bytes[13]]);
    let bits = u16::from_le_bytes([bytes[14], bytes[15]]);
    let compression = field(16);
    if header < 40
        || header > bytes.len()
        || planes != 1
        || width <= 0
        || height == 0
        || !matches!(bits, 24 | 32)
        || field(32) != 0
        || !matches!(compression, 0 | 3)
    {
        return Ok(None);
    }
    let columns = width as usize;
    let rows = height.unsigned_abs() as usize;
    if !crate::chat::valid_screen_dimensions(columns as u32, rows as u32) {
        return Ok(None);
    }
    let offset = if compression == 3 && header == 40 {
        52
    } else {
        header
    };
    if compression == 3
        && (bits != 32
            || bytes.len() < 52
            || [field(40), field(44), field(48)] != [0x00ff0000, 0x0000ff00, 0x000000ff])
    {
        return Ok(None);
    }
    let bytes_per_pixel = bits as usize / 8;
    let stride = (columns * bytes_per_pixel).div_ceil(4) * 4;
    let Some(bitmap) = bytes
        .get(offset..)
        .filter(|data| data.len() >= stride * rows)
    else {
        return Ok(None);
    };
    let mut rgb = Vec::with_capacity(columns * rows * 3);
    for row in 0..rows {
        let source = if height > 0 { rows - row - 1 } else { row } * stride;
        for pixel in
            bitmap[source..source + columns * bytes_per_pixel].chunks_exact(bytes_per_pixel)
        {
            rgb.extend_from_slice(&[pixel[2], pixel[1], pixel[0]]);
        }
    }
    let mut png = Vec::new();
    let mut encoder = png::Encoder::new(&mut png, columns as u32, rows as u32);
    encoder.set_color(png::ColorType::Rgb);
    encoder.set_compression(png::Compression::Fast);
    encoder.write_header()?.write_image_data(&rgb)?;
    Ok(Some(png))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn screenshot_dib_preserves_pixels_and_row_order() {
        let mut bitmap = vec![0; 40];
        bitmap[..4].copy_from_slice(&40u32.to_le_bytes());
        bitmap[4..8].copy_from_slice(&2i32.to_le_bytes());
        bitmap[8..12].copy_from_slice(&2i32.to_le_bytes());
        bitmap[12..14].copy_from_slice(&1u16.to_le_bytes());
        bitmap[14..16].copy_from_slice(&24u16.to_le_bytes());
        bitmap.extend([255, 0, 0, 255, 255, 255, 0, 0, 0, 0, 255, 0, 255, 0, 0, 0]);
        let screenshot = dib_to_png(&bitmap).unwrap().unwrap();
        let mut reader = png::Decoder::new(std::io::Cursor::new(screenshot))
            .read_info()
            .unwrap();
        let mut pixels = vec![0; reader.output_buffer_size().unwrap()];
        reader.next_frame(&mut pixels).unwrap();
        assert_eq!(pixels, [255, 0, 0, 0, 255, 0, 0, 0, 255, 255, 255, 255]);
        assert!(dib_to_png(&bitmap[..bitmap.len() - 1]).unwrap().is_none());
    }
}
