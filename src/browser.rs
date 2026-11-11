use crate::core::*;
use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::{HashMap, VecDeque},
    fs,
    io::{Read, Write},
    net::TcpStream,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tauri::{LogicalPosition, LogicalSize, Manager, WebviewUrl};
use tungstenite::{stream::MaybeTlsStream, Message, WebSocket};

pub struct Remote {
    socket: WebSocket<MaybeTlsStream<TcpStream>>,
    sequence: u64,
}
impl Remote {
    fn call(&mut self, method: &str, params: Value) -> Result<Value> {
        self.sequence += 1;
        let id = self.sequence;
        self.socket.send(Message::Text(
            json!({"id":id,"method":method,"params":params})
                .to_string()
                .into(),
        ))?;
        loop {
            let message = self.socket.read()?;
            if let Message::Text(text) = message {
                let v: Value = serde_json::from_str(text.as_str())?;
                if v["id"] == id {
                    if v["error"].is_object() {
                        bail!("Chrome: {}", v["error"])
                    }
                    return Ok(v["result"].clone());
                }
            }
        }
    }
}
#[derive(Clone)]
pub struct Page {
    pub id: String,
    pub session: String,
    pub label: String,
    pub mode: String,
    pub url: String,
    pub title: String,
    pub picked: Option<Value>,
    pub node: Option<i64>,
    pub picking: bool,
    pub pick_epoch: u64,
    pub poll_gate: Arc<tokio::sync::Mutex<()>>,
    pub revision: u64,
    pub remote: Option<Arc<Mutex<Remote>>>,
}
impl Page {
    pub fn public(&self) -> Value {
        json!({"id":self.id,"sessionId":self.session,"mode":self.mode,"url":self.url,"title":self.title,"picked":self.picked,"picking":self.picking,"revision":self.revision})
    }
}
#[derive(Default)]
pub struct BrowserState {
    pub pages: HashMap<String, Page>,
    pub active: HashMap<String, String>,
    pub chrome: HashMap<String, String>,
    visible: Option<String>,
    traces: HashMap<String, TraceHistory>,
}
#[derive(Default, Deserialize, Serialize)]
struct TraceHistory {
    steps: VecDeque<TraceStep>,
    passed: u64,
    failed: u64,
    total: u64,
}
impl TraceHistory {
    fn record(&mut self, step: TraceStep) {
        self.total += 1;
        if step.details["kind"] == "assert" {
            if step.details["passed"] == true {
                self.passed += 1;
            } else {
                self.failed += 1;
            }
        }
        self.steps.push_back(step);
        while self.steps.len() > 100 {
            self.steps.pop_front();
        }
        for step in self
            .steps
            .iter_mut()
            .rev()
            .filter(|step| step.image.is_some())
            .skip(4)
        {
            step.image = None;
        }
    }
}
#[derive(Deserialize, Serialize)]
struct TraceStep {
    details: Value,
    #[serde(skip)]
    image: Option<String>,
}
impl TraceStep {
    fn public(&self) -> Value {
        let mut details = self.details.clone();
        if let Some(fields) = details.as_object_mut() {
            fields.insert("hasImage".into(), json!(self.image.is_some()));
        }
        details
    }
}
fn trace_path(core: &Core, sid: &str) -> PathBuf {
    core.directory
        .join("browser-traces")
        .join(format!("{}.json", hash(sid.as_bytes())))
}
fn trace_history<'a>(
    core: &Core,
    sid: &str,
    browser: &'a mut BrowserState,
) -> Result<&'a mut TraceHistory> {
    use std::collections::hash_map::Entry;
    Ok(match browser.traces.entry(sid.to_owned()) {
        Entry::Occupied(entry) => entry.into_mut(),
        Entry::Vacant(entry) => {
            let path = trace_path(core, sid);
            let bytes = match fs::read(&path) {
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    fs::read(path.with_extension("previous"))
                }
                result => result,
            };
            let history = match bytes {
                Ok(bytes) => serde_json::from_slice(&bytes).context("Tarayıcı kaydı okunamadı")?,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => TraceHistory::default(),
                Err(e) => return Err(e.into()),
            };
            entry.insert(history)
        }
    })
}
fn save_trace(core: &Core, sid: &str, history: &TraceHistory) -> Result<()> {
    atomic(&trace_path(core, sid), &serde_json::to_vec(history)?)
}
impl BrowserState {
    pub fn close(&mut self) {
        for p in self.pages.values() {
            if let Some(r) = &p.remote {
                let _ = r.lock().unwrap().socket.close(None);
            }
        }
    }
}
pub fn url(value: &str) -> Result<url::Url> {
    let raw = value.trim();
    let normalized = if raw.contains("://") {
        raw.to_string()
    } else {
        format!("http://{raw}")
    };
    let u = url::Url::parse(&normalized)?;
    if !["http", "https"].contains(&u.scheme())
        || !u.username().is_empty()
        || u.password().is_some()
    {
        bail!("HTTP veya HTTPS web adresi gir")
    };
    Ok(u)
}
fn get(core: &Core, args: &Value) -> Result<Page> {
    let id = if let Some(id) = args["pageId"].as_str() {
        id.to_string()
    } else {
        core.browsers
            .lock()
            .unwrap()
            .active
            .get(string(args, "sessionId")?)
            .cloned()
            .context("Önce bir tarayıcı sekmesi aç")?
    };
    let b = core.browsers.lock().unwrap();
    let p = b.pages.get(&id).context("Tarayıcı sekmesi kapalı")?;
    if args["sessionId"].as_str().is_some_and(|s| s != p.session) {
        bail!("Sekme başka bir oturuma ait")
    };
    Ok(p.clone())
}
fn changed(core: &Core, p: &Page) {
    core.emit("browser", p.public());
}
async fn cdp(core: &Arc<Core>, p: &Page, method: &str, params: Value) -> Result<Value> {
    if let Some(remote) = p.remote.clone() {
        let method = method.to_string();
        return tauri::async_runtime::spawn_blocking(move || {
            remote.lock().unwrap().call(&method, params)
        })
        .await?;
    }
    let w = core
        .app
        .get_webview(&p.label)
        .context("Tarayıcı görünümü kapalı")?;
    #[cfg(windows)]
    {
        use webview2_com::CallDevToolsProtocolMethodCompletedHandler;
        use windows::core::PCWSTR;
        let (tx, rx) = tokio::sync::oneshot::channel::<Result<Value>>();
        let tx = Arc::new(Mutex::new(Some(tx)));
        let method = method.to_string();
        let params = params.to_string();
        w.with_webview(move |platform| {
            let result = (|| -> Result<()> {
                unsafe {
                    let web = platform.controller().CoreWebView2()?;
                    let m = method.encode_utf16().chain(Some(0)).collect::<Vec<_>>();
                    let a = params.encode_utf16().chain(Some(0)).collect::<Vec<_>>();
                    let t = tx.clone();
                    let callback = CallDevToolsProtocolMethodCompletedHandler::create(Box::new(
                        move |error, result| {
                            let value = error
                                .map_err(|w| anyhow!(w))
                                .and_then(|_| serde_json::from_str(&result).map_err(Into::into));
                            if let Some(tx) = t.lock().unwrap().take() {
                                let _ = tx.send(value);
                            }
                            Ok(())
                        },
                    ));
                    web.CallDevToolsProtocolMethod(
                        PCWSTR(m.as_ptr()),
                        PCWSTR(a.as_ptr()),
                        &callback,
                    )?;
                }
                Ok(())
            })();
            if let Err(w) = result {
                if let Some(tx) = tx.lock().unwrap().take() {
                    let _ = tx.send(Err(w));
                }
            }
        })?;
        tokio::time::timeout(Duration::from_secs(15), rx)
            .await
            .context("Tarayıcı yanıtı gecikti")?
            .context("Tarayıcı kapandı")?
    }
    #[cfg(not(windows))]
    {
        let _ = (w, method, params);
        bail!("Bu platformda otomasyon için Chrome modunu kullan")
    }
}
async fn eval(core: &Arc<Core>, p: &Page, script: &str) -> Result<Value> {
    let r = cdp(
        core,
        p,
        "Runtime.evaluate",
        json!({"expression":script,"returnByValue":true,"awaitPromise":true,"userGesture":true}),
    )
    .await?;
    if !r["exceptionDetails"].is_null() {
        bail!("Sayfa işlemi başarısız: {}", r["exceptionDetails"]["text"])
    }
    Ok(r["result"]["value"].clone())
}
#[cfg(windows)]
fn install_events(core: &Arc<Core>, p: &Page) -> Result<()> {
    use webview2_com::DevToolsProtocolEventReceivedEventHandler;
    use windows::core::PCWSTR;
    let c = core.clone();
    let pid = p.id.clone();
    let w = core.app.get_webview(&p.label).context("Tarayıcı kapalı")?;
    w.with_webview(move |platform| unsafe {
        let Ok(web) = platform.controller().CoreWebView2() else {
            return;
        };
        for event in [
            "Overlay.inspectNodeRequested",
            "Overlay.inspectModeCanceled",
            "Page.frameNavigated",
        ] {
            let event_name = event.to_string();
            let wide = event.encode_utf16().chain(Some(0)).collect::<Vec<_>>();
            let Ok(receiver) = web.GetDevToolsProtocolEventReceiver(PCWSTR(wide.as_ptr())) else {
                continue;
            };
            let c = c.clone();
            let pid = pid.clone();
            let callback =
                DevToolsProtocolEventReceivedEventHandler::create(Box::new(move |_, args| {
                    if let Some(args) = args {
                        let mut raw = windows::core::PWSTR::null();
                        args.ParameterObjectAsJson(&mut raw)?;
                        let text = webview2_com::CoTaskMemPWSTR::from(raw).to_string();
                        if let Ok(v) = serde_json::from_str::<Value>(&text) {
                            let mut b = c.browsers.lock().unwrap();
                            if let Some(p) = b.pages.get_mut(&pid) {
                                if event_name.starts_with("Overlay") {
                                    p.node = v["backendNodeId"].as_i64();
                                    p.picking = false;
                                    p.pick_epoch += 1;
                                } else if v["frame"]["parentId"].is_null() {
                                    p.revision += 1;
                                    p.picking = false;
                                    p.pick_epoch += 1;
                                    p.picked = None;
                                    p.node = None;
                                    p.url = v["frame"]["url"].as_str().unwrap_or("").to_string();
                                }
                                changed(&c, p);
                            }
                        }
                    }
                    Ok(())
                }));
            let mut token = 0i64;
            let _ = receiver.add_DevToolsProtocolEventReceived(&callback, &mut token);
        }
    })?;
    Ok(())
}
async fn native_page(core: &Arc<Core>, sid: &str, address: url::Url) -> Result<Page> {
    let pid = id();
    let label = format!("page-{pid}");
    let p = Page {
        id: pid.clone(),
        session: sid.into(),
        label: label.clone(),
        mode: "embedded".into(),
        url: address.to_string(),
        title: address.host_str().unwrap_or("Sayfa").into(),
        picked: None,
        node: None,
        picking: false,
        pick_epoch: 0,
        poll_gate: Arc::new(tokio::sync::Mutex::new(())),
        revision: 0,
        remote: None,
    };
    let (window, app) = (
        core.app
            .get_window("main")
            .context("Uygulama penceresi yok")?,
        core.app.clone(),
    );
    let directory = core.directory.join("browser-profiles").join(sid);
    fs::create_dir_all(&directory)?;
    let builder=tauri::webview::WebviewBuilder::new(&label,WebviewUrl::External(address)).data_directory(directory).initialization_script("if(/json|text\\/plain/.test(document.contentType)){addEventListener('DOMContentLoaded',()=>{document.documentElement.style.background='#f8fafc';document.documentElement.style.color='#17202a';document.body.style.color='#17202a';});}");
    let (tx, rx) = tokio::sync::oneshot::channel();
    core.app.run_on_main_thread(move || {
        let r = window
            .add_child(
                builder,
                LogicalPosition::new(0., 0.),
                LogicalSize::new(900., 650.),
            )
            .map_err(|w| anyhow!(w));
        if let Ok(w) = &r {
            let _ = w.hide();
        }
        let _ = tx.send(r.map(|_| ()));
        let _ = app;
    })?;
    rx.await??;
    core.browsers.lock().unwrap().pages.insert(pid, p.clone());
    install_events(core, &p)?;
    cdp(core, &p, "Page.enable", json!({})).await?;
    Ok(p)
}
fn http_json(port: u16, path: &str) -> Result<Value> {
    use std::io::{BufRead, BufReader};
    let mut s = TcpStream::connect(("127.0.0.1", port))?;
    s.set_read_timeout(Some(Duration::from_secs(3)))?;
    write!(
        s,
        "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"
    )?;
    let mut reader = BufReader::new(s);
    let mut status = String::new();
    reader.read_line(&mut status)?;
    if !status.contains(" 200 ") {
        bail!("Chrome yerel bağlantısı yanıt vermedi")
    }
    let mut length = None;
    let mut header_size = 0;
    loop {
        let mut line = String::new();
        reader.read_line(&mut line)?;
        header_size += line.len();
        if header_size > 16384 {
            bail!("Chrome başlık sınırı aşıldı")
        };
        if line == "\r\n" || line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            if name.eq_ignore_ascii_case("Content-Length") {
                length = value.trim().parse::<usize>().ok();
            }
        }
    }
    let length = length
        .filter(|n| *n <= 2 * 1024 * 1024)
        .context("Chrome yanıt uzunluğu geçersiz")?;
    let mut body = vec![0u8; length];
    reader.read_exact(&mut body)?;
    Ok(serde_json::from_slice(&body)?)
}
fn chrome_exe() -> Option<PathBuf> {
    [
        PathBuf::from("C:/Program Files/Google/Chrome/Application/chrome.exe"),
        PathBuf::from("C:/Program Files (x86)/Google/Chrome/Application/chrome.exe"),
        home().join("AppData/Local/Google/Chrome/Application/chrome.exe"),
    ]
    .into_iter()
    .find(|p| p.is_file())
}
async fn chrome_page(core: &Arc<Core>, sid: &str, address: url::Url) -> Result<Page> {
    let c = core.clone();
    let sid = sid.to_string();
    let address = address.to_string();
    tauri::async_runtime::spawn_blocking(move || {
        let dir = c.directory.join("chrome-profiles").join(&sid);
        fs::create_dir_all(&dir)?;
        let active = dir.join("DevToolsActivePort");
        let existing = || -> Result<u16> {
            let text = fs::read_to_string(&active)?;
            let port = text
                .lines()
                .next()
                .context("Chrome portu yok")?
                .parse::<u16>()?;
            http_json(port, "/json/version")?;
            Ok(port)
        };
        let port = match existing() {
            Ok(p) => p,
            Err(_) => {
                let _ = fs::remove_file(&active);
                std::process::Command::new(chrome_exe().context("Google Chrome kurulu değil")?)
                    .args([
                        "--remote-debugging-address=127.0.0.1",
                        "--remote-debugging-port=0",
                        "--no-first-run",
                        "--no-default-browser-check",
                        &format!("--user-data-dir={}", dir.display()),
                        "about:blank",
                    ])
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .spawn()?;
                let mut port = None;
                for _ in 0..100 {
                    std::thread::sleep(Duration::from_millis(150));
                    if let Ok(p) = existing() {
                        port = Some(p);
                        break;
                    }
                }
                port.context("Chrome bağlantısı kurulamadı")?
            }
        };
        let info = http_json(port, "/json/version")?;
        let socket_url = string(&info, "webSocketDebuggerUrl")?;
        let u = url::Url::parse(socket_url)?;
        if u.scheme() != "ws" || u.host_str() != Some("127.0.0.1") || u.port() != Some(port) {
            bail!("Chrome bağlantısı yerel değil")
        }
        let (mut socket, _) = tungstenite::connect(socket_url)?;
        if let MaybeTlsStream::Plain(stream) = socket.get_mut() {
            stream.set_read_timeout(Some(Duration::from_secs(12)))?;
        }
        let mut browser = Remote {
            socket,
            sequence: 0,
        };
        let target = browser.call("Target.createTarget", json!({"url":address}))?;
        let tid = string(&target, "targetId")?;
        let targets = http_json(port, "/json/list")?;
        let target = targets
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["id"] == tid)
            .context("Chrome sekmesi bulunamadı")?;
        let es = string(target, "webSocketDebuggerUrl")?;
        let (mut socket, _) = tungstenite::connect(es)?;
        if let MaybeTlsStream::Plain(stream) = socket.get_mut() {
            stream.set_read_timeout(Some(Duration::from_secs(12)))?;
        }
        let p = Page {
            id: id(),
            session: sid.clone(),
            label: tid.into(),
            mode: "chrome".into(),
            url: address,
            title: "Chrome".into(),
            picked: None,
            node: None,
            picking: false,
            pick_epoch: 0,
            poll_gate: Arc::new(tokio::sync::Mutex::new(())),
            revision: 0,
            remote: Some(Arc::new(Mutex::new(Remote {
                socket,
                sequence: 0,
            }))),
        };
        c.browsers
            .lock()
            .unwrap()
            .chrome
            .insert(sid, port.to_string());
        c.browsers
            .lock()
            .unwrap()
            .pages
            .insert(p.id.clone(), p.clone());
        Ok(p)
    })
    .await?
}
pub async fn dispatch(core: &Arc<Core>, action: &str, args: &Value) -> Result<Value> {
    if matches!(
        action,
        "trace" | "trace_summary" | "trace_image" | "trace_clear"
    ) {
        let sid = string(args, "sessionId")?;
        core.session(sid)?;
        let mut browser = core.browsers.lock().unwrap();
        if action == "trace_clear" {
            save_trace(core, sid, &TraceHistory::default())?;
            browser.traces.remove(sid);
            drop(browser);
            core.emit("browser-trace", json!({"sessionId":sid}));
            return Ok(json!([]));
        }
        let history = trace_history(core, sid, &mut browser)?;
        let steps = history.steps.iter();
        if action == "trace_image" {
            return Ok(json!(steps
                .filter(|step| step.details["id"] == args["stepId"])
                .find_map(|step| step.image.as_deref())));
        }
        let steps = steps.map(TraceStep::public).collect::<Vec<_>>();
        if action == "trace_summary" {
            return Ok(json!({
                "steps":steps,"passed":history.passed,"failed":history.failed,"total":history.total
            }));
        }
        return Ok(json!(steps));
    }
    if action == "tabs" {
        let b = core.browsers.lock().unwrap();
        return Ok(
            json!({"tabs":b.pages.values().filter(|p|args["sessionId"].as_str().is_none_or(|s|s==p.session)).map(Page::public).collect::<Vec<_>>(),"active":b.active.get(args["sessionId"].as_str().unwrap_or(""))}),
        );
    }
    if action == "open" {
        let sid = string(args, "sessionId")?;
        core.session(sid)?;
        let address = url(string(args, "url")?)?;
        let mode = args["mode"].as_str().unwrap_or("embedded");
        let p = if args["newTab"] == false {
            match get(core, args) {
                Ok(mut p) if p.mode == mode => {
                    cdp(core, &p, "Page.navigate", json!({"url":address.as_str()})).await?;
                    p.url = address.to_string();
                    p.picked = None;
                    p.revision += 1;
                    core.browsers
                        .lock()
                        .unwrap()
                        .pages
                        .insert(p.id.clone(), p.clone());
                    p
                }
                _ => {
                    if mode == "chrome" {
                        chrome_page(core, sid, address).await?
                    } else {
                        native_page(core, sid, address).await?
                    }
                }
            }
        } else if mode == "chrome" {
            chrome_page(core, sid, address).await?
        } else {
            native_page(core, sid, address).await?
        };
        core.browsers
            .lock()
            .unwrap()
            .active
            .insert(sid.into(), p.id.clone());
        changed(core, &p);
        return Ok(p.public());
    }
    if action == "hide" {
        let mut browser = core.browsers.lock().unwrap();
        for p in browser.pages.values() {
            if p.mode == "embedded" {
                if let Some(w) = core.app.get_webview(&p.label) {
                    let _ = w.hide();
                }
            }
        }
        browser.visible = None;
        return Ok(json!(true));
    }
    let mut p = get(core, args)?;
    match action{
        "activate"=>{core.browsers.lock().unwrap().active.insert(p.session.clone(),p.id.clone());Ok(p.public())},
        "viewport"=>{let mut browser=core.browsers.lock().unwrap();browser.visible=None;for other in browser.pages.values(){if other.id!=p.id{if let Some(w)=core.app.get_webview(&other.label){let _=w.hide();}}}if let Some(w)=core.app.get_webview(&p.label){if args["visible"]==false{w.hide()?}else{let x=args["x"].as_f64().unwrap_or(0.).max(0.);let y=args["y"].as_f64().unwrap_or(0.).max(0.);let width=args["width"].as_f64().unwrap_or(10.).max(1.);let height=args["height"].as_f64().unwrap_or(10.).max(1.);w.set_position(LogicalPosition::new(x,y))?;w.set_size(LogicalSize::new(width,height))?;w.show()?;browser.visible=Some(p.id.clone());}}Ok(json!(true))},
        "close"=>{if let Some(w)=core.app.get_webview(&p.label){w.close()?}else{cdp(core,&p,"Page.close",json!({})).await?;}let mut b=core.browsers.lock().unwrap();b.pages.remove(&p.id);if b.visible.as_deref()==Some(&p.id){b.visible=None;}
        if b.active.get(&p.session)==Some(&p.id){let next=b.pages.values().find(|x|x.session==p.session).map(|x|x.id.clone());if let Some(next)=next{b.active.insert(p.session.clone(),next);}else{b.active.remove(&p.session);}}Ok(json!(true))},
        "reload"=>cdp(core,&p,"Page.reload",json!({})).await,
        "back"|"forward"=>{let h=cdp(core,&p,"Page.getNavigationHistory",json!({})).await?;let idx=h["currentIndex"].as_i64().unwrap_or(0)+if action=="back"{-1}else{1};if idx>=0{if let Some(w)=h["entries"].as_array().and_then(|a|a.get(idx as usize)){cdp(core,&p,"Page.navigateToHistoryEntry",json!({"entryId":w["id"]})).await?;}}Ok(json!(true))},
        "pick"=>{
            if p.picking { cancel_picker(core, &mut p).await?; }
            else {
                if p.mode=="chrome" { eval(core,&p,PICK_SCRIPT).await?; }
                else {
                    eval(core,&p,PICK_ESCAPE_SCRIPT).await?;
                    cdp(core,&p,"DOM.enable",json!({})).await?;
                    cdp(core,&p,"Overlay.enable",json!({})).await?;
                    cdp(core,&p,"Overlay.setInspectMode",json!({"mode":"searchForNode","highlightConfig":{"showInfo":true,"contentColor":{"r":160,"g":210,"b":90,"a":0.3},"borderColor":{"r":180,"g":240,"b":120,"a":0.9}}})).await?;
                }
                p.picking=true; p.node=None; p.pick_epoch+=1;
            }
            core.browsers.lock().unwrap().pages.insert(p.id.clone(),p.clone()); changed(core,&p); Ok(p.public())
        },
        "cancel_pick"=>{cancel_picker(core,&mut p).await?;core.browsers.lock().unwrap().pages.insert(p.id.clone(),p.clone());changed(core,&p);Ok(p.public())},
        "clear_pick"=>{
            if args["pickId"].as_str()!=p.picked.as_ref().and_then(|v|v["id"].as_str()){bail!("Seçim değişti")};
            cancel_picker(core,&mut p).await?;
            let _=eval(core,&p,"window.__workflowPick=null").await;
            p.picked=None; core.browsers.lock().unwrap().pages.insert(p.id.clone(),p.clone());changed(core,&p);Ok(p.public())
        },
        "poll"=>{
            // UI and agent polling must not consume the same pending selection twice.
            let poll_gate=p.poll_gate.clone(); let _poll_guard=poll_gate.lock().await;
            p=get(core,args)?;
            let observed_epoch=p.pick_epoch; let observed_node=p.node;
            let state=eval(core,&p,"({url:location.href,title:document.title,pick:window.__workflowPick||null,picking:window.__workflowPicking===true,cancelPick:window.__workflowPickEscape===true})").await?;let address=state["url"].as_str().unwrap_or(&p.url).to_string();if address!=p.url{p.revision+=1;p.picked=None;}p.url=address;p.title=state["title"].as_str().unwrap_or("").into();
            if p.mode=="chrome" {p.picking=state["picking"]==true;}
            else if p.picking && state["cancelPick"]==true {cancel_picker(core,&mut p).await?;}
            if let Some(node)=p.node.take(){
                cdp(core,&p,"Overlay.setInspectMode",json!({"mode":"none","highlightConfig":{}})).await?;
                let _=cdp(core,&p,"Overlay.hideHighlight",json!({})).await;
                let _=eval(core,&p,"window.__workflowCancelEscape?.();window.__workflowPickEscape=false").await;
                let resolved=cdp(core,&p,"DOM.resolveNode",json!({"backendNodeId":node})).await?;let object=string(&resolved["object"],"objectId")?;let capture=format!("function(){{return ({EXTRACT_SCRIPT})(this)}}");let r=cdp(core,&p,"Runtime.callFunctionOn",json!({"objectId":object,"functionDeclaration":capture,"returnByValue":true})).await?;let _=cdp(core,&p,"Runtime.releaseObject",json!({"objectId":object})).await;if !r["result"]["value"].is_null(){p.picked=Some(save_pick(core,&p,r["result"]["value"].clone()).await?);}}
            else if !state["pick"].is_null(){let _=eval(core,&p,"window.__workflowPick=null").await;p.picked=Some(save_pick(core,&p,state["pick"].clone()).await?);p.picking=false;}
            {let mut b=core.browsers.lock().unwrap();if let Some(current)=b.pages.get(&p.id){if current.revision>p.revision{p.revision=current.revision;p.picked=None;}
                if current.pick_epoch!=observed_epoch {p.pick_epoch=current.pick_epoch;p.picking=current.picking;p.picked=current.picked.clone();p.node=current.node;}
                else if current.node!=observed_node {p.node=current.node;p.picking=current.picking;}}b.pages.insert(p.id.clone(),p.clone());}Ok(p.public())},
        "response"=>eval(core,&p,"(()=>{const text=document.querySelector('pre')?.textContent||document.body?.innerText||'';let data=null;try{data=JSON.parse(text)}catch{}return {text,json:data};})()").await,
        "read"=>{let script=format!("({READ_SCRIPT})({})",json!(id()));let content=eval(core,&p,&script).await?;let mut result=json!({"pageId":p.id,"url":content["url"],"title":content["title"],"text":content["text"],"elements":content["elements"],"documentId":content["documentId"],"json":content["json"]});if args["screenshot"]==true{result["screenshot"]=screenshot(core,&p).await?;}Ok(result)},
        "screenshot"=>Ok(json!({"pageId":p.id,"data":screenshot(core,&p).await?})),
        "attachment"=>{let pick=p.picked.context("Bir öğe seç")?;if args["pickId"]!=pick["id"]{bail!("Seçim değişti")};Ok(pick)},
        "action"=>perform(core,&p,args).await,
        "assert"=>assert_page(core,&p,args).await,
        _=>bail!("Tarayıcı işlemi desteklenmiyor")
    }
}

