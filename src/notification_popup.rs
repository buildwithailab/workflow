use crate::{core::Core, notifications};
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::{
    path::PathBuf,
    sync::{mpsc, Arc, Mutex},
    thread::ThreadId,
    time::Duration,
};
use tauri::{
    webview::{Color, NewWindowResponse, PermissionResponse},
    Manager, WebviewUrl, WebviewWindow, WebviewWindowBuilder,
};

pub(crate) const LABEL: &str = "notification-popup";
const FINISHED_LIFETIME: Duration = Duration::from_secs(9);

#[derive(Clone)]
struct Card {
    id: String,
    generation: u64,
    acknowledged: bool,
}

struct Popup {
    ui_thread: ThreadId,
    generation: u64,
    current: Option<Card>,
    closing: bool,
}

fn local_page(url: &url::Url) -> bool {
    matches!(
        (url.scheme(), url.host_str()),
        ("tauri", Some("localhost")) | ("http" | "https", Some("tauri.localhost"))
    ) && url.port().is_none()
        && url.username().is_empty()
        && url.password().is_none()
        && url.path() == "/notification.html"
        && url.query().is_none()
        && url.fragment().is_none()
}

pub(crate) fn initialize(core: &Arc<Core>, webview_directory: PathBuf) -> Result<()> {
    core.app.manage(Mutex::new(Popup {
        ui_thread: std::thread::current().id(),
        generation: 0,
        current: None,
        closing: false,
    }));
    // Construct in Tauri's setup hook. A synchronous WebView2 constructor in a
    // later window event can block its own message loop on Windows.
    let window = WebviewWindowBuilder::new(
        &core.app,
        LABEL,
        WebviewUrl::App("notification.html".into()),
    )
    .title("Workflow")
    .inner_size(330., 156.)
    .resizable(false)
    .maximizable(false)
    .minimizable(false)
    .closable(false)
    .visible(false)
    .focused(false)
    .focusable(false)
    .always_on_top(true)
    .skip_taskbar(true)
    .decorations(false)
    .shadow(false)
    .background_color(Color(28, 34, 43, 255))
    .disable_drag_drop_handler()
    .data_directory(webview_directory)
    .on_navigation(local_page)
    .on_new_window(|_, _| NewWindowResponse::Deny)
    .on_permission_request(|_, _| PermissionResponse::Deny)
    .build()?;
    #[cfg(windows)]
    {
        use windows::Win32::{Foundation::HWND, UI::WindowsAndMessaging::*};
        let hwnd = HWND(window.hwnd()?.0);
        unsafe {
            let old = GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32;
            let style = (old | WS_EX_NOACTIVATE.0 | WS_EX_TOOLWINDOW.0) & !WS_EX_APPWINDOW.0;
            SetWindowLongPtrW(hwnd, GWL_EXSTYLE, style as isize);
            SetWindowPos(
                hwnd,
                None,
                0,
                0,
                0,
                0,
                SWP_FRAMECHANGED | SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
            )?;
        }
    }
    Ok(())
}

fn refresh(window: &WebviewWindow) -> Result<()> {
    window.eval("window.refreshWorkflowPopup?.()")?;
    Ok(())
}

fn bounds(left: i32, top: i32, width: u32, height: u32, scale: f64) -> (i32, i32, i32, i32) {
    let pixels = |logical: f64| (logical * scale).round() as i32;
    let w = (width as i32).min(pixels(330.)).max(1);
    let h = (height as i32).min(pixels(156.)).max(1);
    let margin = pixels(16.);
    (
        left + (width as i32 - w - margin).max(0),
        top + (height as i32 - h - margin).max(0),
        w,
        h,
    )
}

