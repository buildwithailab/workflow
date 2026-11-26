use crate::{accounts::Profile, core::*, terminal, usage};
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    io::{BufRead, BufReader, Read, Write},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, Command, Stdio},
    sync::{mpsc, Arc, Mutex},
    time::{Duration, Instant, UNIX_EPOCH},
};

const CATALOG_TTL: Duration = Duration::from_secs(120);
const EFFORTS: &[&str] = &[
    "none", "minimal", "low", "medium", "high", "xhigh", "max", "ultra",
];

#[derive(Default)]
struct CachedCatalog {
    loaded: Option<Instant>,
    models: Vec<Value>,
}

#[derive(Default)]
pub(crate) struct CatalogCache(Mutex<HashMap<String, Arc<Mutex<CachedCatalog>>>>);

impl CatalogCache {
    pub(crate) fn clear_idle(&self) {
        self.0.lock().unwrap().retain(|_, entry| Arc::strong_count(entry) > 1);
    }
    fn load(
        &self,
        key: &str,
        force: bool,
        fetch: impl FnOnce() -> Result<Vec<Value>>,
    ) -> Result<Vec<Value>> {
        let slot = {
            let mut entries = self.0.lock().unwrap();
            if !entries.contains_key(key) && entries.len() >= 64 {
                entries.retain(|_, value| Arc::strong_count(value) > 1);
            }
            entries.entry(key.into()).or_default().clone()
        };
        // Coalesce requests for this context without blocking other accounts or projects.
        let mut entry = slot.lock().unwrap();
        if !force && entry.loaded.is_some_and(|at| at.elapsed() < CATALOG_TTL) {
            return Ok(entry.models.clone());
        }
        entry.loaded = None;
        entry.models.clear();
        let models = fetch()?;
        if models.is_empty() {
            bail!("Bu hesap için seçilebilir model bulunamadı");
        }
        entry.models = models.clone();
        entry.loaded = Some(Instant::now());
        Ok(models)
    }
}

fn file_stamp(path: &Path) -> Value {
    std::fs::metadata(path)
        .ok()
        .map(|m| {
            let time = m
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok());
            json!([m.len(), time.map(|t| (t.as_secs(), t.subsec_nanos()))])
        })
        .unwrap_or(Value::Null)
}

fn catalog_context(core: &Core, s: &Value, profile: &Profile) -> Result<(PathBuf, String)> {
    let provider = string(s, "provider")?;
    let custom = core.db.lock().unwrap()["providers"][provider]["executable"]
        .as_str()
        .map(str::to_string);
    let exe = if std::env::var("WORKFLOW_TEST_MODE").as_deref() == Ok("1") {
        std::env::var_os("WORKFLOW_TEST_RPC_EXE")
            .map(PathBuf::from)
            .or_else(|| terminal::executable(provider, custom.as_deref()))
    } else {
        terminal::executable(provider, custom.as_deref())
    }
    .context("CLI kurulumu bulunamadı")?;
    let directory = profile.config_dir();
    // Only metadata is fingerprinted. No account contents enter this in-memory cache.
    let stamps: Vec<_> = [
        "auth.json",
        "auth-status.json",
        ".credentials.json",
        "config.toml",
        "settings.json",
        "config.json",
    ]
    .iter()
    .map(|name| file_stamp(&directory.join(name)))
    .collect();
    let project = Path::new(string(s, "directory")?);
    let project_stamps: Vec<_> = [
        ".claude/settings.json",
        ".claude/settings.local.json",
        ".codex/config.toml",
    ]
    .iter()
    .map(|name| file_stamp(&project.join(name)))
    .collect();
    let identity = core.db.lock().unwrap()["providers"][provider]["accounts"][&profile.id]
        ["identity"]
        .as_str()
        .map(|value| hash(value.as_bytes()));
    let key = json!([
        provider,
        profile.id,
        directory,
        s["directory"],
        exe,
        file_stamp(&exe),
        stamps,
        project_stamps,
        identity
    ])
    .to_string();
    Ok((exe, key))
}

