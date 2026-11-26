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
        {"name":"verification_request","description":"Read instructions for a user-initiated verification operation. Never initiate or approve verification yourself.","inputSchema":object(json!({"operation":{"type":"string"}}),vec!["operation"])},
        {"name":"verification_propose","description":"Submit the requested checklist without executing it. Only the user may approve Start.","inputSchema":object(json!({"operation":{"type":"string"},"items":{"type":"array","minItems":1,"maxItems":20,"items":{"type":"object","properties":{"title":{"type":"string","maxLength":180},"check":{"type":"string","maxLength":2000}},"required":["title","check"],"additionalProperties":false}}}),vec!["operation","items"])},
        {"name":"verification_report","description":"Report all approved items honestly with actual evidence. These are model reports, not independently measured proof.","inputSchema":object(json!({"operation":{"type":"string"},"results":{"type":"array","minItems":1,"maxItems":20,"items":{"type":"object","properties":{"id":{"type":"string"},"status":{"type":"string","enum":["passed","failed","blocked"]},"evidence":{"type":"string","maxLength":4000}},"required":["id","status","evidence"],"additionalProperties":false}}}),vec!["operation","results"])},
        {"name":"browser_open","description":"Open an HTTP/HTTPS page in this session's embedded browser or installed Chrome. Set newTab true for another tab.","inputSchema":object(json!({"url":{"type":"string"},"mode":{"type":"string","enum":["embedded","chrome"]},"newTab":{"type":"boolean"},"pageId":{"type":"string"}}),vec!["url"])},
        {"name":"browser_read","description":"Read a tab's visible text, structured elements and optional screenshot. Use documentId and element ref for actions. Returns tabs if no tab is open.","inputSchema":object(json!({"pageId":{"type":"string"},"screenshot":{"type":"boolean"}}),vec![])},
        {"name":"browser_action","description":"Interact with the session browser. Actions: click, double_click, right_click, hover, drag, fill, type, select, check, key, scroll, wait. For ref actions include documentId from the latest browser_read. Coordinates refer to the latest screenshot viewport.","inputSchema":object(json!({"pageId":{"type":"string"},"action":{"type":"string"},"documentId":{"type":"string"},"ref":{"type":"string"},"x":{"type":"number"},"y":{"type":"number"},"toX":{"type":"number"},"toY":{"type":"number"},"text":{"type":"string"},"checked":{"type":"boolean"},"key":{"type":"string"},"deltaX":{"type":"number"},"deltaY":{"type":"number"},"ms":{"type":"integer"}}),vec!["action"])},
        {"name":"browser_assert","description":"Verify browser text or URL and record a passed/failed test step. Checks: text_contains, text_absent, url_contains. Supply a short human-readable label without credentials or personal data. Failure captures a screenshot when possible.","inputSchema":object(json!({"pageId":{"type":"string"},"check":{"type":"string","enum":["text_contains","text_absent","url_contains"]},"expected":{"type":"string"},"label":{"type":"string"},"timeoutMs":{"type":"integer","minimum":0,"maximum":8000}}),vec!["check","expected"])},
        {"name":"session_catalog","description":"List only sessions explicitly connected to this session, with permissions.","inputSchema":object(json!({}),vec![])},
        {"name":"read_context","description":"Read a bounded recent page of a connected conversation. Use nextBeforeByte as beforeByte for older messages. Never starts a provider or sends input.","inputSchema":object(json!({"target":{"type":"string"},"beforeByte":{"type":"integer","minimum":0}}),vec!["target"])},
        {"name":"read_files","description":"Read shareable files from a connected session with file permission. If paths is omitted, list shareable changed files. Workflow excludes ignored files and likely credentials.","inputSchema":object(json!({"target":{"type":"string"},"paths":{"type":"array","items":{"type":"string"}}}),vec!["target"])},
        {"name":"request_work","description":"Request work from a connected existing session with delegation permission. The user accepts it in Workflow; connecting sessions never starts jobs by itself. No new agent is spawned.","inputSchema":object(json!({"target":{"type":"string"},"instruction":{"type":"string"}}),vec!["target","instruction"])},
        {"name":"read_result","description":"Read the status or result of a work request created by this session.","inputSchema":object(json!({"requestId":{"type":"string"}}),vec!["requestId"])},
        {"name":"report_result","description":"Return the outcome of a work request addressed to this session.","inputSchema":object(json!({"requestId":{"type":"string"},"result":{"type":"string"}}),vec!["requestId","result"])}
    ]);
    #[cfg(any(windows, target_os = "macos"))]
    let catalog = {
        let mut catalog = catalog;
        catalog.as_array_mut().unwrap().push(json!({
            "name": "teams_send_message",
            "description": "Submit a message to the Teams chat saved for this session through Workflow's signed-in Teams Web connection. Use only when the user explicitly asks to send a Teams message. If the user supplies a Teams chat or message link, pass it exactly so Workflow can verify that it matches the saved destination. A submitted result means Teams Web received the send action, and delivery remains unverified. If Workflow opens the Teams window after a send error, ask the user to inspect the chat. Wait for the user to request another attempt.",
            "inputSchema": object(json!({
                "link": {"type":"string","description":"Include the Teams chat or message link when the user provides one"},
                "text": {"type":"string","description":"Plain-text message to send"}
            }), vec!["text"]),
            "annotations": {"readOnlyHint":false,"destructiveHint":true,"idempotentHint":false,"openWorldHint":true}
        }));
        catalog.as_array_mut().unwrap().push(json!({
            "name": "teams_open",
            "description": "Show the existing Workflow Teams Web window when the user asks to see it. The window stays on its current page. The tool only makes the window visible so the user can inspect a draft or other issue.",
            "inputSchema": object(json!({}), vec![]),
            "annotations": {"readOnlyHint":false,"destructiveHint":false,"idempotentHint":true,"openWorldHint":false}
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
fn prepare_connection(stream: &TcpStream) -> Result<()> {
    // Accepted sockets can inherit the listener's nonblocking mode. Each
    // request has its own worker and must wait for the complete bounded line.
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    stream.set_write_timeout(Some(Duration::from_secs(10)))?;
    Ok(())
}
fn connection(core: &Arc<Core>, mut stream: TcpStream) -> Result<()> {
    prepare_connection(&stream)?;
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
        run,
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
    let quote = |value: &str| crate::terminal::codex_config_string(value, windows_script);
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
        #[cfg(any(windows, target_os = "macos"))]
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
                json!({"protocolVersion":"2024-11-05","capabilities":{"tools":{}},"serverInfo":{"name":"workflow","version":env!("CARGO_PKG_VERSION")}}),
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
fn link(core: &Core, source: &str, target: &str, permission: &str) -> Result<()> {
    let mut db = core.db.lock().unwrap();
    let sessions = db["sessions"].as_array().unwrap();
    let source_session = sessions
        .iter()
        .find(|s| s["id"] == source)
        .context("Oturum bulunamadı")?;
    let target_session = sessions
        .iter()
        .find(|s| s["id"] == target)
        .context("Oturum bulunamadı")?;
    if source_session["archived"] == true || target_session["archived"] == true {
        bail!("Arşivlenmiş oturuma bağlantı kapalı")
    }
    let index = db["links"]
        .as_array()
        .unwrap()
        .iter()
        .position(|l| l["source"] == source && l["target"] == target && l[permission] == true)
        .context("Bu oturuma istenen erişim için bağlantı izni yok")?;
    let previous = &db["links"][index];
    if link_needs_reapproval(previous, source_session, target_session) {
        bail!("Bağlı oturumun konuşması değişti; bağlantı izinlerini yeniden kaydet")
    }
    let mut updated = previous.clone();
    for (key, current) in [
        ("sourceConversation", &source_session["conversationId"]),
        ("targetConversation", &target_session["conversationId"]),
    ] {
        if updated[key].is_null() && !current.is_null() {
            updated[key] = current.clone();
        }
    }
    if updated != *previous {
        let previous = std::mem::replace(&mut db["links"][index], updated);
        if let Err(error) = core.save_workspace(&db) {
            db["links"][index] = previous;
            return Err(error);
        }
        drop(db);
        core.emit("workspace", core.snapshot());
    }
    Ok(())
}
async fn call(core: &Arc<Core>, source: &str, run: &str, method: &str, mut args: Value) -> Result<Value> {
    if matches!(method, "verification_request" | "verification_propose" | "verification_report") {
        return crate::verification::tool(core, source, run, method, &args);
    }
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
        #[cfg(any(windows, target_os = "macos"))]
        "teams_send_message" => crate::teams::send_for_session(core, source, &args).await,
        #[cfg(any(windows, target_os = "macos"))]
        "teams_open" => crate::teams::show_for_session(core, source),
        "session_catalog" => {
            let db = core.db.lock().unwrap();
            let sessions = db["sessions"].as_array().unwrap();
            let source_session = sessions
                .iter()
                .find(|s| s["id"] == source)
                .context("Oturum bulunamadı")?;
            let entries: Vec<_> = db["links"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|link| link["source"] == source)
                .filter_map(|link| {
                    let target = sessions.iter().find(|s| s["id"] == link["target"] && s["archived"] != true)?;
                    Some(json!({
                        "id":target["id"], "name":target["name"], "provider":target["provider"],
                        "permissions":link, "needsReapproval":link_needs_reapproval(link, source_session, target),
                    }))
                })
                .collect();
            Ok(json!(entries))
        }
        "read_context" => {
            let target = string(&args, "target")?;
            link(core, source, target, "read")?;
            let before = match args.get("beforeByte") {
                None | Some(Value::Null) => None,
                Some(value) => Some(value.as_u64().context("Geçmiş sayfası geçersiz")?),
            };
            if before.is_some() { history::messages_page(core,target,before) }
            else { history::messages(core,target) }
        }
        "read_files" => {
            let target = string(&args, "target")?;
            link(core, source, target, "files")?;
            let scope = json!({"sessionId":target});
            let root = core.root(&scope)?;
            let project = core
                .root(&json!({"projectId":core.session(target)?["projectId"]}))?
                .canonicalize()?;
            if let Some(paths) = args.get("paths") {
                let paths = paths.as_array().context("Dosya yolları liste olmalı")?;
                if paths.len() > 20 {
                    bail!("Tek istekte en fazla 20 dosya")
                };
                let mut out = vec![];
                let mut bytes_read = 0;
                for path in paths {
                    let relative = path.as_str().context("Dosya yolu metin olmalı")?;
                    let bytes = crate::sharing::read_shared(&root, &project, relative)?;
                    bytes_read += bytes.len();
                    if bytes_read > crate::sharing::BATCH_LIMIT {
                        bail!("Tek istekte en fazla 20 MB dosya paylaşılabilir")
                    }
                    out.push(json!({"path":relative,"content":files::preview_content(std::path::Path::new(relative), &bytes)?}));
                }
                Ok(json!(out))
            } else {
                let Value::Array(changes) = files::list_changes(core, &scope)? else {
                    bail!("Geçersiz değişiklik listesi")
                };
                let root = root.canonicalize()?;
                let mut visible = Vec::new();
                for change in changes {
                    let relative = string(&change, "path")?;
                    if crate::sharing::project_path_blocked(
                        &root,
                        &project,
                        std::path::Path::new(relative),
                    )? {
                        continue;
                    }
                    // A renamed or deleted file may still have a sensitive baseline.
                    if let Some(before) = change["before"].as_str() {
                        if !crate::sharing::safe_baseline(core, before).unwrap_or(false) {
                            continue;
                        }
                    }
                    if change["after"].is_string()
                        && crate::sharing::read_shared(&root, &project, relative).is_err()
                    {
                        continue;
                    }
                    visible.push(change);
                }
                Ok(json!(visible))
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
    fn accepted_connection_waits_for_delayed_and_fragmented_request() {
        for fragmented in [false, true] {
            let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
            listener.set_nonblocking(true).unwrap();
            let address = listener.local_addr().unwrap();
            let (accepted, wait_for_accept) = std::sync::mpsc::channel();
            let client = std::thread::spawn(move || {
                let mut client = TcpStream::connect_timeout(&address, Duration::from_secs(3)).unwrap();
                client.set_write_timeout(Some(Duration::from_secs(3))).unwrap();
                wait_for_accept.recv_timeout(Duration::from_secs(3)).unwrap();
                std::thread::sleep(Duration::from_millis(100));
                if fragmented {
                    client.write_all(b"{\"synthetic\":").unwrap();
                    std::thread::sleep(Duration::from_millis(100));
                    client.write_all(b"true}\n").unwrap();
                } else {
                    client.write_all(b"{\"synthetic\":true}\n").unwrap();
                }
            });
            let deadline = std::time::Instant::now() + Duration::from_secs(3);
            let stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(std::time::Instant::now() < deadline, "accept timed out");
                        std::thread::sleep(Duration::from_millis(1));
                    }
                    Err(_) => panic!("accept failed"),
                }
            };
            // Force the inherited state so the regression is deterministic on
            // platforms where accept returns a blocking socket by default.
            stream.set_nonblocking(true).unwrap();
            prepare_connection(&stream).unwrap();
            accepted.send(()).unwrap();
            let mut line = String::new();
            BufReader::new(stream).read_line(&mut line).unwrap();
            client.join().unwrap();
            assert_eq!(serde_json::from_str::<Value>(&line).unwrap(), json!({"synthetic":true}));
        }
    }

    #[test]
    fn accepted_connection_has_bounded_reads_and_writes() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (stream, _) = listener.accept().unwrap();
        prepare_connection(&stream).unwrap();
        assert_eq!(stream.read_timeout().unwrap(), Some(Duration::from_secs(10)));
        assert_eq!(stream.write_timeout().unwrap(), Some(Duration::from_secs(10)));
        drop(client);
    }

    #[test]
    fn codex_mcp_array_preserves_paths_through_windows_shims() {
        let command = r"C:\My Project\Workflow.exe";
        let lease = r"C:\O'Brien & %USER%\İ.json";
        let native = codex_configuration(command, lease, false);
        let native_args = native[3]
            .strip_prefix("mcp_servers.workflow.args=")
            .unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(native_args).unwrap(),
            json!(["mcp", lease])
        );

        let shim = codex_configuration(command, lease, true);
        assert_eq!(
            shim[1],
            r#"mcp_servers.workflow.command=\"C:\u005cMy\u0020Project\u005cWorkflow.exe\""#
        );
        assert_eq!(
            shim[3],
            r#"mcp_servers.workflow.args=[\"mcp\",\"C:\u005cO\u0027Brien\u0020\u0026\u0020\u0025USER\u0025\u005c\u0130.json\"]"#
        );
    }

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

    #[cfg(any(windows, target_os = "macos"))]
    #[test]
    fn teams_requires_a_message_and_accepts_an_optional_link() {
        let catalog = tools();
        let tool = catalog
            .as_array()
            .unwrap()
            .iter()
            .find(|tool| tool["name"] == "teams_send_message")
            .unwrap();
        assert_eq!(tool["inputSchema"]["required"], json!(["text"]));
        assert_eq!(tool["inputSchema"]["properties"]["link"]["type"], "string");
        assert_eq!(tool["annotations"]["idempotentHint"], false);
    }

    #[cfg(any(windows, target_os = "macos"))]
    #[test]
    fn teams_open_exposes_no_chat_or_message_parameters() {
        let catalog = tools();
        let tool = catalog
            .as_array()
            .unwrap()
            .iter()
            .find(|tool| tool["name"] == "teams_open")
            .unwrap();
        assert_eq!(tool["inputSchema"]["properties"], json!({}));
        assert_eq!(tool["inputSchema"]["required"], json!([]));
        assert_eq!(tool["annotations"]["destructiveHint"], false);
    }
}
