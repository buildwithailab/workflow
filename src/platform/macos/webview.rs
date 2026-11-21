//! Public WKWebView/AppKit APIs; no WebKit inspector or private CDP endpoint.
use anyhow::{bail, Context, Result};
use block2::RcBlock;
use objc2::{rc::autoreleasepool, runtime::AnyObject, ClassType, MainThreadMarker};
use objc2_app_kit::{NSBitmapImageFileType, NSBitmapImageRep, NSEvent, NSEventModifierFlags as Flags, NSEventType, NSImage};
use objc2_foundation::{NSDictionary, NSError, NSPoint, NSRect, NSSize, NSString, NSRange, NSNotFound};
use objc2_web_kit::{WKContentWorld, WKSnapshotConfiguration, WKWebView};
use serde_json::{json, Value};
use std::{sync::{Arc, Mutex}, time::Duration};

pub(crate) async fn evaluate(view: &tauri::Webview, script: &str) -> Result<Value> {
    let (send, receive) = tokio::sync::oneshot::channel();
    let send = Arc::new(Mutex::new(Some(send)));
    let body = format!("return JSON.stringify(await ({script}));");
    view.with_webview(move |platform| unsafe {
        let web = &*platform.inner().cast::<WKWebView>();
        // An evaluation in a document being replaced can lose its completion
        // callback in WebKit. Let callers retry after navigation finishes.
        if web.isLoading() {
            if let Some(send)=send.lock().unwrap().take(){let _=send.send(Err(anyhow::anyhow!("Sayfa yükleniyor; yeniden oku")));}
            return;
        }
        let callback = RcBlock::new(move |object: *mut AnyObject, error: *mut NSError| {
            let result = autoreleasepool(|_| {
                if !error.is_null() { bail!("WebKit sayfa işlemi tamamlanamadı"); }
                if object.is_null() { return Ok(Value::Null); }
                let is_string: bool = objc2::msg_send![object, isKindOfClass: NSString::class()];
                if !is_string { bail!("WebKit yanıt biçimi geçersiz"); }
                let text = &*object.cast::<NSString>();
                Ok(serde_json::from_str(&text.to_string())?)
            });
            if let Some(send) = send.lock().unwrap().take() { let _ = send.send(result); }
        });
        web.callAsyncJavaScript_arguments_inFrame_inContentWorld_completionHandler(
            &NSString::from_str(&body), None, None, &WKContentWorld::pageWorld(MainThreadMarker::new().expect("Tauri main thread")), Some(&callback));
    })?;
    tokio::time::timeout(Duration::from_secs(15), receive).await.context("WebKit yanıtı gecikti")?.context("Tarayıcı kapandı")?
}

pub(crate) async fn navigate(view: &tauri::Webview, action: &str) -> Result<Value> {
    let action = action.to_owned();
    let (send, receive) = tokio::sync::oneshot::channel();
    view.with_webview(move |platform| unsafe {
        let web = &*platform.inner().cast::<WKWebView>();
        match action.as_str() { "back" => { web.goBack(); }, "forward" => { web.goForward(); }, _ => { web.reload(); } }
        let _ = send.send(json!(true));
    })?;
    Ok(receive.await?)
}