fn normalized_codex_model(model: &Value) -> Option<Value> {
    if model["hidden"] == true || model["show_in_picker"] == false {
        return None;
    }
    let id = model["model"].as_str().or_else(|| model["id"].as_str())?;
    let supported = model
        .get("supportedReasoningEfforts")
        .or_else(|| model.get("supported_reasoning_efforts"));
    let mut efforts = Vec::new();
    for option in supported.and_then(Value::as_array).into_iter().flatten() {
        let level = option
            .as_str()
            .or_else(|| option["reasoningEffort"].as_str())
            .or_else(|| option["effort"].as_str())
            .or_else(|| option["reasoning_effort"].as_str());
        if let Some(level) = level.filter(|level| EFFORTS.contains(level)) {
            if !efforts
                .iter()
                .any(|entry: &Value| entry["reasoningEffort"] == level)
            {
                efforts.push(json!({"reasoningEffort":level}));
            }
        }
    }
    let default = model
        .get("defaultReasoningEffort")
        .or_else(|| model.get("default_reasoning_effort"))
        .and_then(Value::as_str)
        .filter(|level| EFFORTS.contains(level));
    if let Some(level) = default {
        if !efforts
            .iter()
            .any(|entry| entry["reasoningEffort"] == level)
        {
            efforts.push(json!({"reasoningEffort":level}));
        }
    }
    Some(
        json!({"id":id,"name":model.get("displayName").or_else(|| model.get("display_name")).and_then(Value::as_str).unwrap_or(id),
        "default":model["isDefault"] == true || model["is_default"] == true,
        "efforts":efforts,"defaultEffort":default}),
    )
}

