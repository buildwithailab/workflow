use crate::{
    browser::webview_cdp,
    core::{string, Core},
};
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};
use tauri::{webview::NewWindowResponse, Manager, WebviewUrl, WebviewWindow, WebviewWindowBuilder};
use tokio::time::{sleep, Instant};
use url::Url;

const WINDOW: &str = "teams-connection";
const HOME: &str = "https://teams.microsoft.com/v2/";
const PAGE: &str = include_str!("teams_web.js");
const TARGET_ERROR: &str = "Teams sohbeti doğrulanamadı.";
const LOGIN_ERROR: &str = "Önce Teams hesabına giriş yap.";
const VERIFY_ERROR: &str =
    "Teams gönderimi doğrulanamadı. Tekrar göndermeden önce sohbeti kontrol et.";
const SEND_TIMEOUT: Duration = Duration::from_secs(44);
pub(crate) const MCP_READ_TIMEOUT: Duration = Duration::from_secs(50);

#[derive(Default)]
pub struct Teams {
    operation: tokio::sync::Mutex<()>,
    hide_after_login: AtomicBool,
}

struct Target {
    id: String,
    url: Url,
}

fn teams_host(url: &Url) -> bool {
    url.scheme() == "https"
        && matches!(
            url.host_str(),
            Some("teams.microsoft.com" | "teams.cloud.microsoft")
        )
        && url.port().is_none()
        && url.username().is_empty()
        && url.password().is_none()
}