pub(crate) async fn screenshot(view: &tauri::Webview, clip: Option<Value>) -> Result<Value> {
    use base64::{engine::general_purpose::STANDARD, Engine};
    let (send, receive) = tokio::sync::oneshot::channel();
    let send = Arc::new(Mutex::new(Some(send)));
    view.with_webview(move |platform| unsafe {
        let web = &*platform.inner().cast::<WKWebView>();
        let config = WKSnapshotConfiguration::new(MainThreadMarker::new().expect("Tauri main thread"));
        let bounds=web.bounds();
        let scale=web.window().map(|window|window.backingScaleFactor()).unwrap_or(2.);
        if !scale.is_finite() || scale<=0. || !crate::chat::valid_screen_dimensions((bounds.size.width*scale) as u32,(bounds.size.height*scale) as u32) {
            if let Some(send)=send.lock().unwrap().take(){let _=send.send(Err(anyhow::anyhow!("Tarayıcı görüntüsü çok büyük")));}return;
        }
        if let Some(clip) = clip {
            let values=["x","y","width","height"].map(|key|clip[key].as_f64().filter(|value|value.is_finite()));
            let [Some(x),Some(y),Some(width),Some(height)]=values else {
                if let Some(send)=send.lock().unwrap().take(){let _=send.send(Err(anyhow::anyhow!("Görüntü alanı geçersiz")));}return;
            };
            let x=x.max(0.);let y=y.max(0.);
            let width=width.min(bounds.size.width-x);let height=height.min(bounds.size.height-y);
            if width<=0. || height<=0. {
                if let Some(send)=send.lock().unwrap().take(){let _=send.send(Err(anyhow::anyhow!("Öğe görünür alanda değil")));}return;
            }
            config.setRect(NSRect::new(NSPoint::new(x,y),NSSize::new(width,height)));
        }
        let callback = RcBlock::new(move |image: *mut NSImage, error: *mut NSError| {
            let result = autoreleasepool(|_| -> Result<Value> {
                if !error.is_null() || image.is_null() { bail!("Tarayıcı görüntüsü alınamadı"); }
                let data = (&*image).TIFFRepresentation().context("Tarayıcı görüntüsü boş")?;
                let bitmap = NSBitmapImageRep::imageRepWithData(&data).context("Tarayıcı görüntüsü okunamadı")?;
                let png = bitmap.representationUsingType_properties(NSBitmapImageFileType::PNG, &NSDictionary::new()).context("Tarayıcı görüntüsü dönüştürülemedi")?;
                if png.len()>crate::chat::MAX_SCREENSHOT_BYTES {bail!("Tarayıcı görüntüsü çok büyük");}
                Ok(json!({"data":STANDARD.encode(png.to_vec())}))
            });
            if let Some(send) = send.lock().unwrap().take() { let _ = send.send(result); }
        });
        web.takeSnapshotWithConfiguration_completionHandler(Some(&config), &callback);
    })?;
    tokio::time::timeout(Duration::from_secs(15), receive).await.context("Tarayıcı görüntüsü gecikti")?.context("Tarayıcı kapandı")?
}

fn flags(bits: u64) -> Flags {
    let mut flags = Flags::empty();
    for (mask, flag) in [(1, Flags::Option), (2, Flags::Control), (4, Flags::Command), (8, Flags::Shift)] {
        if bits & mask != 0 { flags |= flag; }
    }
    flags
}

fn key(value: &str) -> (String, u16) {
    let (text, code) = match value {
        "Enter" => ("\r", 36), "Tab" => ("\t", 48), "Escape" => ("\x1b", 53), "Backspace" => ("\x7f", 51),
        "Delete" => ("\u{f728}", 117), "ArrowLeft" => ("\u{f702}",123), "ArrowRight" => ("\u{f703}",124),
        "ArrowDown" => ("\u{f701}",125), "ArrowUp" => ("\u{f700}",126), "Home" => ("\u{f729}",115),
        "End" => ("\u{f72b}",119), "PageUp" => ("\u{f72c}",116), "PageDown" => ("\u{f72d}",121), _ => (value, 0),
    };
    (text.to_owned(), code)
}