fn reveal(core: &Arc<Core>, card: &Card) -> Result<()> {
    let state = core.app.state::<Mutex<Popup>>();
    if !state
        .lock()
        .unwrap()
        .current
        .as_ref()
        .is_some_and(|c| c.generation == card.generation)
    {
        bail!("Bildirim artık güncel değil");
    }
    let event =
        notifications::popup_event(core, &card.id).context("Bildirim artık güncel değil")?;
    let window = core
        .app
        .get_webview_window(LABEL)
        .context("Bildirim penceresi yok")?;
    let monitor = window
        .cursor_position()
        .ok()
        .and_then(|p| window.monitor_from_point(p.x, p.y).ok().flatten())
        .or_else(|| {
            core.app
                .get_window("main")?
                .current_monitor()
                .ok()
                .flatten()
        })
        .or(window.primary_monitor()?)
        .context("Ekran bulunamadı")?;
    let area = monitor.work_area();
    let (x, y, w, h) = bounds(
        area.position.x,
        area.position.y,
        area.size.width,
        area.size.height,
        monitor.scale_factor(),
    );
    #[cfg(windows)]
    {
        use windows::Win32::{Foundation::HWND, UI::WindowsAndMessaging::*};
        // Position and reveal without activating. This top-level HWND has no
        // owner, so Windows leaves it visible when the main HWND is minimized.
        unsafe {
            SetWindowPos(
                HWND(window.hwnd()?.0),
                Some(HWND_TOPMOST),
                x,
                y,
                w,
                h,
                SWP_NOACTIVATE | SWP_SHOWWINDOW,
            )?;
        }
    }
    #[cfg(not(windows))]
    {
        window.set_position(tauri::PhysicalPosition::new(x, y))?;
        window.set_size(tauri::PhysicalSize::new(w as u32, h as u32))?;
        window.show()?;
    }
    refresh(&window)?;
    if event["kind"] == "completed" {
        let weak = Arc::downgrade(core);
        let generation = card.generation;
        tauri::async_runtime::spawn(async move {
            tokio::time::sleep(FINISHED_LIFETIME).await;
            if let Some(core) = weak.upgrade() {
                if let Some(expired) = take_if(&core, |c| c.generation == generation) {
                    hide_if_empty(&core);
                    let _ =
                        notifications::popup_action(&core, "dismiss", &json!({"id":expired.id}));
                }
            }
        });
    }
    Ok(())
}

pub(crate) fn present(core: &Arc<Core>, event_id: &str, kind: &str, _turkish: bool) -> Result<()> {
    if !matches!(kind, "approval" | "question" | "completed") {
        bail!("Geçersiz bildirim türü");
    }
    let checked =
        notifications::popup_event(core, event_id).context("Bildirim artık güncel değil")?;
    if checked["kind"] != kind {
        bail!("Bildirim artık güncel değil");
    }
    let state = core.app.state::<Mutex<Popup>>();
    let (card, on_ui_thread) = {
        let mut state = state.lock().unwrap();
        if state.closing {
            bail!("Pencere kapanıyor");
        }
        if let Some(current) = state.current.as_ref().filter(|c| c.id == event_id) {
            if current.acknowledged {
                return Ok(());
            }
            drop(state);
            return refresh(
                &core
                    .app
                    .get_webview_window(LABEL)
                    .context("Bildirim penceresi yok")?,
            );
        }
        state.generation += 1;
        let card = Card {
            id: event_id.into(),
            generation: state.generation,
            acknowledged: false,
        };
        state.current = Some(card.clone());
        (card, state.ui_thread == std::thread::current().id())
    };
    let result = if on_ui_thread {
        reveal(core, &card)
    } else {
        let (sender, receiver) = mpsc::sync_channel(1);
        let owner = core.clone();
        let candidate = card.clone();
        core.app
            .run_on_main_thread(move || {
                let _ = sender.send(reveal(&owner, &candidate));
            })
            .map_err(anyhow::Error::from)
            .and_then(|_| {
                receiver
                    .recv_timeout(Duration::from_secs(4))
                    .context("Bildirim penceresi yanıt vermedi")?
            })
    };
    if result.is_err() && take_if(core, |c| c.generation == card.generation).is_some() {
        hide_if_empty(core);
    }
    result
}

fn take_if(core: &Core, matches: impl FnOnce(&Card) -> bool) -> Option<Card> {
    let state = core.app.try_state::<Mutex<Popup>>()?;
    let mut state = state.lock().unwrap();
    if state.current.as_ref().is_some_and(matches) {
        state.current.take()
    } else {
        None
    }
}

