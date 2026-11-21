use crate::core::{string, Core};
use anyhow::{bail, Context, Result};
use objc2::{AnyThread, rc::autoreleasepool};
use objc2_app_kit::{NSBitmapImageFileType, NSBitmapImageRep, NSPasteboard, NSPasteboardTypeString, NSPasteboardTypePNG, NSPasteboardTypeTIFF, NSPasteboardTypeFileURL};
use objc2_foundation::{NSDictionary, NSString};
use serde_json::{json, Value};
use std::{sync::mpsc, time::Duration};

enum Content { Value(Value), Png(Vec<u8>), Files(Vec<std::path::PathBuf>) }

pub(crate) fn dispatch(core: &Core, _owner: Option<()>, action: &str, args: &Value) -> Result<Value> {
    let action = action.to_owned();
    let input = args.clone();
    let (send, receive) = mpsc::sync_channel(1);
    core.app.run_on_main_thread(move || {
        let result = autoreleasepool(|_| read(&action, &input));
        let _ = send.send(result);
    })?;
    let payload = receive.recv_timeout(Duration::from_secs(5)).context("Pano yanıtı gecikti")??;
    match payload {
    Content::Png(bytes) => crate::chat::save_screenshot(core, string(&args, "id")?, &bytes),
    Content::Files(paths) => {
        let session = core.session(string(&args, "id")?)?;
        let attachments = paths.iter().map(|path| crate::chat::file_reference(&session, path.to_str().context("Dosya yolu geçersiz")?, false)).collect::<Result<Vec<_>>>()?;
        Ok(json!({"attachments": attachments}))
    },
    Content::Value(value) => Ok(value),
    }
}

fn read(action: &str, args: &Value) -> Result<Content> {
    let board = NSPasteboard::generalPasteboard();
    match action {
        "write" => {
            let value = NSString::from_str(string(args, "text")?);
            board.clearContents();
            if !board.setString_forType(&value, unsafe { NSPasteboardTypeString }) { bail!("Metin panoya kopyalanamadı"); }
            Ok(Content::Value(json!(true)))
        }
        "read" => Ok(Content::Value(json!(board.stringForType(unsafe { NSPasteboardTypeString }).map(|s| s.to_string()).unwrap_or_default()))),
        "read_attachment" => {
            if let Some(items) = board.pasteboardItems() {
                let paths = items.iter().filter_map(|item| item.stringForType(unsafe { NSPasteboardTypeFileURL }))
                    .map(|value| url::Url::parse(&value.to_string())?.to_file_path().map_err(|_| anyhow::anyhow!("Dosya adresi geçersiz")))
                    .collect::<Result<Vec<_>>>()?;
                if !paths.is_empty() { return Ok(Content::Files(paths)); }
            }
            for format in [unsafe { NSPasteboardTypePNG }, unsafe { NSPasteboardTypeTIFF }] {
                if let Some(data) = board.dataForType(format) {
                    if data.len() > crate::chat::MAX_SCREENSHOT_BYTES { bail!("Ekran görüntüsü çok büyük."); }
                    if format == unsafe { NSPasteboardTypePNG } {
                        return Ok(Content::Png(data.to_vec()));
                    }
                    let image = NSBitmapImageRep::initWithData(NSBitmapImageRep::alloc(), &data).context("Pano görseli okunamadı")?;
                    if !crate::chat::valid_screen_dimensions(image.pixelsWide() as u32, image.pixelsHigh() as u32) { bail!("Ekran görüntüsü çok büyük."); }
                    let png = unsafe { image.representationUsingType_properties(NSBitmapImageFileType::PNG, &NSDictionary::new()) }.context("Pano görseli dönüştürülemedi")?;
                    return Ok(Content::Png(png.to_vec()));
                }
            }
            Ok(Content::Value(Value::Null))
        }
        _ => bail!("Pano işlemi bulunamadı"),
    }
}

