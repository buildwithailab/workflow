//! User-configured quota queries. Raw output and commands never enter quota IPC.
use crate::{accounts::Profile, core::*, provider_network, terminal, usage};
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    io::Read,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

const FAILED: &str = "Özel kota sorgusu başarısız. Komutu ve sağlayıcı girişini kontrol et.";
const TIMED_OUT: &str = "Özel kota sorgusu zaman aşımına uğradı.";
const INVALID: &str =
    "Kota çıktısı tanınmadı. JSON kota alanları veya Spend/Budget satırları gerekli.";
pub(crate) const LIMIT: usize = 64 * 1024;

pub(crate) trait Stop: Send + Sync {
    fn stop(&self);
}
impl Stop for terminal::LiveTerminal {
    fn stop(&self) {
        terminal::LiveTerminal::stop(self);
    }
}
#[derive(Default)]
struct State {
    closed: bool,
    probes: HashMap<String, Option<Arc<dyn Stop>>>,
}
#[derive(Default)]
struct Cached {
    at: Option<Instant>,
    value: Value,
}
#[derive(Default)]
pub(crate) struct Queries {
    state: Mutex<State>,
    cache: Mutex<HashMap<String, Arc<Mutex<Cached>>>>,
}
pub(crate) struct Registration<'a> {
    owner: &'a Queries,
    id: String,
}
impl Drop for Registration<'_> {
    fn drop(&mut self) {
        let probe = self.owner.state.lock().unwrap().probes.remove(&self.id);
        if let Some(Some(probe)) = probe {
            probe.stop();
        }
    }
}
impl Registration<'_> {
    pub(crate) fn check(&self) -> Result<()> {
        let state = self.owner.state.lock().unwrap();
        if state.closed || !state.probes.contains_key(&self.id) {
            bail!(FAILED);
        }
        Ok(())
    }
    pub(crate) fn attach(&self, probe: Arc<dyn Stop>) -> Result<()> {
        let mut state = self.owner.state.lock().unwrap();
        if state.closed || !state.probes.contains_key(&self.id) {
            drop(state);
            probe.stop();
            bail!(FAILED);
        }
        state.probes.insert(self.id.clone(), Some(probe));
        Ok(())
    }
}
impl Queries {
    pub(crate) fn running(&self) -> bool {
        !self.state.lock().unwrap().probes.is_empty()
    }
    fn reserve(&self) -> Result<Registration<'_>> {
        let mut state = self.state.lock().unwrap();
        if state.closed || state.probes.len() >= 4 {
            drop(state);
            bail!("Kota sorgusu şu anda kullanılamıyor. Biraz sonra yeniden dene.");
        }
        let id = id();
        state.probes.insert(id.clone(), None);
        Ok(Registration { owner: self, id })
    }
    pub(crate) fn shutdown(&self) {
        let probes = {
            let mut state = self.state.lock().unwrap();
            state.closed = true;
            std::mem::take(&mut state.probes)
        };
        for probe in probes.into_values().flatten() {
            probe.stop();
        }
    }
    pub(crate) fn clear_idle(&self) {
        self.cache
            .lock()
            .unwrap()
            .retain(|_, slot| Arc::strong_count(slot) > 1);
    }
    fn cached(&self, key: String, force: bool, fetch: impl FnOnce() -> Value) -> Value {
        let slot = {
            let mut cache = self.cache.lock().unwrap();
            if cache.len() >= 64 {
                cache.retain(|_, slot| Arc::strong_count(slot) > 1);
            }
            if cache.len() >= 64 && !cache.contains_key(&key) {
                return unavailable(FAILED);
            }
            cache.entry(key).or_default().clone()
        };
        let mut cached = slot.lock().unwrap();
        if !force
            && cached.at.is_some_and(|at| {
                at.elapsed()
                    < Duration::from_secs(if cached.value["state"] == "ready" {
                        60
                    } else {
                        10
                    })
            })
        {
            return cached.value.clone();
        }
        cached.value = fetch();
        cached.at = Some(Instant::now());
        cached.value.clone()
    }
}