fn hide_if_empty(core: &Core) {
    let app = core.app.clone();
    let _ = core.app.run_on_main_thread(move || {
        if app
            .state::<Mutex<Popup>>()
            .lock()
            .unwrap()
            .current
            .is_none()
        {
            if let Some(window) = app.get_webview_window(LABEL) {
                #[cfg(windows)]
                if let Ok(hwnd) = window.hwnd() {
                    use windows::Win32::{Foundation::HWND, UI::WindowsAndMessaging::*};
                    // Its visibility is owned by our nonactivating native calls,
                    // so Tao's cached VISIBLE flag remains false while shown.
                    let _ = unsafe {
                        SetWindowPos(
                            HWND(hwnd.0),
                            None,
                            0,
                            0,
                            0,
                            0,
                            SWP_HIDEWINDOW
                                | SWP_NOACTIVATE
                                | SWP_NOSIZE
                                | SWP_NOMOVE
                                | SWP_NOZORDER,
                        )
                    };
                }
                #[cfg(not(windows))]
                let _ = window.hide();
            }
        }
    });
}

pub(crate) fn hide(core: &Core) {
    if take_if(core, |_| true).is_some() {
        hide_if_empty(core);
    }
}

pub(crate) fn shutdown(core: &Core) {
    if let Some(state) = core.app.try_state::<Mutex<Popup>>() {
        let mut state = state.lock().unwrap();
        if state.closing {
            return;
        }
        state.closing = true;
        state.current = None;
    }
    if let Some(window) = core.app.get_webview_window(LABEL) {
        let _ = window.destroy();
    }
}

pub(crate) fn dispatch(core: &Arc<Core>, action: &str, args: &Value) -> Result<Value> {
    if action == "read" {
        let state = core.app.state::<Mutex<Popup>>();
        let card = state.lock().unwrap().current.clone();
        return Ok(card
            .filter(|c| args["id"].is_null() || args["id"] == c.id)
            .and_then(|c| notifications::popup_event(core, &c.id))
            .map(|e| json!({"id":e["id"],"kind":e["kind"],"turkish":e["turkish"]}))
            .unwrap_or(Value::Null));
    }
    let id = args["id"].as_str().context("Geçersiz bildirim")?;
    if action == "presented" {
        let state = core.app.state::<Mutex<Popup>>();
        if !state
            .lock()
            .unwrap()
            .current
            .as_ref()
            .is_some_and(|c| c.id == id)
        {
            bail!("Bildirim artık güncel değil");
        }
        let outcome = notifications::popup_action(core, action, &json!({"id":id}))?;
        if outcome == true {
            if let Some(card) = state
                .lock()
                .unwrap()
                .current
                .as_mut()
                .filter(|c| c.id == id)
            {
                card.acknowledged = true;
            }
        }
        return Ok(outcome);
    }
    if !matches!(action, "open" | "dismiss") {
        bail!("Geçersiz bildirim işlemi");
    }
    take_if(core, |c| c.id == id).context("Bildirim artık güncel değil")?;
    hide_if_empty(core);
    notifications::popup_action(core, action, &json!({"id":id}))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn popup_anchors_to_the_scaled_work_area_on_negative_coordinates() {
        assert_eq!(bounds(-1920, -100, 1920, 1040, 1.25), (-433, 725, 413, 195));
        assert_eq!(bounds(10, 20, 210, 130, 1.), (10, 20, 210, 130));
    }
    #[test]
    fn popup_stays_on_its_own_local_page() {
        assert!(local_page(
            &"http://tauri.localhost/notification.html".parse().unwrap()
        ));
        for other in [
            "http://tauri.localhost/index.html",
            "https://example.invalid/notification.html",
            "http://tauri.localhost:3000/notification.html",
            "https://tauri.localhost/notification.html?route=x",
        ] {
            assert!(!local_page(&other.parse().unwrap()));
        }
    }
}
