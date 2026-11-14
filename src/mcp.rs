use crate::{browser, core::*, files, history};
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::{
    fs,
    io::{BufRead, BufReader, Read, Write},
    net::{TcpListener, TcpStream},
    path::PathBuf,
    sync::{atomic::Ordering, Arc},
    time::Duration,
};

fn tools() -> Value {
    let object = |properties: Value, required: Vec<&str>| json!({"type":"object","properties":properties,"required":required});
    let catalog = json!([
        {"name":"browser_open","description":"Open an HTTP/HTTPS page in this session's embedded browser or installed Chrome. Set newTab true for another tab.","inputSchema":object(json!({"url":{"type":"string"},"mode":{"type":"string","enum":["embedded","chrome"]},"newTab":{"type":"boolean"},"pageId":{"type":"string"}}),vec!["url"])},
        {"name":"browser_read","description":"Read a tab's visible text, structured elements and optional screenshot. Use documentId and element ref for actions. Returns tabs if no tab is open.","inputSchema":object(json!({"pageId":{"type":"string"},"screenshot":{"type":"boolean"}}),vec![])},
        {"name":"browser_action","description":"Interact with the session browser. Actions: click, double_click, right_click, hover, drag, fill, type, select, check, key, scroll, wait. For ref actions include documentId from the latest browser_read. Coordinates refer to the latest screenshot viewport.","inputSchema":object(json!({"pageId":{"type":"string"},"action":{"type":"string"},"documentId":{"type":"string"},"ref":{"type":"string"},"x":{"type":"number"},"y":{"type":"number"},"toX":{"type":"number"},"toY":{"type":"number"},"text":{"type":"string"},"checked":{"type":"boolean"},"key":{"type":"string"},"deltaX":{"type":"number"},"deltaY":{"type":"number"},"ms":{"type":"integer"}}),vec!["action"])},
        {"name":"browser_assert","description":"Verify browser text or URL and record a passed/failed test step. Checks: text_contains, text_absent, url_contains. Supply a short human-readable label without credentials or personal data. Failure captures a screenshot when possible.","inputSchema":object(json!({"pageId":{"type":"string"},"check":{"type":"string","enum":["text_contains","text_absent","url_contains"]},"expected":{"type":"string"},"label":{"type":"string"},"timeoutMs":{"type":"integer","minimum":0,"maximum":8000}}),vec!["check","expected"])},
        {"name":"session_catalog","description":"List only sessions explicitly connected to this session, with permissions.","inputSchema":object(json!({}),vec![])},
        {"name":"read_context","description":"Read a connected session conversation, without starting a provider or sending terminal input.","inputSchema":object(json!({"target":{"type":"string"}}),vec!["target"])},
        {"name":"read_files","description":"Read files from a connected session with file permission. If paths is omitted, list changed files.","inputSchema":object(json!({"target":{"type":"string"},"paths":{"type":"array","items":{"type":"string"}}}),vec!["target"])},
        {"name":"request_work","description":"Request work from a connected existing session with delegation permission. The user accepts it in Workflow; connecting sessions never starts jobs by itself. No new agent is spawned.","inputSchema":object(json!({"target":{"type":"string"},"instruction":{"type":"string"}}),vec!["target","instruction"])},
        {"name":"read_result","description":"Read the status or result of a work request created by this session.","inputSchema":object(json!({"requestId":{"type":"string"}}),vec!["requestId"])},
        {"name":"report_result","description":"Return the outcome of a work request addressed to this session.","inputSchema":object(json!({"requestId":{"type":"string"},"result":{"type":"string"}}),vec!["requestId","result"])}
    ]);
    #[cfg(windows)]
    let catalog = {
        let mut catalog = catalog;
        catalog.as_array_mut().unwrap().push(json!({
            "name": "teams_send_message",
            "description": "Submit a message to the Teams chat saved for this session through Workflow's signed-in Teams Web connection. Use only when the user explicitly asks to send a Teams message. If the user supplies a Teams chat or message link, pass it exactly so Workflow can verify that it matches the saved destination. A submitted result means Teams Web received the send action. Report submission without claiming delivery and check with the user before retrying.",
            "inputSchema": object(json!({
                "link": {"type":"string","description":"Include the Teams chat or message link when the user provides one"},
                "text": {"type":"string","description":"Plain-text message to send"}
            }), vec!["text"]),
            "annotations": {"readOnlyHint":false,"destructiveHint":true,"idempotentHint":false,"openWorldHint":true}
        }));
        catalog
    };
    catalog
}
pub fn start(core: &Arc<Core>) -> Result<()> {
    let listener = TcpListener::bind(("127.0.0.1", 0))?;
    listener.set_nonblocking(true)?;
    *core.endpoint.lock().unwrap() = listener.local_addr()?.to_string();
    let weak = Arc::downgrade(core);
    std::thread::spawn(move || loop {
        let Some(core) = weak.upgrade() else { break };
        match listener.accept() {
            Ok((stream, _)) => {
                let c = core.clone();
                std::thread::spawn(move || {
                    let _ = connection(&c, stream);
                });
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(40))
            }
            Err(_) => break,
        }
    });
    Ok(())
}
fn connection(core: &Arc<Core>, mut stream: TcpStream) -> Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    let mut line = String::new();
    BufReader::new(stream.try_clone()?)
        .take(1024 * 1024)
        .read_line(&mut line)?;
    let request: Value = serde_json::from_str(&line)?;
    let sid = string(&request, "sessionId")?;
    let run = string(&request, "runId")?;
    let token = string(&request, "token")?;
    let auth = core.directory.join("leases").join(format!("{sid}.json"));
    uuid::Uuid::parse_str(sid)?;
    let expected: Value = serde_json::from_slice(&fs::read(auth)?)?;
    if expected["runId"] != run || !constant_eq(expected["token"].as_str().unwrap_or(""), token) {
        bail!("Araç bağlantısı doğrulanamadı")
    }
    let live = core
        .terms
        .lock()
        .unwrap()
        .get(sid)
        .cloned()
        .context("Oturum kapalı")?;
    if live.run_id != run || live.exited.load(Ordering::SeqCst) {
        bail!("Oturum çalıştırması kapalı")
    }
    let response = tauri::async_runtime::block_on(call(
        core,
        sid,
        string(&request, "method")?,
        request["params"].clone(),
    ));
    let response = match response {
        Ok(v) => json!({"result":v}),
        Err(e) => json!({"error":e.to_string()}),
    };
    writeln!(stream, "{response}")?;
    Ok(())
}
fn constant_eq(a: &str, b: &str) -> bool {
    let aa = a.as_bytes();
    let bb = b.as_bytes();
    let mut n = aa.len() ^ bb.len();
    for i in 0..aa.len().max(bb.len()) {
        n |= (aa.get(i).copied().unwrap_or(0) ^ bb.get(i).copied().unwrap_or(0)) as usize;
    }
    n == 0
}
pub fn configuration(
    core: &Core,
    session: &Value,
    run_id: &str,
    provider_exe: &std::path::Path,
) -> Result<Vec<String>> {
    let sid = string(session, "id")?;
    let provider = string(session, "provider")?;
    let token = format!("{}{}", id(), id());
    let folder = core.directory.join("leases");
    fs::create_dir_all(&folder)?;
    let file = folder.join(format!("{sid}.json"));
    let lease = json!({"sessionId":sid,"runId":run_id,"endpoint":*core.endpoint.lock().unwrap(),"token":token});
    atomic(&file, &serde_json::to_vec(&lease)?)?;
    let executable = std::env::current_exe()?;
    let argv = vec!["mcp".to_string(), file.to_string_lossy().to_string()];
    let config =
        json!({"mcpServers":{"workflow":{"command":executable,"args":argv,"tools":["*"]}}});
    let cfg = folder.join(format!("{sid}-mcp.json"));
    atomic(&cfg, &serde_json::to_vec_pretty(&config)?)?;
    if provider == "claude" {
        return Ok(vec!["--mcp-config".into(), cfg.to_string_lossy().into()]);
    }
    if provider == "copilot" {
        return Ok(vec![
            "--additional-mcp-config".into(),
            format!("@{}", cfg.display()),
        ]);
    }
    Ok(codex_configuration(
        &executable.to_string_lossy(),
        &file.to_string_lossy(),
        crate::terminal::uses_windows_script(provider_exe),
    ))
}

