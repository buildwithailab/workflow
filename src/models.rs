use crate::{accounts::Profile, core::*, terminal, usage};
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::{
    io::{BufRead, BufReader, Read, Write},
    process::{Child, ChildStdin, Command, Stdio},
    sync::{mpsc, Arc},
    time::Duration,
};
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
    input: ChildStdin,
    rx: mpsc::Receiver<Value>,
    next: u64,
    framing: String,
}
impl Drop for Rpc {
    fn drop(&mut self) {
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
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            c.creation_flags(0x08000000);
        }
        let mut child = c.spawn()?;
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
        usage::require_account(core, provider, &profile)?;
        let custom = core.db.lock().unwrap()["providers"][provider]["executable"]
            .as_str()
            .map(str::to_string);
        let exe = if std::env::var("WORKFLOW_TEST_MODE").ok().as_deref() == Some("1") {
            std::env::var_os("WORKFLOW_TEST_RPC_EXE")
                .map(std::path::PathBuf::from)
                .or_else(|| terminal::executable(provider, custom.as_deref()))
        } else {
            terminal::executable(provider, custom.as_deref())
        }
        .context("CLI kurulumu bulunamadı")?;
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
                r["data"].as_array().cloned().unwrap_or_default().into_iter().filter(|m|m["hidden"]!=true).map(|m|json!({"id":m["model"],"name":m["displayName"],"default":m["isDefault"],"efforts":m["supportedReasoningEfforts"],"defaultEffort":m["defaultReasoningEffort"]})).collect::<Vec<_>>()
            }
            "claude" => {
                let r = rpc.request("initialize", json!({}))?;
                r["models"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default()
                    .iter()
                    .map(|m| json!({"id":m["value"],"name":m["displayName"],"efforts":[]}))
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
                    .filter(|m| m["policy"]["state"] != "disabled" && m["hidden"] != true)
                    .map(|m| json!({"id":m["id"],"name":m["name"],"efforts":[]}))
                    .collect()
            }
        };
        if models.is_empty() {
            bail!("Bu hesap için seçilebilir model bulunamadı")
        };
        core.pending
            .lock()
            .unwrap()
            .insert(format!("models:{sid}"), json!(models));
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
        cached
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["id"] == model)
            .context("Model bu hesapta listelenmedi")?;
        let live = core
            .terms
            .lock()
            .unwrap()
            .get(sid)
            .cloned()
            .filter(|t| !t.exited.load(std::sync::atomic::Ordering::SeqCst));
        if let Some(live) = live {
            if args["runId"] != live.run_id
                || args["seq"].as_u64() != Some(*live.seq.lock().unwrap())
            {
                bail!("Terminal değişti; ajan hazır olunca yeniden seç")
            };
            let command = if provider == "codex" {
                let effort = args["effort"].as_str().unwrap_or("medium");
                if ![
                    "none", "minimal", "low", "medium", "high", "xhigh", "max", "ultra",
                ]
                .contains(&effort)
                {
                    bail!("Düşünme seviyesi geçersiz")
                };
                format!("/model {model} --reasoning={effort}\r")
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