pub(crate) async fn input(view: &tauri::Webview, method: &str, args: Value) -> Result<Value> {
    if method == "Input.dispatchMouseEvent" && args["type"] == "mouseWheel" {
        return evaluate(view, &format!("(()=>{{const e=document.elementFromPoint({},{});let w=e;while(w&&w!==document.body&&w.scrollHeight<=w.clientHeight)w=w.parentElement;(w||document.scrollingElement).scrollBy({},{});return true}})()",
            args["x"],args["y"],args["deltaX"],args["deltaY"])).await;
    }
    let method = method.to_owned();
    let (send, receive) = tokio::sync::oneshot::channel();
    view.with_webview(move |platform| unsafe {
        let result = autoreleasepool(|_| -> Result<Value> {
            let web = &*platform.inner().cast::<WKWebView>();
            let window = web.window().context("Tarayıcı penceresi kapalı")?;
            let modifiers = flags(args["modifiers"].as_u64().unwrap_or(0));
            if method == "Input.dispatchMouseEvent" {
                let x = args["x"].as_f64().context("X gerekli")?;
                let y = args["y"].as_f64().context("Y gerekli")?;
                if !x.is_finite() || !y.is_finite() || x < 0. || y < 0. { bail!("Koordinat geçersiz"); }
                let point = web.convertPoint_toView(NSPoint::new(x, if web.isFlipped() { y } else { web.bounds().size.height - y }), None);
                let right = args["button"] == "right";
                let kind = match args["type"].as_str() {
                    Some("mousePressed") => if right { NSEventType::RightMouseDown } else { NSEventType::LeftMouseDown },
                    Some("mouseReleased") => if right { NSEventType::RightMouseUp } else { NSEventType::LeftMouseUp },
                    Some("mouseMoved") => if args["buttons"] == 1 { NSEventType::LeftMouseDragged } else { NSEventType::MouseMoved },
                    _ => bail!("Fare işlemi desteklenmiyor"),
                };
                let event = NSEvent::mouseEventWithType_location_modifierFlags_timestamp_windowNumber_context_eventNumber_clickCount_pressure(
                    kind, point, modifiers, 0., window.windowNumber(), None, 0, args["clickCount"].as_i64().unwrap_or(1) as isize, 1.).context("Fare olayı oluşturulamadı")?;
                match kind {
                    NSEventType::LeftMouseDown => web.mouseDown(&event), NSEventType::LeftMouseUp => web.mouseUp(&event),
                    NSEventType::RightMouseDown => web.rightMouseDown(&event), NSEventType::RightMouseUp => web.rightMouseUp(&event),
                    NSEventType::LeftMouseDragged => web.mouseDragged(&event), _ => web.mouseMoved(&event),
                }
            } else {
                window.makeFirstResponder(Some(web));
                if method == "Input.insertText" {
                    let text = args["text"].as_str().context("Metin gerekli")?;
                    let responder = window.firstResponder().context("Tarayıcı metin alanı odaklanamadı")?;
                    let accepts: bool = objc2::msg_send![&*responder, respondsToSelector: objc2::sel!(insertText:replacementRange:)];
                    if !accepts { bail!("Tarayıcı metin alanına yazılamadı"); }
                    let _: () = objc2::msg_send![&*responder, insertText: &*NSString::from_str(text), replacementRange: NSRange::new(NSNotFound as usize, 0)];
                    return Ok(json!({}));
                }
                let (text, code) = key(args["key"].as_str().context("Tuş gerekli")?);
                let down = args["type"] != "keyUp";
                let event = NSEvent::keyEventWithType_location_modifierFlags_timestamp_windowNumber_context_characters_charactersIgnoringModifiers_isARepeat_keyCode(
                    if down {NSEventType::KeyDown} else {NSEventType::KeyUp}, NSPoint::new(0.,0.), modifiers, 0., window.windowNumber(), None,
                    &NSString::from_str(&text), &NSString::from_str(&text), false, code).context("Klavye olayı oluşturulamadı")?;
                if down { web.keyDown(&event); } else { web.keyUp(&event); }
            }
            Ok(json!({}))
        });
        let _ = send.send(result);
    })?;
    receive.await?
}

pub(crate) async fn protocol(view: &tauri::Webview, method: &str, args: Value) -> Result<Value> {
    match method {
        "Runtime.evaluate" => Ok(json!({"result":{"value":evaluate(view, args["expression"].as_str().context("Sayfa işlemi boş")?).await?}})),
        "Page.navigate" => { view.navigate(crate::browser::url(args["url"].as_str().context("Adres gerekli")?)?)?; Ok(json!({})) },
        "Page.reload" => navigate(view,"reload").await,
        "Page.captureScreenshot" => screenshot(view, args.get("clip").cloned()).await,
        "Input.dispatchMouseEvent" | "Input.dispatchKeyEvent" | "Input.insertText" => input(view, method, args).await,
        _ => bail!("WKWebView işlemi desteklenmiyor: {method}"),
    }
}