async fn assert_page(core: &Arc<Core>, page: &Page, args: &Value) -> Result<Value> {
    let check = string(args, "check")?;
    let text = string(args, "expected")?;
    if text.trim().is_empty() || text.len() > 1000 {
        bail!("Doğrulanacak metin boş olamaz ve 1000 karakteri aşamaz.")
    }
    let script = match check {
        "text_contains" => format!("(document.body?.innerText || '').includes({})", json!(text)),
        "text_absent" => format!(
            "!!document.body && !document.body.innerText.includes({})",
            json!(text)
        ),
        "url_contains" => format!("location.href.includes({})", json!(text)),
        _ => bail!("Bilinmeyen tarayıcı doğrulaması."),
    };
    let timeout = Duration::from_millis(args["timeoutMs"].as_u64().unwrap_or(3000).min(8000));
    let started = Instant::now();
    loop {
        if eval(core, page, &script).await? == true {
            return Ok(json!({"pageId":page.id,"passed":true}));
        }
        if started.elapsed() >= timeout {
            return Ok(
                json!({"pageId":page.id,"passed":false,"message":"Beklenen koşul süre içinde sağlanmadı."}),
            );
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

pub async fn agent_action(
    core: &Arc<Core>,
    sid: &str,
    action: &str,
    args: &Value,
) -> Result<Value> {
    if action == "read" {
        return dispatch(core, action, args).await;
    }
    let started = Instant::now();
    let time = now();
    let previous = (action != "open").then(|| get(core, args).ok()).flatten();
    let result = dispatch(core, action, args).await;
    let output = result.as_ref().ok();
    let passed =
        result.is_ok() && (action != "assert" || output.is_some_and(|out| out["passed"] == true));
    let page = if action == "open" {
        output.and_then(|out| get(core, &json!({"sessionId":sid,"pageId":out["id"]})).ok())
    } else {
        previous
    };
    let elapsed = started.elapsed().as_millis() as u64;
    let image = if !passed {
        match &page {
            Some(p) => tokio::time::timeout(Duration::from_secs(3), screenshot(core, p))
                .await
                .ok()
                .and_then(Result::ok)
                .and_then(|value| {
                    let data = value.as_str()?;
                    (data.len() <= 2 * 1024 * 1024).then(|| data.to_owned())
                }),
            None => None,
        }
    } else {
        None
    };
    let address = page
        .as_ref()
        .and_then(|p| url::Url::parse(&p.url).ok())
        .map(|mut address| {
            address.set_query(None);
            address.set_fragment(None);
            let _ = address.set_password(None);
            let _ = address.set_username("");
            address.to_string().chars().take(250).collect::<String>()
        });
    let operation = match action {
        "assert" => args["check"].as_str(),
        "action" => args["action"].as_str(),
        _ => Some("open"),
    };
    let detail = if action == "assert" {
        args["label"].as_str()
    } else {
        output
            .and_then(|out| out["target"].as_str())
            .or_else(|| args["ref"].as_str())
    };
    let clean = |text: &str| {
        text.chars()
            .filter(|c| !c.is_control())
            .take(100)
            .collect::<String>()
    };
    let step = TraceStep {
        details: json!({
            "id":id(),"time":time,"kind":action,"operation":operation.map(clean),"passed":passed,
            "durationMs":elapsed,"url":address,"detail":detail.map(clean)
        }),
        image,
    };
    let saved = (|| {
        let mut browser = core.browsers.lock().unwrap();
        let history = trace_history(core, sid, &mut browser)?;
        history.record(step);
        save_trace(core, sid, history)
    })();
    if let Err(error) = saved {
        eprintln!("Tarayıcı kaydı saklanamadı: {error}");
    }
    core.emit("browser-trace", json!({"sessionId":sid}));
    result
}
async fn cancel_picker(core: &Arc<Core>, p: &mut Page) -> Result<()> {
    if p.mode == "chrome" {
        eval(core,p,"window.__workflowCancelPick?.();window.__workflowPicking=false;window.__workflowPick=null").await?;
    } else {
        if p.picking || p.node.is_some() {
            cdp(
                core,
                p,
                "Overlay.setInspectMode",
                json!({"mode":"none","highlightConfig":{}}),
            )
            .await?;
        }
        let _ = cdp(core, p, "Overlay.hideHighlight", json!({})).await;
        eval(
            core,
            p,
            "window.__workflowCancelEscape?.();window.__workflowPickEscape=false",
        )
        .await?;
    }
    p.picking = false;
    p.node = None;
    p.pick_epoch += 1;
    Ok(())
}
async fn screenshot(core: &Arc<Core>, p: &Page) -> Result<Value> {
    let r = cdp(
        core,
        p,
        "Page.captureScreenshot",
        json!({"format":"png","captureBeyondViewport":false}),
    )
    .await?;
    Ok(json!(format!(
        "data:image/png;base64,{}",
        string(&r, "data")?
    )))
}
async fn save_pick(core: &Arc<Core>, p: &Page, mut value: Value) -> Result<Value> {
    use base64::{engine::general_purpose::STANDARD, Engine};
    let pid = id();
    let folder = core.directory.join("browser-picks").join(&pid);
    fs::create_dir_all(&folder)?;
    let bounds = &value["bounds"];
    let clip = json!({"x":bounds["x"].as_f64().unwrap_or(0.).max(0.),"y":bounds["y"].as_f64().unwrap_or(0.).max(0.),"width":bounds["width"].as_f64().unwrap_or(1.).max(1.),"height":bounds["height"].as_f64().unwrap_or(1.).max(1.),"scale":1});
    let image = cdp(
        core,
        p,
        "Page.captureScreenshot",
        json!({"format":"png","captureBeyondViewport":false,"clip":clip}),
    )
    .await?;
    let bytes = STANDARD.decode(string(&image, "data")?)?;
    atomic(&folder.join("element.png"), &bytes)?;
    value["id"] = json!(pid);
    value["url"] = json!(p.url);
    value["pageId"] = json!(p.id);
    value["screenshot"] = json!(folder.join("element.png"));
    atomic(
        &folder.join("context.json"),
        &serde_json::to_vec_pretty(&value)?,
    )?;
    value["contextFile"] = json!(folder.join("context.json"));
    Ok(value)
}
async fn perform(core: &Arc<Core>, p: &Page, args: &Value) -> Result<Value> {
    let action = string(args, "action")?;
    if let Some(reference) = args["ref"].as_str() {
        let document = string(args, "documentId")?;
        // A hidden WebView2 controller does not deliver pointer input. A DOM
        // activation still works for an explicitly resolved button or link.
        let background_click = action == "click"
            && p.mode == "embedded"
            && core.browsers.lock().unwrap().visible.as_deref() != Some(&p.id);
        let value = if action == "check" {
            &args["checked"]
        } else {
            &args["text"]
        };
        let script = format!(
            r#"(() => {{
                const w = window.__workflowElements?.[{reference}];
                if (window.__workflowDocument !== {document} || !w?.isConnected)
                    throw Error('Sayfa veya öğe değişti; tekrar oku');
                const target = (w.getAttribute('aria-label') || w.labels?.[0]?.innerText ||
                    w.innerText || w.tagName).trim().slice(0, 100);
                if ({background_click}) {{
                    if (typeof w.click !== 'function' || w.matches(':disabled') || !w.getClientRects().length)
                        throw Error('Öğe tıklanamıyor');
                    w.click();
                    return {{target}};
                }}
                w.scrollIntoView({{block:'center', inline:'nearest'}});
                const action = {action};
                const value = {value};
                if (action === 'check') {{
                    if (!(w instanceof HTMLInputElement) || !['checkbox', 'radio'].includes(w.type) ||
                        typeof value !== 'boolean' || w.matches(':disabled'))
                        throw Error('Öğe işaretlenemiyor');
                    if (w.checked !== value) w.click();
                    if (w.checked !== value) throw Error('İşaret değiştirilemedi');
                    return {{target}};
                }}
                if (action === 'fill' || action === 'select') {{
                    if (typeof value !== 'string' || w.matches(':disabled, [readonly]'))
                        throw Error('Öğe yazılabilir değil');
                    if (action === 'select') {{
                        if (!(w instanceof HTMLSelectElement) || !Array.from(w.options).some(o => o.value === value))
                            throw Error('Seçenek bulunamadı');
                        w.value = value;
                    }} else if (w instanceof HTMLInputElement || w instanceof HTMLTextAreaElement) {{
                        const proto = w instanceof HTMLInputElement ? HTMLInputElement.prototype : HTMLTextAreaElement.prototype;
                        Object.getOwnPropertyDescriptor(proto, 'value').set.call(w, value);
                    }} else if (w.isContentEditable) {{
                        w.textContent = value;
                    }} else {{
                        throw Error('Öğe yazılabilir değil');
                    }}
                    w.dispatchEvent(new Event('input', {{bubbles:true}}));
                    w.dispatchEvent(new Event('change', {{bubbles:true}}));
                    return {{target}};
                }}
                if (action === 'type' || action === 'key') {{
                    w.focus?.({{preventScroll:true}});
                    const focused = w.getRootNode().activeElement;
                    if (!focused || !w.contains(focused) || !w.isConnected)
                        throw Error('Öğe odaklanamıyor');
                }}
                const r = w.getBoundingClientRect();
                return {{x:r.x+r.width/2, y:r.y+r.height/2, target}};
            }})()"#,
            document = json!(document),
            reference = json!(reference),
            action = json!(action),
        );
        let point = eval(core, p, &script).await?;
        if background_click || matches!(action, "fill" | "select" | "check") {
            return Ok(json!({"pageId":p.id,"done":true,"target":point["target"]}));
        }
        let mut a = args.clone();
        a.as_object_mut().unwrap().remove("ref");
        a["x"] = point["x"].clone();
        a["y"] = point["y"].clone();
        let mut result = Box::pin(perform(core, p, &a)).await?;
        result["target"] = point["target"].clone();
        return Ok(result);
    }
    match action {
        "click" | "double_click" | "right_click" | "hover" => {
            let x = args["x"].as_f64().context("X koordinatı gerekli")?;
            let y = args["y"].as_f64().context("Y koordinatı gerekli")?;
            if !x.is_finite() || !y.is_finite() || x < 0. || y < 0. {
                bail!("Koordinat geçersiz")
            };
            cdp(
                core,
                p,
                "Input.dispatchMouseEvent",
                json!({"type":"mouseMoved","x":x,"y":y}),
            )
            .await?;
            if action != "hover" {
                let button = if action == "right_click" {
                    "right"
                } else {
                    "left"
                };
                let count = if action == "double_click" { 2 } else { 1 };
                for i in 1..=count {
                    cdp(
                        core,
                        p,
                        "Input.dispatchMouseEvent",
                        json!({"type":"mousePressed","x":x,"y":y,"button":button,"clickCount":i}),
                    )
                    .await?;
                    cdp(
                        core,
                        p,
                        "Input.dispatchMouseEvent",
                        json!({"type":"mouseReleased","x":x,"y":y,"button":button,"clickCount":i}),
                    )
                    .await?;
                }
            }
        }
        "type" => {
            cdp(
                core,
                p,
                "Input.insertText",
                json!({"text":string(args,"text")?}),
            )
            .await?;
        }
        "key" => {
            let chord = string(args, "key")?;
            let parts = chord.split('+').collect::<Vec<_>>();
            let key = *parts.last().context("Tuş gerekli")?;
            let modifiers = parts[..parts.len() - 1].iter().fold(0, |m, k| {
                m | match k.to_ascii_lowercase().as_str() {
                    "alt" => 1,
                    "ctrl" | "control" => 2,
                    "meta" | "cmd" | "command" => 4,
                    "shift" => 8,
                    _ => 0,
                }
            });
            let code = match key {
                "Enter" => 13,
                "Tab" => 9,
                "Escape" => 27,
                "Backspace" => 8,
                "ArrowLeft" => 37,
                "ArrowUp" => 38,
                "ArrowRight" => 39,
                "ArrowDown" => 40,
                "Delete" => 46,
                "Home" => 36,
                "End" => 35,
                "PageUp" => 33,
                "PageDown" => 34,
                "Space" => 32,
                _ => {
                    if key.len() == 1 {
                        key.to_ascii_uppercase().as_bytes()[0] as i32
                    } else if let Some(f) =
                        key.strip_prefix('F').and_then(|f| f.parse::<i32>().ok())
                    {
                        111 + f.clamp(1, 24)
                    } else {
                        0
                    }
                }
            };
            for t in ["keyDown", "keyUp"] {
                cdp(core,p,"Input.dispatchKeyEvent",json!({"type":t,"key":if key=="Space"{" "}else{key},"modifiers":modifiers,"windowsVirtualKeyCode":code,"text":if t=="keyDown"&&key=="Enter"&&modifiers==0{"\r"}else{""}})).await?;
            }
        }
        "scroll" => {
            cdp(core,p,"Input.dispatchMouseEvent",json!({"type":"mouseWheel","x":args["x"].as_f64().unwrap_or(100.),"y":args["y"].as_f64().unwrap_or(100.),"deltaX":args["deltaX"].as_f64().unwrap_or(0.),"deltaY":args["deltaY"].as_f64().unwrap_or(400.)})).await?;
        }
        "drag" => {
            let x = args["x"].as_f64().context("X gerekli")?;
            let y = args["y"].as_f64().context("Y gerekli")?;
            let tx = args["toX"].as_f64().context("Hedef X gerekli")?;
            let ty = args["toY"].as_f64().context("Hedef Y gerekli")?;
            cdp(
                core,
                p,
                "Input.dispatchMouseEvent",
                json!({"type":"mousePressed","x":x,"y":y,"button":"left","clickCount":1}),
            )
            .await?;
            for i in 1..=10 {
                cdp(core,p,"Input.dispatchMouseEvent",json!({"type":"mouseMoved","x":x+(tx-x)*i as f64/10.,"y":y+(ty-y)*i as f64/10.,"button":"left","buttons":1})).await?;
            }
            cdp(
                core,
                p,
                "Input.dispatchMouseEvent",
                json!({"type":"mouseReleased","x":tx,"y":ty,"button":"left","clickCount":1}),
            )
            .await?;
        }
        "wait" => {
            tokio::time::sleep(Duration::from_millis(
                args["ms"].as_u64().unwrap_or(500).min(5000),
            ))
            .await
        }
        _ => bail!("Geçersiz fare/klavye işlemi"),
    }
    Ok(json!({"pageId":p.id,"done":true}))
}