fn settings(core: &Core) -> Value {
    core.db.lock().unwrap()["preferences"]["providerUsageCommands"].clone()
}
fn validate(value: &Value) -> Result<()> {
    let object = value.as_object().context("Kota komutları geçersiz.")?;
    for (provider, setting) in object {
        if !["codex", "claude", "copilot"].contains(&provider.as_str()) || !setting.is_object() {
            bail!("Kota komutları geçersiz.");
        }
        let command = string(setting, "command")?;
        if command.len() > 4096
            || command.contains('\0')
            || !["provider", "shell"].contains(&string(setting, "kind")?)
        {
            bail!("Kota komutu geçersiz.");
        }
        if !command.trim().is_empty() && setting["kind"] == "provider" {
            if !command.starts_with('/')
                || command.chars().any(char::is_control)
                || command.len() > 1024
            {
                bail!("Sağlayıcı komutu / ile başlamalı ve tek satır olmalı.");
            }
        }
    }
    Ok(())
}
pub(crate) fn dispatch(core: &Core, action: &str, args: &Value) -> Result<Value> {
    if action == "read" {
        let value = settings(core);
        return Ok(if value.is_object() { value } else { json!({}) });
    }
    if action != "save" {
        bail!("Kota ayarı işlemi geçersiz.");
    }
    let value = &args["commands"];
    validate(value)?;
    let mut db = core.db.lock().unwrap();
    let mut updated = db.clone();
    updated["preferences"]["providerUsageCommands"] = value.clone();
    core.save_workspace(&updated)?;
    *db = updated;
    Ok(json!(true))
}

pub(crate) fn configured(core: &Core, provider: &str) -> Option<Value> {
    let value = settings(core)[provider].clone();
    value["command"]
        .as_str()
        .is_some_and(|s| !s.trim().is_empty())
        .then_some(value)
}
fn directory(core: &Core, provider: &str, profile: &Profile, kind: &str) -> Result<PathBuf> {
    if provider == "codex" || kind == "shell" {
        let relative = format!("quota-workspaces/{provider}/{}", profile.id);
        let path = safe_path(&core.directory, &relative)?;
        std::fs::create_dir_all(&path)?;
        return safe_path(&core.directory, &relative);
    }
    let db = core.db.lock().unwrap();
    Ok(db["sessions"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|s| {
            s["provider"] == provider
                && s["archived"] != true
                && s["accountId"].as_str().unwrap_or("default") == profile.id
                && s["directory"]
                    .as_str()
                    .is_some_and(|p| Path::new(p).is_dir())
        })
        .and_then(|s| s["directory"].as_str())
        .map(PathBuf::from)
        .or_else(|| {
            db["projects"]
                .as_array()
                .into_iter()
                .flatten()
                .find_map(|p| {
                    p["directory"]
                        .as_str()
                        .filter(|p| Path::new(p).is_dir())
                        .map(PathBuf::from)
                })
        })
        .unwrap_or_else(|| core.directory.clone()))
}