impl Target {
    fn parse(link: &str) -> Result<Self> {
        let error = "Geçerli bir Teams sohbet bağlantısı gir.";
        let link = Url::parse(link.trim())
            .ok()
            .filter(teams_host)
            .context(error)?;
        let path: Vec<_> = link.path_segments().context(error)?.collect();
        let raw_id = match path.as_slice() {
            ["l", "chat", id, "conversations" | "0"] | ["l", "message", id, _] => *id,
            _ => bail!(error),
        };
        if raw_id.contains(['&', '+']) {
            bail!(error);
        }
        let encoded = format!("id={raw_id}");
        let id = url::form_urlencoded::parse(encoded.as_bytes())
            .next()
            .context(error)?
            .1
            .into_owned();
        let valid_chat = id
            .strip_prefix("19:")
            .and_then(|id| id.split_once('@'))
            .is_some_and(|(name, suffix)| {
                !name.is_empty()
                    && name
                        .bytes()
                        .all(|c| c.is_ascii_alphanumeric() || b"-_.".contains(&c))
                    && matches!(suffix, "unq.gbl.spaces" | "thread.v2")
            });
        if id != "48:notes" && !valid_chat {
            bail!(error);
        }
        let mut url = Url::parse(HOME)?;
        url.path_segments_mut()
            .expect("Teams home is a hierarchical URL")
            .clear()
            .extend(["l", "chat", &id, "conversations"]);
        url.query_pairs_mut()
            .append_pair("context", r#"{"contextType":"chat"}"#);
        if let Some((_, tenant)) = link.query_pairs().find(|(key, _)| key == "tenantId") {
            let tenant = uuid::Uuid::parse_str(&tenant).context(error)?;
            url.query_pairs_mut()
                .append_pair("tenantId", &tenant.to_string());
        }
        Ok(Self { id, url })
    }
}

fn profile(core: &Core) -> std::path::PathBuf {
    core.directory.join("teams-profile")
}

async fn window(core: &Core) -> Result<WebviewWindow> {
    if let Some(window) = core.app.get_webview_window(WINDOW) {
        return Ok(window);
    }
    let main = core
        .app
        .get_webview_window("main")
        .context("Uygulama penceresi yok")?;
    let window =
        WebviewWindowBuilder::new(&core.app, WINDOW, WebviewUrl::External(Url::parse(HOME)?))
            .title("Workflow · Microsoft Teams")
            .owner(&main)?
            .inner_size(1050., 760.)
            .min_inner_size(600., 500.)
            .center()
            .visible(false)
            .skip_taskbar(true)
            .data_directory(profile(core))
            .on_navigation(|url| url.scheme() == "https" || url.as_str() == "about:blank")
            .on_new_window(|_, _| NewWindowResponse::Deny)
            .build()?;
    if let Err(error) = keep_in_webview(&window).await {
        let _ = window.destroy();
        return Err(error);
    }
    let app = core.app.clone();
    window.on_window_event(move |event| {
        if let tauri::WindowEvent::CloseRequested { api, .. } = event {
            api.prevent_close();
            if let Some(window) = app.get_webview_window(WINDOW) {
                let _ = window.hide();
            }
        }
    });
    Ok(window)
}

async fn keep_in_webview(window: &WebviewWindow) -> Result<()> {
    use webview2_com::{
        LaunchingExternalUriSchemeEventHandler, Microsoft::Web::WebView2::Win32::ICoreWebView2_18,
    };
    use windows::core::Interface;

    let (tx, rx) = tokio::sync::oneshot::channel();
    window.with_webview(move |view| {
        let result = (|| -> Result<()> {
            unsafe {
                let web = view
                    .controller()
                    .CoreWebView2()?
                    .cast::<ICoreWebView2_18>()?;
                let handler =
                    LaunchingExternalUriSchemeEventHandler::create(Box::new(|_, args| {
                        if let Some(args) = args {
                            args.SetCancel(true)?;
                        }
                        Ok(())
                    }));
                let mut token = 0;
                web.add_LaunchingExternalUriScheme(&handler, &mut token)?;
            }
            Ok(())
        })();
        let _ = tx.send(result);
    })?;
    tokio::time::timeout(Duration::from_secs(15), rx)
        .await?
        .context("Teams tarayıcısı kapandı.")?
}

pub fn close(app: &tauri::AppHandle) {
    if let Some(window) = app.get_webview_window(WINDOW) {
        let _ = window.destroy();
    }
}

async fn page(window: &WebviewWindow, action: &str, args: Value) -> Result<Value> {
    if !teams_host(&window.url()?) {
        return Ok(Value::Null);
    }
    let input = json!({"action": action, "args": args});
    let result = webview_cdp(
        window.as_ref(),
        "Runtime.evaluate",
        json!({
            "expression": format!("({PAGE})({input})"),
            "returnByValue": true,
            "awaitPromise": true,
            "userGesture": true
        }),
    )
    .await?;
    if !result["exceptionDetails"].is_null() {
        bail!("Teams sayfası işlemi tamamlayamadı.");
    }
    Ok(result["result"]["value"].clone())
}

async fn connection(window: &WebviewWindow, teams: &Teams) -> Result<Value> {
    let ready = page(window, "status", Value::Null)
        .await
        .unwrap_or(Value::Null)["ready"]
        == true;
    if ready && teams.hide_after_login.swap(false, Ordering::Relaxed) {
        window.hide()?;
    }
    let visible = window.is_visible()?;
    Ok(json!({
        "state": if ready { "connected" } else if visible { "signing-in" } else { "disconnected" },
        "visible": visible,
    }))
}

fn message(args: &Value) -> Result<&str> {
    let text = string(args, "text")?;
    if text.trim().is_empty() {
        bail!("Teams mesajını yaz.");
    }
    Ok(text)
}

fn authorized_target(saved: Option<&str>, link: Option<&str>) -> Result<Target> {
    let saved =
        Target::parse(saved.context("Oturum menüsünden bu oturumun Teams hedefini kaydet.")?)?;
    if let Some(link) = link {
        if Target::parse(link)?.url != saved.url {
            bail!("Teams bağlantısı bu oturum için kaydedilen hedefle eşleşmiyor.");
        }
    }
    Ok(saved)
}

async fn send(window: &WebviewWindow, target: Target, text: &str) -> Result<Value> {
    let mut focus_enabled = false;
    let result = tokio::time::timeout(
        SEND_TIMEOUT,
        send_in_background(window, target, text, &mut focus_enabled),
    )
    .await;
    if focus_enabled {
        let _ = tokio::time::timeout(
            Duration::from_secs(2),
            webview_cdp(
                window.as_ref(),
                "Emulation.setFocusEmulationEnabled",
                json!({"enabled": false}),
            ),
        )
        .await;
    }
    result.context(VERIFY_ERROR)?
}

async fn send_in_background(
    window: &WebviewWindow,
    target: Target,
    text: &str,
    focus_enabled: &mut bool,
) -> Result<Value> {
    let current = page(window, "status", Value::Null).await?;
    if current["ready"] != true {
        bail!(LOGIN_ERROR);
    }
    window.hide()?;
    let in_place = !target.url.query_pairs().any(|(key, _)| key == "tenantId")
        && page(window, "open-target", json!({"id":target.id}))
            .await
            .unwrap_or(Value::Null)
            == true;
    if !in_place {
        window.navigate(target.url)?;
    }
    let previous_document = if in_place {
        Value::Null
    } else {
        current["document"].clone()
    };
    let input = json!({"id": target.id, "previousDocument": previous_document});
    let deadline = Instant::now() + Duration::from_secs(25);
    loop {
        sleep(Duration::from_millis(350)).await;
        let result = page(window, "prepare", input.clone())
            .await
            .unwrap_or(Value::Null);
        if result["target"] == true {
            break;
        }
        if Instant::now() >= deadline {
            if !teams_host(&window.url()?) {
                bail!(LOGIN_ERROR);
            }
            bail!(TARGET_ERROR);
        }
    }

    *focus_enabled = true;
    webview_cdp(
        window.as_ref(),
        "Emulation.setFocusEmulationEnabled",
        json!({"enabled": true}),
    )
    .await?;
    let result = compose_and_submit(window, json!({"id": target.id, "text": text}))
        .await
        .context(VERIFY_ERROR)?;
    match result["state"].as_str() {
        Some("submitted") => Ok(json!({"submitted": true, "deliveryVerified": false})),
        Some("draft") => bail!("Teams sohbetinde gönderilmemiş bir taslak var."),
        Some("unsent") => bail!("Teams mesajı gönderilemedi. Taslağı Teams’te kontrol et."),
        Some("target") => bail!(TARGET_ERROR),
        _ => bail!(VERIFY_ERROR),
    }
}

async fn compose_and_submit(window: &WebviewWindow, input: Value) -> Result<Value> {
    let draft = page(window, "compose", input.clone()).await?;
    match draft["state"].as_str() {
        Some("composed") => {}
        Some("empty") => {
            webview_cdp(
                window.as_ref(),
                "Input.insertText",
                json!({"text": input["text"]}),
            )
            .await?;
        }
        _ => return Ok(draft),
    }
    page(window, "submit", input).await
}

pub async fn send_for_session(core: &Core, sid: &str, args: &Value) -> Result<Value> {
    let teams = core.app.state::<Teams>();
    let _operation = teams
        .operation
        .try_lock()
        .context("Teams şu anda başka bir işlem yürütüyor.")?;
    let session = core.session(sid)?;
    if session["archived"] == true || session["login"] == true || session["tools"] == false {
        bail!("Bu oturum Teams aracını kullanamaz.");
    }
    let link = args.get("link").map(|_| string(args, "link")).transpose()?;
    let target = authorized_target(session["teamsChatLink"].as_str(), link)?;
    let text = message(args)?;
    let window = core.app.get_webview_window(WINDOW).context(LOGIN_ERROR)?;
    send(&window, target, text).await
}

pub async fn dispatch(core: &Arc<Core>, action: &str, args: &Value) -> Result<Value> {
    let teams = core.app.state::<Teams>();
    let _operation = teams.operation.lock().await;
    match action {
        "status" if !profile(core).is_dir() => Ok(json!({"state":"disconnected","visible":false})),
        "status" => connection(&window(core).await?, &teams).await,
        "connect" => {
            let window = window(core).await?;
            let status = connection(&window, &teams).await?;
            if status["state"] == "connected" {
                return Ok(status);
            }
            teams.hide_after_login.store(true, Ordering::Relaxed);
            window.show()?;
            window.set_focus()?;
            connection(&window, &teams).await
        }
        "set_target" => {
            let sid = string(args, "sessionId")?;
            let link = string(args, "link")?.trim();
            let saved = if link.is_empty() {
                None
            } else {
                Some(Target::parse(link)?.url)
            };
            core.update_session(
                sid,
                json!({"teamsChatLink":saved.as_ref().map(Url::as_str)}),
            )
        }
        _ => bail!("Teams işlemi bulunamadı."),
    }
}
