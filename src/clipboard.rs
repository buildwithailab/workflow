use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::{sync::Mutex, thread, time::Duration};
use windows::Win32::{
    Foundation::{GlobalFree, HANDLE, HGLOBAL, HWND},
    System::{
        DataExchange::{
            CloseClipboard, EmptyClipboard, GetClipboardData, IsClipboardFormatAvailable,
            OpenClipboard, SetClipboardData,
        },
        Memory::{GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock, GMEM_MOVEABLE},
    },
};

// CF_UNICODETEXT from winuser.h. No browser clipboard permission is needed.
const UNICODE_TEXT: u32 = 13;
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

pub fn dispatch(owner: HWND, action: &str, args: &Value) -> Result<Value> {
    if !matches!(action, "read" | "write") {
        bail!("Pano işlemi bulunamadı");
    }
    let _lock = CLIPBOARD.lock().unwrap();
    // Other Windows apps may hold the clipboard briefly while copying.
    let mut opened = false;
    for _ in 0..10 {
        if unsafe { OpenClipboard(Some(owner)) }.is_ok() {
            opened = true;
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    if !opened {
        bail!("Pano şu anda kullanılıyor. Tekrar dene.");
    }
    let _clipboard = OpenClipboardGuard;
    match action {
        "read" => unsafe {
            if IsClipboardFormatAvailable(UNICODE_TEXT).is_err() {
                return Ok(json!(""));
            }
            let handle = HGLOBAL(GetClipboardData(UNICODE_TEXT)?.0);
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
            let memory = OwnedMemory(GlobalAlloc(
                GMEM_MOVEABLE,
                text.len() * size_of::<u16>(),
            )?);
            let pointer = GlobalLock(memory.0).cast::<u16>();
            if pointer.is_null() {
                bail!("Metin panoya kopyalanamadı");
            }
            std::ptr::copy_nonoverlapping(text.as_ptr(), pointer, text.len());
            let _ = GlobalUnlock(memory.0);
            // Allocate and fill first, so an allocation failure preserves the clipboard.
            EmptyClipboard()?;
            SetClipboardData(UNICODE_TEXT, Some(HANDLE(memory.0.0)))?;
            std::mem::forget(memory); // Windows owns the allocation after success.
            Ok(json!(true))
        },
        _ => unreachable!(),
    }
}