fn normalized_claude_model(model: &Value) -> Option<Value> {
    let id = model["value"].as_str()?;
    let efforts: Vec<_> = model["supportedEffortLevels"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .filter(|level| ["low", "medium", "high", "xhigh", "max"].contains(level))
        .filter(|_| model["supportsEffort"] != false)
        .map(|level| json!({"reasoningEffort":level}))
        .collect();
    Some(
        json!({"id":id,"name":model["displayName"].as_str().unwrap_or(id),"efforts":efforts,"defaultEffort":null}),
    )
}

fn normalized_copilot_model(model: &Value) -> Option<Value> {
    if model["hidden"] == true || model["policy"]["state"] == "disabled" {
        return None;
    }
    let id = model["id"].as_str()?;
    let supported = model["supportedReasoningEfforts"].as_array().or_else(|| {
        model
            .pointer("/capabilities/supports/reasoning_effort")
            .and_then(Value::as_array)
    });
    let efforts: Vec<_> = supported
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .filter(|level| ["low", "medium", "high", "xhigh", "max"].contains(level))
        .filter(|_| model.pointer("/capabilities/supports/reasoningEffort") != Some(&json!(false)))
        .map(|level| json!({"reasoningEffort":level}))
        .collect();
    let default = model["defaultReasoningEffort"].as_str().filter(|level| {
        efforts
            .iter()
            .any(|option| option["reasoningEffort"] == *level)
    });
    Some(
        json!({"id":id,"name":model["name"].as_str().unwrap_or(id),"efforts":efforts,"defaultEffort":default}),
    )
}
pub fn configured(provider: &str, profile: &Profile) -> Option<String> {
    let value = if provider == "codex" {
        let contents = std::fs::read_to_string(profile.config_dir().join("config.toml")).ok()?;
        contents
            .lines()
            .take_while(|l| !l.trim_start().starts_with('['))
            .find_map(|line| {
                let (k, v) = line.split_once('=')?;
                (k.trim() == "model").then(|| v.trim().trim_matches(['\'', '"']).to_string())
            })?
    } else {
        let path = profile.config_dir().join(if provider == "claude" {
            "settings.json"
        } else {
            "config.json"
        });
        let v: Value = serde_json::from_slice(&std::fs::read(path).ok()?).ok()?;
        v["model"].as_str()?.to_string()
    };
    (!value.is_empty()
        && value.len() < 120
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "._:/-[]".contains(c)))
    .then_some(value)
}
pub(crate) struct Rpc {
    child: Child,
    #[cfg(target_os = "macos")]
    group: crate::platform::macos::process::Group,
    input: ChildStdin,
    rx: mpsc::Receiver<Value>,
    next: u64,
    framing: String,
}
impl Drop for Rpc {
    fn drop(&mut self) {
        #[cfg(target_os = "macos")]
        self.group.stop();
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            let _ = Command::new("taskkill.exe")
                .args(["/pid", &self.child.id().to_string(), "/t", "/f"])
                .creation_flags(0x08000000)
                .output();
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
impl Rpc {
    pub(crate) fn new(
        provider: &str,
        exe: &std::path::Path,
        directory: &std::path::Path,
        profile: &Profile,
    ) -> Result<Self> {
        let mut args: Vec<String> = match provider {
            "codex" => vec!["app-server", "--listen", "stdio://"],
            "claude" => vec![
                "--print",
                "--input-format",
                "stream-json",
                "--output-format",
                "stream-json",
                "--verbose",
                "--no-session-persistence",
            ],
            _ => vec!["--headless", "--no-auto-update", "--stdio"],
        }
        .into_iter()
        .map(str::to_string)
        .collect();
        if std::env::var("WORKFLOW_TEST_MODE").ok().as_deref() == Some("1")
            && std::env::var_os("WORKFLOW_TEST_RPC_EXE").is_some()
        {
            args = vec!["--rpc-fixture".into(), provider.into()]
        }
        let mut c;
        if cfg!(windows)
            && exe
                .extension()
                .is_some_and(|s| s == "cmd" || s == "bat" || s == "ps1")
        {
            let q = |v: &str| format!("'{}'", v.replace('\'', "''"));
            c = Command::new("powershell.exe");
            c.args([
                "-NoLogo",
                "-NoProfile",
                "-Command",
                &format!(
                    "& {} {};exit $LASTEXITCODE",
                    q(&exe.to_string_lossy()),
                    args.iter().map(|s| q(s)).collect::<Vec<_>>().join(" ")
                ),
            ]);
        } else {
            c = Command::new(exe);
            c.args(args);
        }
        c.current_dir(directory)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        for (k, _) in std::env::vars() {
            if terminal::internal_variable(&k) {
                c.env_remove(k);
            }
        }
        profile.apply(&mut c);
        #[cfg(target_os = "macos")]
        c.env("PATH", crate::platform::macos::child_path(exe)?);
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            c.creation_flags(0x08000000);
        }
        #[cfg(target_os = "macos")]
        crate::platform::macos::process::configure(&mut c);
        let mut child = c.spawn()?;
        #[cfg(target_os = "macos")]
        let group = match crate::platform::macos::process::Group::new(child.id(), false) {
            Ok(group) => group,
            Err(error) => { let _ = child.kill(); let _ = child.wait(); return Err(error); }
        };
        let input = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let framing = if provider == "copilot" {
            "headers"
        } else if provider == "claude" {
            "claude"
        } else {
            "lines"
        };
        let f = framing.to_string();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).ok().unwrap_or(0) == 0 {
                    break;
                }
                if f == "headers" {
                    let Some(length) = line
                        .trim()
                        .strip_prefix("Content-Length:")
                        .and_then(|s| s.trim().parse::<usize>().ok())
                    else {
                        continue;
                    };
                    if length > 4 * 1024 * 1024 {
                        break;
                    }
                    let mut empty = String::new();
                    if reader.read_line(&mut empty).is_err() {
                        break;
                    }
                    let mut body = vec![0; length];
                    if reader.read_exact(&mut body).is_err() {
                        break;
                    }
                    if let Ok(v) = serde_json::from_slice::<Value>(&body) {
                        let _ = tx.send(v);
                    }
                } else if let Ok(mut v) = serde_json::from_str::<Value>(&line) {
                    if f == "claude" {
                        if v["type"] != "control_response" {
                            continue;
                        }
                        v = json!({"id":v["response"]["request_id"].as_str().and_then(|s|s.parse::<u64>().ok()),"result":v["response"]["response"],"error":if v["response"]["subtype"]=="success"{Value::Null}else{json!(true)}});
                    }
                    let _ = tx.send(v);
                }
            }
        });
        Ok(Self {
            child,
            #[cfg(target_os = "macos")]
            group,
            input,
            rx,
            next: 0,
            framing: framing.into(),
        })
    }
    pub(crate) fn send(&mut self, v: Value) -> Result<()> {
        let body = serde_json::to_string(&v)?;
        if self.framing == "headers" {
            write!(self.input, "Content-Length: {}\r\n\r\n{}", body.len(), body)?
        } else {
            writeln!(self.input, "{body}")?
        }
        self.input.flush()?;
        Ok(())
    }
    pub(crate) fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        self.next += 1;
        let id = self.next;
        let msg = if self.framing == "claude" {
            let mut request = params;
            request["subtype"] = json!(method);
            json!({"type":"control_request","request_id":id.to_string(),"request":request})
        } else {
            json!({"jsonrpc":"2.0","id":id,"method":method,"params":params})
        };
        self.send(msg)?;
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            let v = self
                .rx
                .recv_timeout(remaining)
                .context("CLI yanıtı alınamadı; girişini ve sürümünü kontrol et")?;
            if v["id"] == id {
                if !v["error"].is_null() {
                    bail!("Sağlayıcı model listesini döndürmedi")
                }
                return Ok(v["result"].clone());
            }
        }
    }
}
pub fn dispatch(core: &Arc<Core>, action: &str, args: &Value) -> Result<Value> {
    let sid = string(args, "id")?;
    let s = core.session(sid)?;
    let provider = string(&s, "provider")?;
    if action == "list" {
        let profile = Profile::get(core, provider, &s["accountId"])?;
        let (exe, context) = catalog_context(core, &s, &profile)?;
        let models = core.model_catalog.load(&context, args["refresh"] == true, || {
        usage::require_account(core, provider, &profile)?;
        let mut rpc = Rpc::new(
            provider,
            &exe,
            std::path::Path::new(string(&s, "directory")?),
            &profile,
        )?;
        let models = match provider {
            "codex" => {
                rpc.request("initialize",json!({"clientInfo":{"name":"workflow","version":"0.1.0"},"capabilities":{"experimentalApi":false}}))?;
                rpc.send(json!({"method":"initialized"}))?;
                let r = rpc.request("model/list", json!({"limit":100,"includeHidden":false}))?;
                r["data"].as_array().into_iter().flatten().filter_map(normalized_codex_model).collect::<Vec<_>>()
            }
            "claude" => {
                let r = rpc.request("initialize", json!({}))?;
                r["models"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default()
                    .iter()
                    .filter_map(normalized_claude_model)
                    .collect()
            }
            _ => {
                rpc.request(
                    "connect",
                    json!({"clientInfo":{"editorName":"Workflow","editorVersion":"0.1.0"}}),
                )?;
                let r = rpc.request("models.list", json!({}))?;
                r["models"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default()
                    .iter()
                    .filter_map(normalized_copilot_model)
                    .collect()
            }
        };
        Ok(models)
        }).map_err(|error| {
            core.pending.lock().unwrap().remove(&format!("models:{sid}"));
            error
        })?;
        let current = core.session(sid)?;
        let current_profile =
            Profile::get(core, string(&current, "provider")?, &current["accountId"])?;
        if catalog_context(core, &current, &current_profile)?.1 != context {
            bail!("Model listesi değişti; listeyi yeniden aç.");
        }
        core.pending.lock().unwrap().insert(
            format!("models:{sid}"),
            json!({"context":context,"models":models}),
        );
        return Ok(json!(models));
    }
    if action == "apply" {
        let model = string(args, "model")?;
        if model.is_empty()
            || model.len() > 120
            || !model
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "._:/-[]".contains(c))
        {
            bail!("Model kimliği geçersiz")
        };
        let cached = core
            .pending
            .lock()
            .unwrap()
            .get(&format!("models:{sid}"))
            .cloned()
            .context("Önce model listesini aç")?;
        let profile = Profile::get(core, provider, &s["accountId"])?;
        if cached["context"].as_str() != Some(catalog_context(core, &s, &profile)?.1.as_str()) {
            bail!("Model listesi değişti; listeyi yeniden aç.");
        }
        let model_info = cached["models"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["id"] == model)
            .context("Model bu hesapta listelenmedi")?;
        let effort = args["effort"].as_str();
        if matches!(provider, "codex" | "claude" | "copilot")
            && effort.is_some_and(|value| {
                ![
                    "none", "minimal", "low", "medium", "high", "xhigh", "max", "ultra",
                ]
                .contains(&value)
                    || !model_info["efforts"].as_array().is_some_and(|options| {
                        options
                            .iter()
                            .any(|option| option["reasoningEffort"] == value)
                    }) && model_info["defaultEffort"] != value
            })
        {
            bail!("Düşünme seviyesi geçersiz");
        }
        let live = core
            .terms
            .lock()
            .unwrap()
            .get(sid)
            .cloned()
            .filter(|t| !t.exited.load(std::sync::atomic::Ordering::SeqCst));
        if args["surface"] == "chat" {
            if provider != "codex" || s["login"] == true || s["archived"] == true {
                bail!("Sohbet bu oturumda kullanılamıyor.");
            }
            if live.is_some() {
                terminal::apply_chat_model(core, args, model, effort)?;
            } else if !args["runId"].is_null() {
                bail!("Terminal değişti; ajan hazır olunca yeniden seç");
            }
            return core.update_session(sid, json!({"model":model,"effort":effort}));
        }
        if let Some(live) = live {
            if args["runId"] != live.run_id
                || args["seq"].as_u64() != Some(*live.seq.lock().unwrap())
            {
                bail!("Terminal değişti; ajan hazır olunca yeniden seç")
            };
            let command = if provider == "codex" {
                if let Some(effort) = effort {
                    format!("/model {model} --reasoning={effort}\r")
                } else {
                    format!("/model {model}\r")
                }
            } else if provider == "copilot" {
                format!("/model --session {model}\r")
            } else {
                format!("/model {model}\r")
            };
            live.write(command.as_bytes())?;
        }
        return core.update_session(sid, json!({"model":model,"effort":args["effort"]}));
    }
    bail!("Model işlemi geçersiz")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn catalogs_reuse_success_but_refresh_and_context_changes_fetch_again() {
        let cache = CatalogCache::default();
        let calls = AtomicUsize::new(0);
        let fetch = || {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok(vec![json!({"id":"sample-model"})])
        };
        cache.load("account-a/project-a", false, fetch).unwrap();
        cache.load("account-a/project-a", false, fetch).unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        cache.load("account-a/project-a", true, fetch).unwrap();
        cache.load("account-b/project-a", false, fetch).unwrap();
        cache.load("account-a/project-b", false, fetch).unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 4);
        let slot = cache.0.lock().unwrap()["account-a/project-a"].clone();
        slot.lock().unwrap().loaded = Some(Instant::now() - CATALOG_TTL);
        cache.load("account-a/project-a", false, fetch).unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 5);
        assert!(cache
            .load("account-a/project-a", true, || bail!("unavailable"))
            .is_err());
        cache.load("account-a/project-a", false, fetch).unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 6);
    }

    #[test]
    fn concurrent_catalogs_coalesce_without_locking_another_context() {
        let cache = Arc::new(CatalogCache::default());
        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let first_cache = cache.clone();
        let first = std::thread::spawn(move || {
            first_cache
                .load("first", false, || {
                    started_tx.send(()).unwrap();
                    release_rx.recv_timeout(Duration::from_secs(5)).unwrap();
                    Ok(vec![json!({"id":"first-model"})])
                })
                .unwrap()
        });
        started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let second_cache = cache.clone();
        let coalesced = std::thread::spawn(move || {
            second_cache
                .load("first", false, || panic!("duplicate CLI request"))
                .unwrap()
        });
        cache
            .load("independent", false, || {
                Ok(vec![json!({"id":"independent-model"})])
            })
            .unwrap();
        release_tx.send(()).unwrap();
        assert_eq!(first.join().unwrap(), coalesced.join().unwrap());
    }

    #[test]
    fn effort_metadata_accepts_reported_shapes_without_inventing_capabilities() {
        let legacy = normalized_codex_model(&json!({"model":"sample","supported_reasoning_efforts":[{"effort":"high"},"medium",{"reasoningEffort":"unknown"}],"default_reasoning_effort":"high"})).unwrap();
        assert_eq!(
            legacy["efforts"],
            json!([{"reasoningEffort":"high"},{"reasoningEffort":"medium"}])
        );
        let claude = normalized_claude_model(&json!({"value":"sample","supportsEffort":true,"supportedEffortLevels":["low","high","xhigh","max","invalid"]})).unwrap();
        assert_eq!(claude["efforts"].as_array().unwrap().len(), 4);
        for model in [
            json!({"value":"sample"}),
            json!({"value":"sample","supportsEffort":false,"supportedEffortLevels":["high"]}),
        ] {
            assert!(normalized_claude_model(&model).unwrap()["efforts"]
                .as_array()
                .unwrap()
                .is_empty());
        }
    }

    #[test]
    fn copilot_efforts_follow_reported_capabilities_and_model_policy() {
        let model = normalized_copilot_model(&json!({"id":"sample-model","supportedReasoningEfforts":["low","medium","high","invalid"],"defaultReasoningEffort":"medium"})).unwrap();
        assert_eq!(model["efforts"].as_array().unwrap().len(), 3);
        assert_eq!(model["defaultEffort"], "medium");
        let legacy = normalized_copilot_model(&json!({"id":"sample-model","capabilities":{"supports":{"reasoning_effort":["high","max"]}},"defaultReasoningEffort":"low"})).unwrap();
        assert_eq!(legacy["efforts"].as_array().unwrap().len(), 2);
        assert!(legacy["defaultEffort"].is_null());
        assert!(normalized_copilot_model(
            &json!({"id":"sample-model","policy":{"state":"disabled"}})
        )
        .is_none());
        assert!(normalized_copilot_model(&json!({"id":"sample-model","hidden":true})).is_none());
        assert!(
            normalized_copilot_model(&json!({"id":"sample-model"})).unwrap()["efforts"]
                .as_array()
                .unwrap()
                .is_empty()
        );
    }
}
