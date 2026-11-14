#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
mod accounts;
mod addons;
mod browser;
mod core;
mod files;
mod git;
mod history;
mod mcp;
mod models;
#[cfg(windows)]
mod preview;
#[cfg(windows)]
mod teams;
mod terminal;
mod usage;
use crate::core::*;
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};
use tauri::{Manager, State};

#[tauri::command]
async fn dispatch(
    webview: tauri::Webview,
    state: State<'_, Arc<Core>>,
    domain: String,
    action: String,
    args: Value,
) -> std::result::Result<Value, String> {
    if webview.label() != "main" {
        return Err("Bu sayfa uygulama araçlarına erişemez".into());
    }
    let core = state.inner().clone();
    #[cfg(windows)]
    if domain == "teams" {
        return teams::dispatch(&core, &action, &args)
            .await
            .map_err(|e| e.to_string());
    }
    #[cfg(windows)]
    if domain == "preview" && action == "open" {
        return preview::open(&core, &args).await.map_err(|e| e.to_string());
    }
    let result = if domain == "browser" {
        browser::dispatch(&core, &action, &args).await
    } else {
        tauri::async_runtime::spawn_blocking(move || route(&core, &domain, &action, &args))
            .await
            .map_err(|e| anyhow::anyhow!(e))
            .and_then(|v| v)
    };
    result.map_err(|e| e.to_string())
}
fn route(core: &Arc<Core>, domain: &str, action: &str, args: &Value) -> Result<Value> {
    match domain {
        "app" => app_action(core, action, args),
        "terminal" => terminal::dispatch(core, action, args),
        "provider_login" => terminal::login(core, action, args),
        "files" => files::dispatch(core, action, args),
        "git" => git::dispatch(core, action, args),
        "history" => history::dispatch(core, action, args),
        "addons" => addons::dispatch(core, action, args),
        "models" => models::dispatch(core, action, args),
        #[cfg(windows)]
        "preview" => preview::dispatch(core, action, args),
        _ => bail!("Uygulama işlemi bulunamadı"),
    }
}
fn app_action(core: &Arc<Core>, action: &str, args: &Value) -> Result<Value> {
    match action {
        "state" => Ok(core.snapshot()),
        "notify" => {
            use tauri_plugin_notification::NotificationExt;
            let sid = string(args, "id")?;
            let s = core.session(sid)?;
            let label = string(args, "label")?;
            let key = (uuid::Uuid::new_v4().as_u128() & 0x7fffffff) as i32;
            core.pending
                .lock()
                .unwrap()
                .insert(format!("notification:{key}"), json!(sid));
            core.app
                .notification()
                .builder()
                .id(key)
                .title(s["name"].as_str().unwrap_or("Workflow"))
                .body(label)
                .show()?;
            Ok(json!(true))
        }
        "preferences" => {
            let mut db = core.db.lock().unwrap();
            for (k, v) in args.as_object().context("Tercih geçersiz")? {
                db["preferences"][k] = v.clone()
            }
            drop(db);
            core.save()?;
            Ok(core.snapshot())
        }
        "provider_usage" => usage::read(core, args),
        "provider_account_add" => accounts::add(core, args),
        "provider_account_verify" => usage::verify_account(core, args),
        "providers" => {
            let mut providers = vec![];
            for (name, label) in [
                ("codex", "Codex"),
                ("claude", "Claude Code"),
                ("copilot", "GitHub Copilot"),
            ] {
                let exe = usage::executable(core, name);
                providers.push(
                    json!({"id":name,"name":label,"installed":exe.is_some(),"executable":exe,"accounts":accounts::list(core,name)}),
                );
            }
            Ok(json!(providers))
        }
        "provider_path" => {
            let provider = string(args, "provider")?;
            if !["codex", "claude", "copilot"].contains(&provider) {
                bail!("Sağlayıcı geçersiz")
            }
            let exe = string(args, "path")?;
            if !Path::new(exe).is_file() {
                bail!("Program dosyası bulunamadı")
            };
            core.db.lock().unwrap()["providers"][provider]["executable"] = json!(exe);
            core.save()?;
            Ok(json!(true))
        }
        "add_project" => {
            let directory = dunce::canonicalize(string(args, "directory")?)?;
            if !directory.is_dir() {
                bail!("Proje klasörü seç")
            }
            core.check_project_directory(&directory)?;
            let directory = directory.to_string_lossy().to_string();
            let mut db = core.db.lock().unwrap();
            let index = db["projects"]
                .as_array()
                .unwrap()
                .iter()
                .position(|p| p["directory"] == directory);
            let project = if let Some(index) = index {
                let previous = db["projects"][index].clone();
                if previous["removed"] != true {
                    return Ok(previous);
                }
                db["projects"][index]["removed"] = json!(false);
                if let Err(error) = core.save_workspace(&db) {
                    db["projects"][index] = previous;
                    return Err(error);
                }
                db["projects"][index].clone()
            } else {
                let project = json!({"id":id(),"name":Path::new(&directory).file_name().unwrap_or_default().to_string_lossy(),"directory":directory,"createdAt":now()});
                db["projects"].as_array_mut().unwrap().push(project.clone());
                if let Err(error) = core.save_workspace(&db) {
                    db["projects"].as_array_mut().unwrap().pop();
                    return Err(error);
                }
                project
            };
            drop(db);
            core.emit("workspace", core.snapshot());
            Ok(project)
        }
        "clone" => {
            let _guard = core.operations.lock().unwrap();
            let address = string(args, "url")?;
            if address.starts_with('-') || address.contains('\n') || address.contains('\r') {
                bail!("Depo adresi geçersiz")
            };
            if address.starts_with("http") {
                let s = url::Url::parse(address)?;
                if !s.username().is_empty() || s.password().is_some() {
                    bail!("Depo adresine kimlik bilgisi ekleme")
                }
            } else if !address.starts_with("git@") && !address.starts_with("ssh://") {
                bail!("HTTPS veya SSH Git adresi kullan")
            }
            let parent = dunce::canonicalize(string(args, "parent")?)?;
            let name = string(args, "name")?;
            if name.is_empty() || name.contains(['\\', '/']) || name == "." || name == ".." {
                bail!("Klasör adı geçersiz")
            };
            let destination = safe_path(&parent, name)?;
            core.check_project_directory(external_path(&destination))?;
            if destination.exists() {
                bail!("Hedef klasör zaten var")
            };
            crate::core::git(
                &parent,
                &[
                    "-c",
                    "http.sslBackend=schannel",
                    "clone",
                    "--progress",
                    "--",
                    address,
                    &destination.to_string_lossy(),
                ],
            )?;
            app_action(core, "add_project", &json!({"directory":destination}))
        }
        "remove_project" => {
            let _operation = core.operations.lock().unwrap();
            let pid = string(args, "id")?;
            let sessions = core.db.lock().unwrap()["sessions"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|s| s["projectId"] == pid)
                .filter_map(|s| s["id"].as_str().map(str::to_string))
                .collect::<Vec<_>>();
            for sid in sessions {
                #[cfg(windows)]
                if core.previews.running(&sid) {
                    bail!("Önce projenin çalışan önizlemelerini durdur")
                }
                if let Some(t) = core.terms.lock().unwrap().get(&sid) {
                    if !t.exited.load(std::sync::atomic::Ordering::SeqCst) {
                        bail!("Önce projenin açık oturumlarını durdur")
                    }
                }
            }
            let mut db = core.db.lock().unwrap();
            let index = db["projects"]
                .as_array()
                .unwrap()
                .iter()
                .position(|p| p["id"] == pid)
                .context("Proje yok")?;
            let previous = db["projects"][index].clone();
            db["projects"][index]["removed"] = json!(true);
            if let Err(error) = core.save_workspace(&db) {
                db["projects"][index] = previous;
                return Err(error);
            }
            drop(db);
            Ok(core.snapshot())
        }
        "create_session" => {
            let _guard = core.operations.lock().unwrap();
            let project = core.project(string(args, "projectId")?)?;
            let provider = string(args, "provider")?;
            if !["codex", "claude", "copilot"].contains(&provider) {
                bail!("Sağlayıcı geçersiz")
            };
            let account = accounts::Profile::get(core, provider, &args["accountId"])?;
            if !account.verified() {
                bail!("Önce bu hesabı bağla.");
            }
            let direct = match args["workspaceMode"].as_str() {
                None | Some("isolated") => false,
                Some("direct") => true,
                _ => bail!("Çalışma alanı seçimi geçersiz"),
            };
            if direct
                && args["addons"]
                    .as_array()
                    .is_some_and(|addons| !addons.is_empty())
            {
                bail!("Oturuma özel eklentiler için ayrı çalışma alanı seç.")
            }
            let sid = id();
            let w = files::create_workspace(core, &project, &sid, direct)?;
            let mut session = json!({"id":sid,"provider":provider,"accountId":account.id,"name":args["name"].as_str().filter(|s|!s.trim().is_empty()).unwrap_or("Yeni oturum"),"projectId":project["id"],"createdAt":now(),"directory":w["directory"],"workspaceKind":w["kind"],"branch":w["branch"],"state":"stopped","tools":args["tools"]!=false,"browserAccess":true,"addons":args["addons"].as_array().cloned().unwrap_or_default(),"login":args["login"]==true,"model":args["model"].as_str().map(str::to_string).or_else(||models::configured(provider,&account))});
            if direct {
                session["baselineRef"] = w["baselineRef"].clone();
            }
            core.db.lock().unwrap()["sessions"]
                .as_array_mut()
                .unwrap()
                .push(session.clone());
            core.save()?;
            core.emit("workspace", core.snapshot());
            Ok(session)
        }
        "conversation_from_login" => {
            let _guard = core.operations.lock().unwrap();
            let sid = string(args, "id")?;
            let session = core.session(sid)?;
            if session["login"] != true {
                return Ok(session);
            }
            if core
                .terms
                .lock()
                .unwrap()
                .get(sid)
                .is_some_and(|t| !t.exited.load(std::sync::atomic::Ordering::SeqCst))
            {
                bail!("Önce hesap girişinin bitmesini bekle veya terminali durdur")
            }
            if !session["conversationId"].is_null() {
                bail!("Bağlı konuşması olan hesap kartı dönüştürülemez")
            }
            core.update_session(
                sid,
                json!({"login":false,"lastUsedAt":null,"runId":null,"exitCode":null}),
            )
        }
        "session_patch" => {
            let sid = string(args, "id")?;
            let mut patch = json!({});
            for k in ["name", "browserAccess", "archived", "position", "size"] {
                if !args[k].is_null() {
                    patch[k] = args[k].clone()
                }
            }
            if patch["archived"] == true {
                let _operation = core.operations.lock().unwrap();
                #[cfg(windows)]
                if core.previews.running(sid) {
                    bail!("Önce bu oturumun önizlemesini durdur")
                }
                if core
                    .terms
                    .lock()
                    .unwrap()
                    .get(sid)
                    .is_some_and(|t| !t.exited.load(std::sync::atomic::Ordering::SeqCst))
                {
                    bail!("Önce oturumu durdur")
                }
                return core.update_session(sid, patch);
            }
            core.update_session(sid, patch)
        }
        "link" => {
            let source = string(args, "source")?;
            let target = string(args, "target")?;
            if source == target {
                bail!("Oturum kendine bağlanamaz")
            };
            let s = core.session(source)?;
            let t = core.session(target)?;
            if s["projectId"] != t["projectId"] || s["archived"] == true || t["archived"] == true {
                bail!("Aynı projedeki açık listelenen oturumları seç")
            };
            let mut db = core.db.lock().unwrap();
            db["links"]
                .as_array_mut()
                .unwrap()
                .retain(|l| !(l["source"] == source && l["target"] == target));
            let link = json!({"id":id(),"source":source,"target":target,"sourceConversation":s["conversationId"],"targetConversation":t["conversationId"],"read":args["read"]==true,"files":args["files"]==true,"work":args["work"]==true});
            db["links"].as_array_mut().unwrap().push(link.clone());
            drop(db);
            core.save()?;
            core.emit("workspace", core.snapshot());
            Ok(link)
        }
        "unlink" => {
            let mut db = core.db.lock().unwrap();
            db["links"]
                .as_array_mut()
                .unwrap()
                .retain(|l| l["id"] != args["id"]);
            drop(db);
            core.save()?;
            core.emit("workspace", core.snapshot());
            Ok(json!(true))
        }
        "job" => {
            let jid = string(args, "id")?;
            let decision = string(args, "decision")?;
            if !["accept", "cancel"].contains(&decision) {
                bail!("Geçersiz karar")
            };
            let mut db = core.db.lock().unwrap();
            let index = db["jobs"]
                .as_array()
                .unwrap()
                .iter()
                .position(|j| j["id"] == jid)
                .context("İstek bulunamadı")?;
            let job = &mut db["jobs"][index];
            if job["state"] != "pending" {
                bail!("İstek durumu değişti")
            };
            let result = if decision == "accept" {
                let target = string(job, "target")?;
                let text=format!("{}\n\nBu işin istek kimliği: {}. Sonucu Workflow report_result aracıyla bu kimliğe bildir.",string(job,"instruction")?,jid);
                let run_id = string(args, "runId")?;
                let terminal = terminal::get(core, &json!({"id":target,"runId":run_id}))?;
                let paste = format!(
                    "\x1b[200~{}\x1b[201~",
                    text.replace(|c: char| c.is_ascii_control(), " ")
                );
                terminal.write(paste.as_bytes())?;
                job["state"] = json!("accepted");
                // A successfully pasted draft cannot be rolled back. Keep the
                // accepted state in memory if writing the workspace fails.
                let saved = core.save_workspace(&db).is_ok();
                json!({"text":text,"saved":saved})
            } else {
                job["state"] = json!("cancelled");
                if let Err(error) = core.save_workspace(&db) {
                    db["jobs"][index]["state"] = json!("pending");
                    return Err(error);
                }
                json!(true)
            };
            drop(db);
            core.emit("workspace", core.snapshot());
            Ok(result)
        }
        "reveal" => {
            use tauri_plugin_opener::OpenerExt;
            let path = if args["path"].is_string() {
                safe_path(&core.root(args)?, string(args, "path")?)?
            } else {
                core.root(args)?
            };
            core.app.opener().reveal_item_in_dir(path)?;
            Ok(json!(true))
        }
        "external" => {
            use tauri_plugin_opener::OpenerExt;
            let address = browser::url(string(args, "url")?)?;
            core.app.opener().open_url(address.as_str(), None::<&str>)?;
            Ok(json!(true))
        }
        "quit" => {
            core.shutdown();
            core.app.exit(0);
            Ok(json!(true))
        }
        _ => bail!("İşlem bulunamadı"),
    }
}
fn fixture(provider: &str) -> Result<()> {
    use std::io::{BufRead, Write};
    println!(
        "Workflow terminal test · {} · PID {}",
        provider,
        std::process::id()
    );
    print!("\r\nReady > ");
    std::io::stdout().flush()?;
    for line in std::io::stdin().lock().lines() {
        let line = line?;
        if let Some(directory) = std::env::var_os("WORKFLOW_TEST_INPUT_DIR") {
            if std::env::var("WORKFLOW_TEST_MODE").ok().as_deref() == Some("1") {
                let directory = PathBuf::from(directory);
                std::fs::create_dir_all(&directory)?;
                let mut file = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(directory.join(format!("{provider}.txt")))?;
                writeln!(file, "{line}")?;
            }
        }
        if line.trim() == "exit" {
            break;
        }
        println!("\r\nReceived: {line}\r\nReady > ");
        std::io::stdout().flush()?;
    }
    Ok(())
}
fn rpc_fixture(provider: &str) -> Result<()> {
    use std::io::{BufRead, Read, Write};
    let profile = account_fixture_directory(provider);
    let login = profile
        .as_ref()
        .and_then(|p| {
            let value = std::fs::read_to_string(p.join("auth-status.json")).ok()?;
            let auth: Value = serde_json::from_str(&value).ok()?;
            auth["login"].as_str().map(str::to_owned)
        })
        .or_else(|| profile.is_none().then(|| "fixture".into()));
    let mut input = std::io::stdin().lock();
    let mut output = std::io::stdout().lock();
    loop {
        let mut line = String::new();
        if input.read_line(&mut line)? == 0 {
            break;
        }
        let v: Value = if provider == "copilot" {
            let Some(length) = line
                .trim()
                .strip_prefix("Content-Length:")
                .and_then(|n| n.trim().parse::<usize>().ok())
            else {
                continue;
            };
            let mut empty = String::new();
            input.read_line(&mut empty)?;
            let mut bytes = vec![0; length];
            input.read_exact(&mut bytes)?;
            serde_json::from_slice(&bytes)?
        } else {
            serde_json::from_str(&line)?
        };
        let request_id = if provider == "claude" {
            v["request_id"]
                .as_str()
                .and_then(|s| s.parse::<u64>().ok())
                .map(Value::from)
                .unwrap_or(Value::Null)
        } else {
            v["id"].clone()
        };
        if request_id.is_null() {
            continue;
        }
        let result = if v["method"] == "account/read" {
            json!({"account":login.as_ref().map(|login| json!({"type":"chatgpt","email":format!("{login}@localhost"),"planType":"test"}))})
        } else if v["method"] == "account/rateLimits/read" {
            json!({"rateLimits":{"primary":{"usedPercent":if profile.is_some() {40} else {25},"windowDurationMins":300,"resetsAt":2000000000},"secondary":{"usedPercent":80,"windowDurationMins":10080,"resetsAt":2000000000}}})
        } else if v["method"] == "auth.getStatus" {
            json!({"isAuthenticated":login.is_some(),"login":login})
        } else if v["method"] == "account.getQuota" {
            json!({"quotaSnapshots":{"premium_interactions":{"remainingPercentage":if profile.is_some() {70} else {40},"resetDate":"2033-05-18T03:33:20Z"},"chat":{"entitlementRequests":-1}}})
        } else if provider == "claude" {
            json!({"models":[{"value":"fixture-claude","displayName":"Fixture Claude"}]})
        } else if v["method"] == "model/list" {
            json!({"data":[{"model":"fixture-codex","displayName":"Fixture Codex","isDefault":true,"defaultReasoningEffort":"medium","supportedReasoningEfforts":[{"reasoningEffort":"medium"}]}]})
        } else if v["method"] == "models.list" {
            json!({"models":[{"id":"fixture-copilot","name":"Fixture Copilot"}]})
        } else {
            json!({})
        };
        let response = if provider == "claude" {
            json!({"type":"control_response","response":{"request_id":request_id.to_string(),"subtype":"success","response":result}})
        } else {
            json!({"jsonrpc":"2.0","id":request_id,"result":result})
        };
        let body = serde_json::to_string(&response)?;
        if provider == "copilot" {
            write!(output, "Content-Length: {}\r\n\r\n{body}", body.len())?
        } else {
            writeln!(output, "{body}")?
        }
        output.flush()?;
    }
    Ok(())
}
fn profile_override() -> Option<std::ffi::OsString> {
    std::env::var_os("WORKFLOW_DATA_DIR").or_else(|| std::env::var_os("AGENTLOOM_DATA_DIR"))
}