#[cfg(any(test, feature = "desktop-tests"))]
const MAX_TEST_REQUEST_BYTES: usize = 64 * 1024 * 1024;

#[cfg(any(test, feature = "desktop-tests"))]
fn check_restore_size(items: &Value, revision: i64, limit: usize) -> Result<usize> {
    #[derive(serde::Serialize)]
    struct Restore<'a> {
        action: &'static str,
        items: &'a Value,
        #[serde(rename = "expectedChangeCount")]
        revision: i64,
    }
    struct Count { bytes: usize, limit: usize }
    impl std::io::Write for Count {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.bytes = self.bytes.checked_add(bytes.len())
                .filter(|size| *size <= self.limit)
                .ok_or_else(|| std::io::Error::other("clipboard-snapshot-too-large"))?;
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> { Ok(()) }
    }
    // Count the newline too, without allocating another copy of private data.
    let mut counter = Count { bytes: 1, limit };
    serde_json::to_writer(&mut counter, &Restore { action: "restore", items, revision })
        .map_err(|_| anyhow::anyhow!("clipboard-snapshot-too-large"))?;
    Ok(counter.bytes)
}

#[cfg(any(test, feature = "desktop-tests"))]
fn check_test_revision(request: &Value, revision: i64) -> Result<()> {
    if request["expectedChangeCount"].as_i64() != Some(revision) {
        bail!("clipboard-revision-changed")
    }
    Ok(())
}