const EXTRACT_SCRIPT: &str = r#"(w)=>{const r=w.getBoundingClientRect(),s=getComputedStyle(w),styles={};for(const k of ['display','position','color','background-color','font-family','font-size','font-weight','padding','margin','border','border-radius','width','height','gap','align-items','justify-content'])styles[k]=s.getPropertyValue(k);return {selector:w.tagName.toLowerCase()+(w.id?'#'+w.id:''),html:w.outerHTML.slice(0,20000),text:w.innerText?.slice(0,4000)||'',styles,bounds:{x:r.x,y:r.y,width:r.width,height:r.height}}}"#;
const PICK_ESCAPE_SCRIPT: &str = r#"(()=>{window.__workflowCancelEscape?.();window.__workflowPickEscape=false;const cancel=()=>removeEventListener('keydown',key,true);const key=e=>{if(e.key==='Escape'){window.__workflowPickEscape=true;cancel()}};window.__workflowCancelEscape=cancel;addEventListener('keydown',key,true);return true})()"#;
const PICK_SCRIPT: &str = r#"(()=>{window.__workflowCancelPick?.();window.__workflowPicking=true;let old=null,outline='';const clear=()=>{if(old)old.style.outline=outline};const hover=w=>{clear();old=w.target;outline=old.style.outline;old.style.outline='2px solid #b7e88b'};const finish=w=>{w.preventDefault();w.stopImmediatePropagation();clear();const el=w.target,r=el.getBoundingClientRect(),s=getComputedStyle(el),styles={};for(const k of ['display','position','color','background-color','font-size','font-family','padding','margin','border','border-radius','width','height'])styles[k]=s.getPropertyValue(k);window.__workflowPick={selector:el.tagName.toLowerCase()+(el.id?'#'+el.id:''),html:el.outerHTML.slice(0,20000),styles,bounds:{x:r.x,y:r.y,width:r.width,height:r.height}};cancel()};const key=w=>{if(w.key==='Escape')cancel()};const cancel=()=>{window.__workflowPicking=false;clear();removeEventListener('pointermove',hover,true);removeEventListener('click',finish,true);removeEventListener('keydown',key,true)};window.__workflowCancelPick=cancel;addEventListener('pointermove',hover,true);addEventListener('click',finish,true);addEventListener('keydown',key,true);return true})()"#;
const READ_SCRIPT: &str = r#"documentId=>{window.__workflowDocument=documentId;window.__workflowElements={};let i=0;const elements=[];for(const w of document.querySelectorAll('button,a,input,textarea,select,[role=button],[contenteditable=true]')){const r=w.getBoundingClientRect();if(!r.width||!r.height||getComputedStyle(w).visibility==='hidden')continue;if(++i>800)break;const ref='w'+i;window.__workflowElements[ref]=w;elements.push({ref,tag:w.tagName.toLowerCase(),type:w.type||'',text:(w.innerText||w.getAttribute('aria-label')||w.placeholder||'').slice(0,200),value:w.type==='password'?'[hidden]':w.value||'',checked:w.checked,bounds:{x:r.x,y:r.y,width:r.width,height:r.height}})}const text=document.body?.innerText||'';let data=null;try{data=JSON.parse(document.querySelector('pre')?.textContent||text)}catch{}return {url:location.href,title:document.title,text:text.slice(0,60000),elements,documentId,json:data}}"#;

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn validates_urls() {
        assert_eq!(
            url("localhost:5080/api").unwrap().as_str(),
            "http://localhost:5080/api"
        );
        assert!(url("file:///C:/secret").is_err());
        assert!(url("https://name:password@example.com").is_err());
    }

    #[test]
    fn trace_totals_outlive_detail_limit_and_images_stay_in_memory() {
        let mut history = TraceHistory::default();
        for i in 0..105 {
            history.record(TraceStep {
                details: json!({"kind":"assert","passed":i % 2 == 0,"id":i}),
                image: (i % 2 == 1).then(|| "image".into()),
            });
        }
        assert_eq!(history.steps.len(), 100);
        assert_eq!(history.steps.front().unwrap().details["id"], 5);
        assert_eq!(
            history.steps.iter().filter(|s| s.image.is_some()).count(),
            4
        );
        let stored = serde_json::to_vec(&history).unwrap();
        let reopened: TraceHistory = serde_json::from_slice(&stored).unwrap();
        assert_eq!(
            (reopened.total, reopened.passed, reopened.failed),
            (105, 53, 52)
        );
        assert!(reopened
            .steps
            .iter()
            .all(|s| s.public()["hasImage"] == false));
    }
}