fn account_fixture_directory(provider: &str) -> Option<PathBuf> {
    let variable = match provider {
        "codex" => "CODEX_HOME",
        "claude" => "CLAUDE_CONFIG_DIR",
        _ => "COPILOT_HOME",
    };
    let directory = PathBuf::from(std::env::var_os(variable)?);
    directory
        .starts_with(PathBuf::from(profile_override()?).join("accounts"))
        .then_some(directory)
}

fn main() {
    let args = std::env::args().collect::<Vec<_>>();
    if args.get(1).is_some_and(|s| s == "--usage-auth-fixture")
        && std::env::var("WORKFLOW_TEST_MODE").ok().as_deref() == Some("1")
    {
        if let Some(folder) = account_fixture_directory("claude")
            .or_else(|| std::env::var_os("WORKFLOW_TEST_USAGE_DIR").map(PathBuf::from))
        {
            if let Ok(value) = std::fs::read_to_string(folder.join("auth-status.json")) {
                println!("{value}");
            }
        }
        return;
    }
    if args.get(1).is_some_and(|s| s == "mcp") {
        if let Some(file) = args.get(2) {
            if let Err(e) = mcp::serve_stdio(PathBuf::from(file)) {
                eprintln!("Workflow araç bağlantısı kapandı: {e}")
            }
        }
        return;
    }
    if args.get(1).is_some_and(|s| s == "--fixture") {
        let _ = fixture(args.get(2).map(String::as_str).unwrap_or("codex"));
        return;
    }
    if args.get(1).is_some_and(|s| s == "--rpc-fixture")
        && std::env::var("WORKFLOW_TEST_MODE").ok().as_deref() == Some("1")
    {
        let _ = rpc_fixture(args.get(2).map(String::as_str).unwrap_or("codex"));
        return;
    }
    let mut context = tauri::generate_context!();
    // Open the window after selecting and locking the profile.
    let config = context.config_mut().app.windows.first_mut().map(|window| {
        window.create = false;
        window.clone()
    });
    let application=tauri::Builder::default().plugin(tauri_plugin_dialog::init()).plugin(tauri_plugin_opener::init()).plugin(tauri_plugin_notification::init()).invoke_handler(tauri::generate_handler![dispatch]).setup(move|app|{
        let (directory, webview_directory) = if let Some(override_path) = profile_override() {
            let directory = PathBuf::from(override_path);
            let webview_directory = directory.join("ui-profile");
            (directory, webview_directory)
        } else {
            let default = app.path().app_local_data_dir()?;
            let legacy = default.with_file_name("local.agentloom.desktop");
            let directory = if !default.join("workspace.json").exists() && legacy.join("workspace.json").is_file() {
                legacy
            } else {
                default
            };
            (directory.clone(), directory)
        };
        let core = Core::new(directory, app.handle().clone())?;
        mcp::start(&core)?;
        app.manage(core.clone());
        #[cfg(windows)]
        app.manage(teams::Teams::default());
        if let Some(config) = &config {
            tauri::WebviewWindowBuilder::from_config(app, config)?.data_directory(webview_directory).build()?;
        }
        {use tauri_plugin_notification::NotificationExt;let weak=Arc::downgrade(&core);app.notification().on_action(move|event|{if let(Some(core),Some(note))=(weak.upgrade(),event.notification()){let sid=core.pending.lock().unwrap().get(&format!("notification:{}",note.id())).and_then(Value::as_str).map(str::to_string);if let Some(sid)=sid{if let Some(w)=core.app.get_webview_window("main"){let _=w.unminimize();let _=w.set_focus();}core.emit("notification-session",json!(sid));}}})?;}
        if std::env::var("WORKFLOW_TEST_MODE").ok().as_deref()==Some("1"){let _=atomic(&core.directory.join("ready.json"),&serde_json::to_vec(&json!({"pid":std::process::id(),"endpoint":*core.endpoint.lock().unwrap()}))?);}
        Ok(())
    }).build(context);
    let application = match application {
        Ok(app) => app,
        Err(e) => {
            #[cfg(windows)]
            unsafe {
                use windows::{
                    core::{w, PCWSTR},
                    Win32::UI::WindowsAndMessaging::*,
                };
                let text = format!("Workflow açılamadı.\n\n{e}")
                    .encode_utf16()
                    .chain(Some(0))
                    .collect::<Vec<_>>();
                if e.to_string().contains("zaten açık") {
                    if let Ok(window) = FindWindowW(None, w!("Workflow")) {
                        let _ = SetForegroundWindow(window);
                        return;
                    }
                }
                let _ = MessageBoxW(
                    None,
                    PCWSTR(text.as_ptr()),
                    w!("Workflow"),
                    MB_OK | MB_ICONERROR,
                );
            }
            #[cfg(not(windows))]
            eprintln!("Workflow açılamadı: {e}");
            return;
        }
    };
    application.run(|app, event| {
        #[cfg(windows)]
        if let tauri::RunEvent::WindowEvent {
            label,
            event: tauri::WindowEvent::CloseRequested { .. },
            ..
        } = &event {
            if label == "main" {
                teams::close(app);
            }
        }
        if matches!(
            event,
            tauri::RunEvent::ExitRequested { .. } | tauri::RunEvent::Exit
        ) {
            if let Some(core) = app.try_state::<Arc<Core>>() {
                core.shutdown();
            }
        }
    });
}