// An application-reserved UUID namespace keeps diagnostic native conversations
// out of both discovery and manual history, including after application restart.
const PROBE_NAMESPACE: [u8; 8] = [0x57, 0x46, 0x51, 0x54, 0x41, 0x50, 0x4a, 0x71];
pub(crate) fn conversation_id() -> String {
    let mut bytes = uuid::Uuid::new_v4().into_bytes();
    bytes[..8].copy_from_slice(&PROBE_NAMESPACE);
    uuid::Uuid::from_bytes(bytes).to_string()
}
pub(crate) fn is_probe_conversation(id: &str) -> bool {
    uuid::Uuid::parse_str(id).is_ok_and(|id| id.as_bytes()[..8] == PROBE_NAMESPACE)
}
fn unavailable(message: &str) -> Value {
    json!({"state":"unavailable","connection":"unknown","source":"custom","message":message,"windows":[],"updatedAt":now()})
}
pub(crate) fn read(core: &Arc<Core>, args: &Value, profile: &Profile, setting: &Value) -> Value {
    let provider = args["provider"].as_str().unwrap_or("");
    if validate(&json!({(provider):setting})).is_err() {
        return unavailable(FAILED);
    }
    let command = setting["command"].as_str().unwrap_or("");
    let kind = setting["kind"].as_str().unwrap_or("");
    // Reserve before auth, PAC or spawning, under the same gate as data reset.
    let reserved = {
        let _operation = core.operations.lock().unwrap();
        core.custom_usage.reserve()
    };
    let Ok(reserved) = reserved else {
        return unavailable(FAILED);
    };
    let Ok(directory) = directory(core, provider, profile, kind) else {
        return unavailable(FAILED);
    };
    if usage::require_account(core, provider, profile).is_err() {
        return unavailable(FAILED);
    }
    let before = usage::custom_connection(core, provider, profile).ok();
    let executable = usage::executable(core, provider);
    let configured_executable =
        core.db.lock().unwrap()["providers"][provider]["executable"].clone();
    let executable_stamp = executable
        .as_ref()
        .and_then(|path| std::fs::metadata(path).ok())
        .map(|m| (m.len(), m.modified().ok()));
    let verified = core.db.lock().unwrap()["providers"][provider]["accounts"][&profile.id]
        ["identity"]
        .as_str()
        .map(|id| hash(id.as_bytes()));
    let auth_stamps: Vec<_> = [
        "auth.json",
        "auth-status.json",
        ".credentials.json",
        "config.json",
    ]
    .iter()
    .map(|name| {
        std::fs::metadata(profile.config_dir().join(name))
            .ok()
            .map(|m| (m.len(), m.modified().ok()))
    })
    .collect();
    let key = hash(
        format!(
            "{provider}|{}|{directory:?}|{kind}|{command}|{auth_stamps:?}|{before:?}|{executable:?}|{executable_stamp:?}|{configured_executable:?}|{verified:?}",
            profile.id
        )
        .as_bytes(),
    );
    let value=core.custom_usage.cached(key,args["refresh"]==true || before.as_ref().is_none_or(|identity|identity.0!="connected" || identity.1.is_none()),|| {
        let connection=before.as_ref().map(|value|value.0.clone()).unwrap_or_else(||"unknown".into());
        let result=(||->Result<Value>{
            reserved.check()?;
            let text=if kind=="provider" {
                let exe=executable.as_ref().context(FAILED)?;
                terminal::usage_probe(core,provider,profile,exe,&directory,command,&reserved)?
            }else{shell(provider,profile,&directory,command,&reserved,executable.as_deref())?};
            let mut value=parse(&text)?;
            if configured(core,provider).as_ref()!=Some(setting){bail!(FAILED);}
            if usage::executable(core,provider)!=executable || core.db.lock().unwrap()["providers"][provider]["executable"]!=configured_executable {bail!(FAILED);}
            let current=Profile::get(core,provider,&args["accountId"])?;
            usage::require_account(core,provider,&current)?;
            let after=usage::custom_connection(core,provider,&current).ok();
            if before!=after {bail!("Hesap değişti. Kullanımı yenile.");}
            value["connection"]=json!(before.as_ref().map(|v|v.0.as_str()).unwrap_or("unknown"));
            value["source"]=json!("custom");value["updatedAt"]=json!(now());Ok(value)
        })();
        match result {Ok(value)=>value,Err(error)=>{let text=error.to_string();let mut value=unavailable(if [TIMED_OUT,INVALID,"Özel kota komutu giriş veya onay bekliyor. Sağlayıcıdaki durumu kontrol et.","Hesap değişti. Kullanımı yenile."].contains(&text.as_str()){&text}else{FAILED});if text!="Hesap değişti. Kullanımı yenile."{value["connection"]=json!(connection);}value}}
    });
    if configured(core, provider).as_ref() != Some(setting)
        || Profile::get(core, provider, &args["accountId"]).is_err()
        || usage::executable(core, provider) != executable
        || core.db.lock().unwrap()["providers"][provider]["executable"] != configured_executable
    {
        return unavailable(FAILED);
    }
    value
}