#[cfg(feature = "desktop-tests")]
pub(crate) fn test_helper() -> Result<()> {
    use base64::{engine::general_purpose::STANDARD, Engine};
    use objc2::{rc::Retained, runtime::ProtocolObject};
    use objc2_app_kit::{NSPasteboardItem, NSPasteboardWriting};
    use objc2_foundation::{NSArray, NSData};
    use std::io::{BufRead, Read, Write};
    let mut input_stream = std::io::stdin().lock();
    loop {
    let mut input = String::new();
    if (&mut input_stream).take((MAX_TEST_REQUEST_BYTES + 1) as u64).read_line(&mut input)? == 0 { break; }
    if input.len() > MAX_TEST_REQUEST_BYTES { bail!("clipboard-request-too-large"); }
    let request: Value = serde_json::from_str(&input)?;
    let mut modified_revision = None;
    let result = autoreleasepool(|_| -> Result<Value> {
        let board = NSPasteboard::generalPasteboard();
        if request["action"] == "revision" { return Ok(json!(board.changeCount())); }
        if request["action"] == "snapshot" {
            let revision = board.changeCount();
            let mut saved = vec![];
            let mut total = 0usize;
            if let Some(items) = board.pasteboardItems() {
                for item in items.iter() {
                    let mut types = vec![];
                    for kind in item.types().iter() {
                        let bytes = item.dataForType(&kind).context("Pano tam olarak korunamıyor; test başlatılmadı")?;
                        let kind = kind.to_string();
                        total = total.checked_add(bytes.len()).and_then(|size| size.checked_add(kind.len()))
                            .filter(|size| *size <= 40 * 1024 * 1024)
                            .ok_or_else(|| anyhow::anyhow!("clipboard-snapshot-too-large"))?;
                        types.push(json!({"type":kind,"data":STANDARD.encode(bytes.to_vec())}));
                    }
                    saved.push(json!(types));
                }
            }
            let saved = json!(saved);
            // Leave room for response framing and any later revision number.
            check_restore_size(&saved, i64::MAX, MAX_TEST_REQUEST_BYTES - 1024)?;
            if board.changeCount() != revision { bail!("clipboard-revision-changed"); }
            return Ok(json!({"items":saved,"changeCount":revision}));
        }
        if request["action"] == "restore" {
            check_restore_size(&request["items"], i64::MAX, MAX_TEST_REQUEST_BYTES - 1024)?;
            let mut items: Vec<Retained<NSPasteboardItem>> = vec![];
            for item in request["items"].as_array().context("Pano verisi geçersiz")? {
                let target = NSPasteboardItem::new();
                for field in item.as_array().context("Pano verisi geçersiz")? {
                    let bytes = STANDARD.decode(string(field,"data")?)?;
                    if !target.setData_forType(&NSData::with_bytes(&bytes), &NSString::from_str(string(field,"type")?)) { bail!("Pano hazırlanamadı"); }
                }
                items.push(target);
            }
            let objects: Vec<&ProtocolObject<dyn NSPasteboardWriting>> = items.iter().map(|item| ProtocolObject::from_ref(&**item)).collect();
            check_test_revision(&request, board.changeCount() as i64)?;
            let revision = board.clearContents();
            modified_revision = Some(revision as i64);
            if !objects.is_empty() && !board.writeObjects(&NSArray::from_slice(&objects)) { bail!("Pano geri yüklenemedi"); }
            if board.pasteboardItems().map(|items| items.len()).unwrap_or(0) != items.len() { bail!("Pano öğelerinin tümü korunamadı"); }
            if board.changeCount() != revision { bail!("clipboard-revision-changed"); }
            return Ok(json!({"changeCount":revision}));
        }
        if request["action"] == "tiff" {
            let bytes = STANDARD.decode(string(&request,"png")?)?;
            let bitmap = NSBitmapImageRep::imageRepWithData(&NSData::with_bytes(&bytes)).context("Test görüntüsü okunamadı")?;
            let tiff = unsafe { bitmap.representationUsingType_properties(NSBitmapImageFileType::TIFF, &NSDictionary::new()) }.context("Test görüntüsü dönüştürülemedi")?;
            check_test_revision(&request, board.changeCount() as i64)?;
            let revision = board.clearContents();
            modified_revision = Some(revision as i64);
            if !board.setData_forType(Some(&tiff), unsafe { NSPasteboardTypeTIFF }) { bail!("Test panosu yazılamadı"); }
            if board.changeCount() != revision { bail!("clipboard-revision-changed"); }
            return Ok(json!({"changeCount":revision}));
        }
        bail!("Pano test işlemi geçersiz")
    });
    let response = match result {
        Ok(value) => json!({"ok":true,"value":value}),
        Err(error) => {
            let category = match error.to_string().as_str() {
                "clipboard-revision-changed" => "clipboard-revision-changed",
                "clipboard-snapshot-too-large" => "clipboard-snapshot-too-large",
                _ => "clipboard-operation-rejected",
            };
            json!({"ok":false,"error":category,"modifiedChangeCount":modified_revision})
        }
    };
    // Captured only by the test driver in memory, never written to a test artifact.
    writeln!(std::io::stdout(),"{response}")?;
    std::io::stdout().flush()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn restore_budget_counts_all_types_and_json_framing() {
        let items = json!([[{"type":"synthetic.utf8","data":"YWJj"}]]);
        let expected = serde_json::to_vec(&json!({"action":"restore","items":items,"expectedChangeCount":7})).unwrap().len() + 1;
        assert_eq!(check_restore_size(&items, 7, expected).unwrap(), expected);
        assert!(check_restore_size(&items, 7, expected - 1).is_err());
        let many = json!([[{"type":"synthetic.one","data":"a".repeat(200)}, {"type":"synthetic.two","data":"b".repeat(200)}]]);
        assert!(check_restore_size(&many, 7, 400).is_err());
        assert!(check_restore_size(&items, 7, MAX_TEST_REQUEST_BYTES - 1024).is_ok());
    }
    #[test]
    fn stale_or_missing_clipboard_revision_never_authorizes_mutation() {
        assert!(check_test_revision(&json!({"expectedChangeCount":8}), 8).is_ok());
        assert!(check_test_revision(&json!({"expectedChangeCount":7}), 8).is_err());
        assert!(check_test_revision(&json!({}), 8).is_err());
    }
}