fn codex_configuration(command: &str, lease: &str, windows_script: bool) -> Vec<String> {
    let quote = |value: &str| {
        if !windows_script {
            return json!(value).to_string();
        }
        // Preserve the TOML delimiters through Windows PowerShell's legacy argv
        // conversion. Encode characters that a batch shim could expand or split.
        use std::fmt::Write as _;
        let mut quoted = String::from(r#"\""#);
        for c in value.chars() {
            if c.is_ascii_alphanumeric() || "-_./:".contains(c) {
                quoted.push(c);
            } else if (c as u32) <= 0xffff {
                let _ = write!(quoted, "\\u{:04x}", c as u32);
            } else {
                let _ = write!(quoted, "\\U{:08x}", c as u32);
            }
        }
        quoted.push_str(r#"\""#);
        quoted
    };
    vec![
        "-c".into(),
        format!("mcp_servers.workflow.command={}", quote(command)),
        "-c".into(),
        format!(
            "mcp_servers.workflow.args=[{},{}]",
            quote("mcp"),
            quote(lease)
        ),
        "-c".into(),
        "mcp_servers.workflow.startup_timeout_sec=20".into(),
    ]
}

fn forward_tool(endpoint: &std::net::SocketAddr, lease: &Value, params: &Value) -> Result<Value> {
    let mut stream = TcpStream::connect_timeout(endpoint, Duration::from_secs(4))
        .context("Workflow araç bağlantısı kurulamadı")?;
    let timeout = match params["name"].as_str() {
        #[cfg(windows)]
        Some("teams_send_message") => crate::teams::MCP_READ_TIMEOUT,
        _ => Duration::from_secs(25),
    };
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(Duration::from_secs(4)))?;
    let mut call = lease.clone();
    call["method"] = params["name"].clone();
    call["params"] = params["arguments"].clone();
    writeln!(stream, "{call}")?;
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line)?;
    let response: Value = serde_json::from_str(&line).context("Workflow araç yanıtı geçersiz")?;
    if let Some(error) = response["error"].as_str() {
        bail!("{error}")
    }
    let mut result = response
        .get("result")
        .context("Workflow araç yanıtı eksik")?
        .clone();
    let image = result
        .as_object_mut()
        .and_then(|value| value.remove("screenshot"));
    let mut content = vec![json!({"type":"text","text":serde_json::to_string(&result)?})];
    if let Some(Value::String(image)) = image {
        if let Some((_, data)) = image.split_once(',') {
            content.push(json!({"type":"image","mimeType":"image/png","data":data}));
        }
    }
    Ok(json!({"content":content}))
}