struct Process {
    child: Mutex<Child>,
    #[cfg(windows)]
    job: Mutex<Option<terminal::Job>>,
    #[cfg(target_os = "macos")]
    group: crate::platform::macos::process::Group,
}
impl Stop for Process {
    fn stop(&self) {
        #[cfg(windows)]
        {
            self.job.lock().unwrap().take();
        }
        #[cfg(target_os = "macos")]
        self.group.stop();
        let _ = self.child.lock().unwrap().kill();
    }
}
impl Drop for Process {
    fn drop(&mut self) {
        self.stop();
        let _ = self.child.lock().unwrap().wait();
    }
}
fn shell(
    provider: &str,
    profile: &Profile,
    directory: &Path,
    script: &str,
    reserved: &Registration<'_>,
    _provider_executable: Option<&Path>,
) -> Result<String> {
    #[cfg(windows)]
    let mut command = {
        use base64::Engine;
        let executable = std::env::var_os("SystemRoot")
            .map(PathBuf::from)
            .context(FAILED)?
            .join("System32/WindowsPowerShell/v1.0/powershell.exe");
        let script =
            format!("[Console]::OutputEncoding=[Text.UTF8Encoding]::new($false)\n{script}");
        let encoded = base64::engine::general_purpose::STANDARD.encode(
            script
                .encode_utf16()
                .flat_map(u16::to_le_bytes)
                .collect::<Vec<_>>(),
        );
        let mut command = Command::new(executable);
        command.args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-EncodedCommand",
            &encoded,
        ]);
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000);
        command
    };
    #[cfg(not(windows))]
    let mut command = {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", script]);
        command
    };
    profile.apply(&mut command);
    #[cfg(target_os = "macos")]
    command.env("PATH", crate::platform::macos::child_path(
        _provider_executable.unwrap_or_else(|| Path::new("/bin/sh")),
    )?);
    provider_network::prepare(provider, profile)?.apply(&mut command);
    for (key, _) in std::env::vars() {
        if terminal::internal_variable(&key) {
            command.env_remove(key);
        }
    }
    command
        .current_dir(external_path(directory))
        .env("WORKFLOW_PROVIDER", provider)
        .env("WORKFLOW_ACCOUNT_ID", &profile.id)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    #[cfg(target_os = "macos")]
    crate::platform::macos::process::configure(&mut command);
    reserved.check()?;
    let mut child = command.spawn().map_err(|_| anyhow::anyhow!(FAILED))?;
    #[cfg(windows)]
    let job = match terminal::Job::new(child.id()) {
        Ok(job) => job,
        Err(_) => {
            let _ = child.kill();
            let _ = child.wait();
            bail!(FAILED);
        }
    };
    #[cfg(target_os = "macos")]
    let group = match crate::platform::macos::process::Group::new(child.id(), false) {
        Ok(group) => group,
        Err(_) => {
            let _ = child.kill();
            let _ = child.wait();
            bail!(FAILED);
        }
    };
    let stdout = child.stdout.take().context(FAILED)?;
    let process = Arc::new(Process {
        child: Mutex::new(child),
        #[cfg(windows)]
        job: Mutex::new(Some(job)),
        #[cfg(target_os = "macos")]
        group,
    });
    reserved.attach(process.clone())?;
    let reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        stdout
            .take(LIMIT as u64 + 1)
            .read_to_end(&mut bytes)
            .map(|_| bytes)
    });
    let start = Instant::now();
    let status = loop {
        match process.child.lock().unwrap().try_wait() {
            Ok(Some(status)) => break Ok(status),
            Ok(None) => {}
            Err(_) => break Err(anyhow::anyhow!(FAILED)),
        }
        if start.elapsed() > Duration::from_secs(25) {
            break Err(anyhow::anyhow!(TIMED_OUT));
        }
        std::thread::sleep(Duration::from_millis(30));
    };
    process.stop();
    let bytes = reader
        .join()
        .map_err(|_| anyhow::anyhow!(FAILED))?
        .map_err(|_| anyhow::anyhow!(FAILED))?;
    if !status?.success() || bytes.len() > LIMIT {
        bail!(FAILED);
    }
    let bytes = bytes.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(&bytes);
    String::from_utf8(bytes.to_vec()).map_err(|_| anyhow::anyhow!(INVALID))
}

