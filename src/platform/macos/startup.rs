use crate::core::Core;
use std::sync::Arc;
use tauri::Manager;
use tauri_plugin_dialog::{DialogExt, MessageDialogKind};

struct StartupComplete;
struct StartupFailure(&'static str);

pub(crate) fn startup_completed(app: &tauri::AppHandle) -> bool {
    app.try_state::<StartupComplete>().is_some()
}

pub(crate) fn complete_startup(
    app: &tauri::App,
    result: std::result::Result<(), Box<dyn std::error::Error>>,
) {
    match result {
        Ok(()) => {
            app.manage(StartupComplete);
        }
        Err(error) => {
            app.manage(StartupFailure(startup_detail(error.as_ref())));
        }
    }
}

pub(crate) fn handle_startup_failure(app: &tauri::AppHandle) {
    let Some(failure) = app.try_state::<StartupFailure>() else { return; };
    let detail = failure.0;
    let app = app.clone();
    // Even creating a sheet can synchronously trigger AppKit drawing. Defer it
    // until the Tao event callback releases its non-reentrant event-handler lock.
    let operation = block2::RcBlock::new(move || present_startup_failure(&app, detail));
    // SAFETY: the block captures only a Send AppHandle and static text. The main
    // operation queue executes it on the AppKit thread and copies the block.
    unsafe { objc2_foundation::NSOperationQueue::mainQueue().addOperationWithBlock(&operation); }
}

fn present_startup_failure(app: &tauri::AppHandle, detail: &'static str) {
    if let Some(core) = app.try_state::<Arc<Core>>() {
        core.shutdown();
    }
    if let Some(window) = app.get_webview_window("main") {
        let dialog = app.dialog().message(detail).title("Workflow açılamadı").kind(MessageDialogKind::Error).parent(&window);
        let app = app.clone();
        dialog.show(move |_| app.exit(1));
    } else {
        // No webview exists for a sheet. This standalone alert is now outside
        // Tao's callback lock and can safely run its native modal loop.
        show_startup_error(detail);
        app.exit(1);
    }
}

pub(crate) fn startup_error(error: &dyn std::fmt::Display) {
    show_startup_error(startup_detail(error));
}

fn startup_detail(error: &dyn std::fmt::Display) -> &'static str {
    let message = error.to_string();
    if message.contains("zaten açık") {
        "Workflow bu veri klasörüyle zaten açık."
    } else if message.contains("özel") || message.contains("kullanıcıya ait") || message.contains("sembolik") {
        "Workflow verileri için kendi kullanıcı hesabına ait, ayrı ve özel bir klasör kullan."
    } else {
        "Uygulama paketi veya veri klasörü yüklenemedi. Klasör izinlerini ve kullanılabilir disk alanını kontrol et."
    }
}

fn show_startup_error(detail: &str) {
    let Some(mtm) = objc2::MainThreadMarker::new() else { return; };
    let alert = objc2_app_kit::NSAlert::new(mtm);
    alert.setMessageText(&objc2_foundation::NSString::from_str("Workflow açılamadı"));
    alert.setInformativeText(&objc2_foundation::NSString::from_str(detail));
    alert.window().setRestorable(false);
    alert.runModal();
}