pub fn serve_stdio(file: PathBuf) -> Result<()> {
    let lease: Value = serde_json::from_slice(&fs::read(&file)?)?;
    let endpoint = string(&lease, "endpoint")?.parse::<std::net::SocketAddr>()?;
    if !endpoint.ip().is_loopback() {
        bail!("Araç bağlantısı yerel değil")
    }
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout().lock();
    for line in stdin.lock().lines() {
        let line = line?;
        if line.len() > 1024 * 1024 {
            bail!("Araç isteği çok büyük")
        };
        let Ok(v) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let request_id = v["id"].clone();
        if request_id.is_null() {
            continue;
        }
        let result = match v["method"].as_str().unwrap_or("") {
            "initialize" => Ok(
                json!({"protocolVersion":"2024-11-05","capabilities":{"tools":{}},"serverInfo":{"name":"workflow","version":"0.1.0"}}),
            ),
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({"tools":tools()})),
            "tools/call" => Ok(forward_tool(&endpoint, &lease, &v["params"]).unwrap_or_else(
                |error| json!({"isError":true,"content":[{"type":"text","text":error.to_string()}]}),
            )),
            _ => Err(anyhow::anyhow!("Araç metodu bulunamadı")),
        };
        let response = match result {
            Ok(result) => json!({"jsonrpc":"2.0","id":request_id,"result":result}),
            Err(error) => {
                json!({"jsonrpc":"2.0","id":request_id,"error":{"code":-32601,"message":error.to_string()}})
            }
        };
        writeln!(stdout, "{response}")?;
        stdout.flush()?;
    }
    Ok(())
}
fn link(core: &Core, source: &str, target: &str, permission: &str) -> Result<Value> {
    let source_session = core.session(source)?;
    let target_session = core.session(target)?;
    if source_session["archived"] == true || target_session["archived"] == true {
        bail!("Arşivlenmiş oturuma bağlantı kapalı")
    }
    let mut changed = false;
    let out;
    {
        let mut db = core.db.lock().unwrap();
        let l = db["links"]
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .find(|l| l["source"] == source && l["target"] == target && l[permission] == true)
            .context("Bu oturuma istenen erişim için bağlantı izni yok")?;
        for (key, current) in [
            ("sourceConversation", &source_session["conversationId"]),
            ("targetConversation", &target_session["conversationId"]),
        ] {
            if !l[key].is_null() && l[key] != *current {
                bail!("Bağlı oturumun konuşması değişti; bağlantı izinlerini yeniden kaydet")
            };
            if l[key].is_null() && !current.is_null() {
                l[key] = current.clone();
                changed = true;
            }
        }
        out = l.clone();
    }
    if changed {
        core.save()?
    }
    Ok(out)
}
async fn call(core: &Arc<Core>, source: &str, method: &str, mut args: Value) -> Result<Value> {
    if method.starts_with("browser_") {
        if core.session(source)?["browserAccess"] == false {
            bail!("Bu oturumun tarayıcı erişimi kapalı")
        };
        if !args.is_object() {
            args = json!({})
        }
        args["sessionId"] = json!(source);
        let action = match method {
            "browser_open" => "open",
            "browser_read" => "read",
            "browser_action" => "action",
            "browser_assert" => "assert",
            _ => bail!("Tarayıcı aracı bulunamadı"),
        };
        if action == "open" && args["newTab"].is_null() {
            args["newTab"] = json!(true)
        }
        return browser::agent_action(core, source, action, &args).await;
    }
    match method {
        #[cfg(windows)]
        "teams_send_message" => crate::teams::send_for_session(core, source, &args).await,
        "session_catalog" => {
            let db = core.db.lock().unwrap();
            Ok(json!(db["links"].as_array().unwrap().iter().filter(|l|l["source"]==source).filter_map(|l|db["sessions"].as_array().unwrap().iter().find(|s|s["id"]==l["target"]&&s["archived"]!=true).map(|s|json!({"id":s["id"],"name":s["name"],"provider":s["provider"],"permissions":l}))).collect::<Vec<_>>()))
        }
        "read_context" => {
            let target = string(&args, "target")?;
            link(core, source, target, "read")?;
            history::messages(core, target)
        }
        "read_files" => {
            let target = string(&args, "target")?;
            link(core, source, target, "files")?;
            if let Some(paths) = args["paths"].as_array() {
                if paths.len() > 20 {
                    bail!("Tek istekte en fazla 20 dosya")
                };
                let mut out = vec![];
                for path in paths {
                    out.push(json!({"path":path,"content":files::dispatch(core,"read",&json!({"sessionId":target,"path":path}))?}));
                }
                Ok(json!(out))
            } else {
                files::list_changes(core, &json!({"sessionId":target}))
            }
        }
        "request_work" => {
            let target = string(&args, "target")?;
            link(core, source, target, "work")?;
            let instruction = string(&args, "instruction")?;
            if instruction.len() > 20000 {
                bail!("İstek çok uzun")
            };
            let request = json!({"id":id(),"source":source,"target":target,"instruction":instruction,"createdAt":now(),"state":"pending"});
            {
                core.db.lock().unwrap()["jobs"]
                    .as_array_mut()
                    .context("İstek kaydı hazır değil")?
                    .push(request.clone());
            }
            core.save()?;
            core.emit("workspace", core.snapshot());
            Ok(request)
        }
        "read_result" => {
            let db = core.db.lock().unwrap();
            db["jobs"]
                .as_array()
                .unwrap()
                .iter()
                .find(|j| j["id"] == args["requestId"] && j["source"] == source)
                .cloned()
                .context("İstek bulunamadı")
        }
        "report_result" => {
            let result = string(&args, "result")?;
            let out;
            {
                let mut db = core.db.lock().unwrap();
                let job = db["jobs"]
                    .as_array_mut()
                    .unwrap()
                    .iter_mut()
                    .find(|j| {
                        j["id"] == args["requestId"]
                            && j["target"] == source
                            && j["state"] == "accepted"
                    })
                    .context("Bu oturuma ait kabul edilmiş iş bulunamadı")?;
                job["state"] = json!("completed");
                job["result"] = json!(result);
                out = job.clone();
            }
            core.save()?;
            core.emit("workspace", core.snapshot());
            Ok(out)
        }
        _ => bail!("Araç bulunamadı"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn token_comparison() {
        assert!(constant_eq("abc", "abc"));
        assert!(!constant_eq("abc", "abcd"));
        assert!(!constant_eq("abc", "abd"));
    }
    #[test]
    fn tools_are_described() {
        assert!(tools()
            .as_array()
            .unwrap()
            .iter()
            .all(|v| v["inputSchema"]["type"] == "object"));
    }
}