fn amount(text: &str) -> Option<f64> {
    let text = text.trim();
    let text = text.strip_prefix(['$', '€', '£']).unwrap_or(text).trim();
    let mut decimals = text.split('.');
    let whole = decimals.next()?;
    if let Some(fraction) = decimals.next() {
        if fraction.is_empty()
            || !fraction.bytes().all(|b| b.is_ascii_digit())
            || decimals.next().is_some()
        {
            return None;
        }
    }
    let groups: Vec<_> = whole.split(',').collect();
    if groups
        .iter()
        .any(|s| s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()))
        || groups.len() > 1 && (groups[0].len() > 3 || groups[1..].iter().any(|s| s.len() != 3))
    {
        return None;
    }
    let raw = text.replace(',', "");
    let value: f64 = raw.parse().ok()?;
    (value.is_finite() && value >= 0. && value <= 1e15).then_some(value)
}
pub(crate) fn parse(text: &str) -> Result<Value> {
    if text.len() > LIMIT {
        bail!(INVALID);
    }
    let text = text.trim();
    let json = serde_json::from_str::<Value>(text).ok();
    let mut windows = Vec::new();
    if let Some(values) = json.as_ref().and_then(|v| v["windows"].as_array()) {
        if values.len() > 8 {
            bail!(INVALID);
        }
        for (index, value) in values.iter().enumerate() {
            let left = value["remainingPercent"]
                .as_f64()
                .or_else(|| value["usedPercent"].as_f64().map(|used| 100. - used))
                .filter(|v| v.is_finite() && (0.0..=100.0).contains(v))
                .context(INVALID)?;
            let label = value["label"].as_str().unwrap_or("Kota");
            if label.len() > 80 || label.chars().any(char::is_control) {
                bail!(INVALID);
            }
            windows.push(json!({"id":format!("custom-{index}"),"label":label,"usedPercent":100.-left,"remainingPercent":left}));
        }
    } else {
        let (mut spent, mut budget) = (None, None);
        if let Some(value) = json.as_ref() {
            spent = value["spent"]
                .as_f64()
                .filter(|n| n.is_finite() && *n >= 0.);
            budget = value["budget"]
                .as_f64()
                .filter(|n| n.is_finite() && *n > 0.);
        } else {
            for line in text.lines() {
                let line = line.trim().trim_start_matches(['│', '|']).trim();
                for (label, target) in [("Spend", &mut spent), ("Budget", &mut budget)] {
                    if let Some(rest) = line
                        .strip_prefix(label)
                        .filter(|rest| rest.starts_with(|c: char| c.is_whitespace() || c == ':'))
                    {
                        let value =
                            amount(rest.trim_start().trim_start_matches(':')).context(INVALID)?;
                        if target.replace(value).is_some() {
                            bail!(INVALID);
                        }
                    }
                }
            }
        }
        let (spent, budget) = (
            spent.context(INVALID)?,
            budget.filter(|n| *n > 0.).context(INVALID)?,
        );
        if spent > 1e15 || budget > 1e15 {
            bail!(INVALID);
        }
        let used = spent / budget * 100.;
        if !used.is_finite() {
            bail!(INVALID);
        }
        windows.push(json!({"id":"custom-budget","label":"Bütçe","usedPercent":used.min(100.),"remainingPercent":(100.-used).clamp(0.,100.),"spent":spent,"budget":budget}));
    }
    if windows.is_empty() {
        bail!(INVALID);
    }
    Ok(json!({"state":"ready","windows":windows}))
}

pub(crate) fn terminal_payload(text: &str) -> Result<String> {
    if text.len() > LIMIT {
        bail!(INVALID);
    }
    if parse(text).is_ok() {
        return Ok(text.to_owned());
    }
    let (mut offset, mut consumed, mut result) = (0, 0, None);
    for line in text.split_inclusive('\n') {
        let trimmed = line.trim_start();
        let start = offset + line.len() - trimmed.len();
        offset += line.len();
        // Do not interpret JSON embedded in an echoed command or user prompt.
        if start < consumed || !trimmed.starts_with('{') {
            continue;
        }
        let mut stream = serde_json::Deserializer::from_str(&text[start..]).into_iter::<Value>();
        if let Some(Ok(value)) = stream.next() {
            consumed = start + stream.byte_offset();
            if !value.is_object()
                || !["windows", "spent", "budget"]
                    .iter()
                    .any(|key| value.get(key).is_some())
            {
                continue;
            }
            let payload = serde_json::to_string(&value)?;
            parse(&payload)?;
            if result.replace(payload).is_some() {
                bail!(INVALID);
            }
        }
    }
    result.context(INVALID)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn measured_budget_and_explicit_json_are_normalized_without_raw_output() {
        let row =
            parse("Private heading must not escape\nPeriod runtime\nSpend $25.00\nBudget $100.00")
                .unwrap();
        assert_eq!(row["windows"][0]["remainingPercent"], 75.);
        assert!(!row.to_string().contains("Private"));
        assert_eq!(
            parse(r#"{"spent":150,"budget":100}"#).unwrap()["windows"][0]["remainingPercent"],
            0.
        );
        assert_eq!(
            parse(r#"{"windows":[{"label":"Daily","remainingPercent":0}]}"#).unwrap()["windows"][0]
                ["remainingPercent"],
            0.
        );
        for text in [
            "Spend $3",
            "Spend $3\nBudget $0",
            "Spend $3\nSpend $4\nBudget $10",
            "Spend $-1\nBudget $10",
            "Spend $NaN\nBudget $10",
            "Spend $3,50\nBudget $10",
            r#"{"windows":[{"remainingPercent":120}]}"#,
        ] {
            assert!(parse(text).is_err());
        }
    }
    #[test]
    fn settings_are_provider_scoped_and_empty_commands_preserve_defaults() {
        assert!(validate(&json!({"claude":{"kind":"provider","command":"/quota-fixture:spend"},"copilot":{"kind":"provider","command":""}})).is_ok());
        for value in [
            json!({"unknown":{"kind":"provider","command":"/quota"}}),
            json!({"codex":{"kind":"provider","command":"ordinary prompt"}}),
            json!({"claude":{"kind":"provider","command":"/quota\n/other"}}),
        ] {
            assert!(validate(&value).is_err());
        }
    }
    #[test]
    fn terminal_json_is_scoped_to_one_result_and_probe_ids_are_reserved() {
        for text in ["Banner\n{\"spent\":25,\"budget\":100}\n❯ \n? for shortcuts", "Banner\n{\n  \"windows\": [{\"label\":\"Daily\",\"remainingPercent\":75}]\n}\n❯\nManual Approval"] {
            let payload=terminal_payload(text).unwrap();assert_eq!(parse(&payload).unwrap()["windows"][0]["remainingPercent"],75.);
        }
        assert!(terminal_payload("❯ /quota {\"spent\":25,\"budget\":100}\nReady").is_err());
        assert!(terminal_payload(
            "{\"spent\":25,\"budget\":100}\n{\"spent\":50,\"budget\":100}\nReady"
        )
        .is_err());
        let first = conversation_id();
        let second = conversation_id();
        assert_ne!(first, second);
        assert!(is_probe_conversation(&first));
        assert!(!is_probe_conversation(&id()));
    }
    #[test]
    fn query_ownership_is_bounded_and_shutdown_rejects_late_spawns() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        struct Probe(Arc<AtomicUsize>);
        impl Stop for Probe {
            fn stop(&self) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }
        let queries = Queries::default();
        let stopped = Arc::new(AtomicUsize::new(0));
        let mut guards = Vec::new();
        for _ in 0..4 {
            let guard = queries.reserve().unwrap();
            guard.attach(Arc::new(Probe(stopped.clone()))).unwrap();
            guards.push(guard);
        }
        assert!(queries.reserve().is_err());
        assert_eq!(stopped.load(Ordering::SeqCst), 0);
        queries.shutdown();
        assert!(!queries.running());
        assert_eq!(stopped.load(Ordering::SeqCst), 4);
        drop(guards);
        assert_eq!(stopped.load(Ordering::SeqCst), 4);
        assert!(queries.reserve().is_err());
    }
}
