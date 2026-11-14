use crate::{
    core::{home, id, now, string, Core},
    history,
};
use anyhow::{bail, Context, Result};
use base64::{engine::general_purpose::STANDARD as B64, Engine};
use portable_pty::{native_pty_system, ChildKiller, CommandBuilder, MasterPty, PtySize};
use serde_json::{json, Value};
use std::{
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
};

pub struct LiveTerminal {
    pub run_id: String,
    pub pid: u32,
    pub exited: AtomicBool,
    pub history: Mutex<Vec<u8>>,
    pub seq: Mutex<u64>,
    writer: Mutex<Box<dyn Write + Send>>,
    master: Mutex<Option<Box<dyn MasterPty + Send>>>,
    screen: Mutex<vt100::Parser>,
    killer: Mutex<Box<dyn ChildKiller + Send + Sync>>,
    #[cfg(windows)]
    job: Mutex<Option<Job>>,
}
#[cfg(windows)]
pub(crate) struct Job(windows::Win32::Foundation::HANDLE);
#[cfg(windows)]
unsafe impl Send for Job {}
#[cfg(windows)]
unsafe impl Sync for Job {}
#[cfg(windows)]
impl Drop for Job {
    fn drop(&mut self) {
        unsafe {
            let _ = windows::Win32::Foundation::CloseHandle(self.0);
        }
    }
}
#[cfg(windows)]
impl Job {
    pub(crate) fn new(pid: u32) -> Result<Self> {
        unsafe {
            use windows::Win32::{
                Foundation::CloseHandle,
                System::{JobObjects::*, Threading::*},
            };
            let h = CreateJobObjectW(None, None)?;
            let job = Self(h);
            let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
            limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            SetInformationJobObject(
                h,
                JobObjectExtendedLimitInformation,
                &limits as *const _ as *const _,
                std::mem::size_of_val(&limits) as u32,
            )?;
            let process = OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, false, pid)?;
            let result = AssignProcessToJobObject(h, process);
            let _ = CloseHandle(process);
            result?;
            Ok(job)
        }
    }

    pub(crate) fn owns(&self, pid: u32) -> bool {
        use windows::Win32::{
            Foundation::CloseHandle,
            System::{
                JobObjects::IsProcessInJob,
                Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION},
            },
        };
        unsafe {
            let Ok(process) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
                return false;
            };
            let mut member = windows::core::BOOL(0);
            let result = IsProcessInJob(process, Some(self.0), &mut member);
            let _ = CloseHandle(process);
            result.is_ok() && member.as_bool()
        }
    }
}
impl LiveTerminal {
    pub fn stop(&self) {
        if self.exited.load(Ordering::SeqCst) {
            return;
        }
        #[cfg(windows)]
        {
            self.job.lock().unwrap().take();
        }
        let _ = self.killer.lock().unwrap().kill();
    }
    pub fn write(&self, data: &[u8]) -> Result<()> {
        if self.exited.load(Ordering::SeqCst) {
            bail!("Oturum kapalı")
        };
        let mut w = self.writer.lock().unwrap();
        w.write_all(data)?;
        w.flush()?;
        Ok(())
    }
    pub fn snapshot(&self) -> Value {
        let seq = self.seq.lock().unwrap();
        json!({"runId":self.run_id,"pid":self.pid,"seq":*seq,"data":B64.encode(&*self.history.lock().unwrap()),"exited":self.exited.load(Ordering::SeqCst)})
    }
}
pub fn executable(provider: &str, custom: Option<&str>) -> Option<PathBuf> {
    if let Some(custom) = custom {
        let p = PathBuf::from(custom);
        return p.is_file().then_some(p);
    }
    let mut folders =
        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()).collect::<Vec<_>>();
    folders.push(home().join(".local/bin"));
    folders.push(home().join("AppData/Roaming/npm"));
    for folder in folders {
        for ext in if cfg!(windows) {
            vec![".exe", ".cmd", ".bat", ".ps1", ""]
        } else {
            vec![""]
        } {
            let p = folder.join(format!("{provider}{ext}"));
            if p.is_file() {
                return Some(p);
            }
        }
    }
    None
}
pub(crate) fn uses_windows_script(exe: &Path) -> bool {
    cfg!(windows)
        && exe
            .extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| {
                ["cmd", "bat", "ps1"]
                    .iter()
                    .any(|kind| extension.eq_ignore_ascii_case(kind))
            })
}

pub fn command(exe: &Path, args: &[String]) -> CommandBuilder {
    if uses_windows_script(exe) {
        let mut cmd = CommandBuilder::new("powershell.exe");
        cmd.args(["-NoLogo", "-NoProfile", "-Command"]);
        let literal = |s: &str| format!("'{}'", s.replace('\'', "''"));
        cmd.arg(format!(
            "& {} {}; exit $LASTEXITCODE",
            literal(&exe.to_string_lossy()),
            args.iter()
                .map(|a| literal(a))
                .collect::<Vec<_>>()
                .join(" ")
        ));
        return cmd;
    }
    let mut cmd = CommandBuilder::new(exe);
    cmd.args(args);
    cmd
}
pub fn internal_variable(key: &str) -> bool {
    [
        "CLAUDECODE",
        "CLAUDE_CODE_ENTRYPOINT",
        "CLAUDE_CODE_SSE_PORT",
        "CLAUDE_CODE_SESSION_ID",
        "CODEX_THREAD_ID",
        "CODEX_SESSION_ID",
        "CODEX_INTERNAL_ORIGINATOR_OVERRIDE",
        "CODEX_APP_RUNTIME_CLI_PATH",
        "CODEX_APP_TOOLS_PIPE_PATH",
        "CODEX_PERMISSION_PROFILE",
        "CODEX_SANDBOX_NETWORK_DISABLED",
        "CODEX_WINDOWS_SANDBOX_PACKAGE_FAMILY",
        "CODEX_CI",
        "CODEX_SHELL",
        "CODEX_TOOL_CALL_ID",
        "ELECTRON_RUN_AS_NODE",
        "NODE_OPTIONS",
    ]
    .contains(&key)
}
fn provider_arguments(session: &Value, conversation: Option<&str>) -> Result<Vec<String>> {
    let provider = string(session, "provider")?;
    let session_id = string(session, "id")?;
    if session["login"] == true {
        return Ok(login_arguments(provider));
    }
    let mut args = vec![];
    if let Some(conversation) = conversation {
        args = if provider == "codex" {
            vec!["resume".into(), conversation.to_string()]
        } else {
            vec!["--resume".into(), conversation.to_string()]
        };
    } else if provider == "claude" || provider == "copilot" {
        args.extend(["--session-id".into(), session_id.to_string()]);
    }
    if let Some(model) = session["model"].as_str().filter(|s| !s.is_empty()) {
        args.extend(["--model".into(), model.to_string()]);
    }
    if let Some(effort) = session["effort"].as_str().filter(|s| !s.is_empty()) {
        if provider == "codex" {
            args.extend(["-c".into(), format!("model_reasoning_effort='{effort}'")]);
        }
    }
    Ok(args)
}

fn codex_mcp_bootstrap_failed(core: &Core, session: &Value, session_id: &str) -> bool {
    if session["exitCode"] != 1 || session["tools"] == false {
        return false;
    }
    let path = core
        .directory
        .join("transcripts")
        .join(format!("{session_id}.ansi"));
    let Ok(file) = fs::File::open(path) else {
        return false;
    };
    let mut bytes = Vec::new();
    file.take(8193).read_to_end(&mut bytes).is_ok() && is_codex_mcp_bootstrap_error(&bytes)
}

fn is_codex_mcp_bootstrap_error(bytes: &[u8]) -> bool {
    if bytes.len() > 8192 {
        return false;
    }
    let mut parser = vt100::Parser::new(100, 500, 0);
    parser.process(bytes);
    let screen = parser.screen().contents();
    let output = screen.trim();
    output.starts_with(r#"Error: invalid type: string "[mcp,"#)
        && output.ends_with("expected a sequence\nin `mcp_servers.workflow.args`")
}

pub fn start(core: &Arc<Core>, session_id: &str) -> Result<Value> {
    let _guard = core.operations.lock().unwrap();
    if let Some(t) = core.terms.lock().unwrap().get(session_id) {
        if !t.exited.load(Ordering::SeqCst) {
            return Ok(t.snapshot());
        }
    }
    let session = core.session(session_id)?;
    let provider = string(&session, "provider")?;
    let profile = crate::accounts::Profile::get(core, provider, &session["accountId"])?;
    if profile.directory.is_some()
        && core
            .login_terminal
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|login| {
                login.provider == provider
                    && login.account_id == profile.id
                    && !login.terminal.exited.load(Ordering::SeqCst)
            })
    {
        bail!("Bu hesap için açık olan giriş işlemini tamamla.");
    }
    if session["login"] != true {
        crate::usage::require_account(core, provider, &profile)?;
    }
    let directory = PathBuf::from(string(&session, "directory")?);
    let custom = core.db.lock().unwrap()["providers"][provider]["executable"]
        .as_str()
        .map(str::to_string);
    let fixture = std::env::var("WORKFLOW_TEST_PROVIDER")
        .ok()
        .filter(|_| std::env::var("WORKFLOW_TEST_MODE").ok().as_deref() == Some("1"));
    let exe = if let Some(p) = fixture.as_ref() {
        PathBuf::from(p)
    } else {
        executable(provider, custom.as_deref())
            .context("Sağlayıcının CLI kurulumu bulunamadı. Üstteki Sağlayıcılar düğmesini aç.")?
    };
    let mut conversation = session["conversationId"].as_str().map(str::to_string);
    if conversation.is_none()
        && !session["lastUsedAt"].is_null()
        && session["login"] != true
        && fixture.is_none()
    {
        conversation = history::discover(core, session_id)?;
        if let Some(thread) = &conversation {
            core.update_session(session_id, json!({"conversationId":thread}))?;
        } else if !codex_mcp_bootstrap_failed(core, &session, session_id) {
            bail!("Bu kartın konuşma kaydı henüz doğrulanmadı. ⋯ menüsünden Konuşma seç ile kaydı bağla; yeni konuşma otomatik açılmadı.")
        }
    }
    let mut args = provider_arguments(&session, conversation.as_deref())?;
    let run_id = id();
    let tools = session["tools"] != false && session["login"] != true;
    let tool_args = if tools {
        crate::mcp::configuration(core, &session, &run_id, &exe)?
    } else {
        vec![]
    };
    if fixture.is_some() {
        args = vec!["--fixture".into(), provider.to_string()];
    } else if tools {
        args.extend(tool_args);
    }
    crate::addons::prepare(core, &session)?;
    let mut cmd = command(&exe, &args);
    profile.apply_pty(&mut cmd);
    cmd.cwd(&directory);
    cmd.env("TERM", "xterm-256color");
    cmd.env("COLORTERM", "truecolor");
    cmd.env("TERM_PROGRAM", "workflow");
    if session["addons"]
        .as_array()
        .is_some_and(|a| a.iter().any(|v| v == "openspec"))
    {
        let bin = core.directory.join("addons/openspec/node_modules/.bin");
        let mut paths = vec![bin];
        paths.extend(std::env::split_paths(
            &std::env::var_os("PATH").unwrap_or_default(),
        ));
        cmd.env("PATH", std::env::join_paths(paths)?);
    }
    let folder = core.directory.join("transcripts");
    fs::create_dir_all(&folder)?;
    let log = fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(folder.join(format!("{session_id}.ansi")))?;
    let (t, mut child, reader) = spawn_terminal(cmd, run_id.clone())?;
    core.terms
        .lock()
        .unwrap()
        .insert(session_id.into(), t.clone());
    let mut patch = json!({"runId":run_id,"lastUsedAt":now(),"state":"running"});
    if provider != "codex" && session["login"] != true && session["conversationId"].is_null() {
        patch["conversationId"] = json!(session_id)
    };
    if let Err(error) = core.update_session(session_id, patch) {
        core.terms.lock().unwrap().remove(session_id);
        t.stop();
        drop(reader);
        drop(t);
        let _ = child.wait();
        return Err(error);
    }
    read_terminal(
        core.clone(),
        t.clone(),
        reader,
        Some((session_id.into(), log)),
    );
    let c = core.clone();
    let tt = t.clone();
    let sid = session_id.to_string();
    let rid = run_id.clone();
    std::thread::spawn(move || {
        let status = child.wait().map(|s| s.exit_code()).unwrap_or(1);
        let _guard = c.operations.lock().unwrap();
        tt.exited.store(true, Ordering::SeqCst);
        #[cfg(windows)]
        tt.job.lock().unwrap().take();
        let _ = c.update_session(&sid, json!({"state":"stopped","exitCode":status}));
        c.emit("terminal", json!({"id":sid,"runId":rid,"exitCode":status}));
    });
    if provider == "codex"
        && session["login"] != true
        && session["workspaceKind"] != "direct"
        && fixture.is_none()
    {
        let c = core.clone();
        let sid = session_id.to_string();
        std::thread::spawn(move || {
            for _ in 0..30 {
                std::thread::sleep(std::time::Duration::from_secs(2));
                if c.session(&sid)
                    .ok()
                    .is_some_and(|s| s["runId"] == run_id && s["conversationId"].is_null())
                {
                    if let Ok(Some(thread)) = history::discover(&c, &sid) {
                        let _guard = c.operations.lock().unwrap();
                        if c.session(&sid)
                            .ok()
                            .is_some_and(|s| s["runId"] == run_id && s["conversationId"].is_null())
                        {
                            let _ = c.update_session(&sid, json!({"conversationId":thread}));
                        }
                        break;
                    }
                } else {
                    break;
                }
            }
        });
    }
    Ok(t.snapshot())
}

fn login_arguments(provider: &str) -> Vec<String> {
    match provider {
        "claude" => vec!["auth".into(), "login".into()],
        _ => vec!["login".into()],
    }
}

type SpawnedTerminal = (
    Arc<LiveTerminal>,
    Box<dyn portable_pty::Child + Send + Sync>,
    Box<dyn Read + Send>,
);

fn spawn_terminal(mut cmd: CommandBuilder, run_id: String) -> Result<SpawnedTerminal> {
    for key in std::env::vars_os().map(|(k, _)| k) {
        let k = key.to_string_lossy();
        if internal_variable(&k) {
            cmd.env_remove(&*k);
        }
    }
    let pair = native_pty_system().openpty(PtySize {
        rows: 36,
        cols: 120,
        pixel_width: 0,
        pixel_height: 0,
    })?;
    let mut child = pair.slave.spawn_command(cmd)?;
    let pid = child
        .process_id()
        .context("Terminal işlem kimliği alınamadı")?;
    drop(pair.slave);
    #[cfg(windows)]
    let job = match Job::new(pid) {
        Ok(j) => j,
        Err(e) => {
            let _ = child.kill();
            bail!("Terminal süreç grubu oluşturulamadı: {e}")
        }
    };
    let writer = pair.master.take_writer()?;
    let reader = pair.master.try_clone_reader()?;
    let t = Arc::new(LiveTerminal {
        run_id,
        pid,
        exited: AtomicBool::new(false),
        history: Mutex::new(vec![]),
        seq: Mutex::new(0),
        writer: Mutex::new(writer),
        master: Mutex::new(Some(pair.master)),
        screen: Mutex::new(vt100::Parser::new(36, 120, 10000)),
        killer: Mutex::new(child.clone_killer()),
        #[cfg(windows)]
        job: Mutex::new(Some(job)),
    });
    Ok((t, child, reader))
}

fn read_terminal(
    core: Arc<Core>,
    tt: Arc<LiveTerminal>,
    mut reader: Box<dyn Read + Send>,
    transcript: Option<(String, fs::File)>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let (session_id, mut log) = match transcript {
            Some((id, file)) => (Some(id), Some(file)),
            None => (None, None),
        };
        let mut buffer = [0u8; 16384];
        let mut query = Vec::<u8>::new();
        loop {
            match reader.read(&mut buffer) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if let Some(f) = log.as_mut() {
                        let _ = f.write_all(&buffer[..n]);
                    }
                    {
                        let mut parser = tt.screen.lock().unwrap();
                        for byte in &buffer[..n] {
                            parser.process(&[*byte]);
                            query.push(*byte);
                            if query.len() > 16 {
                                query.remove(0);
                            }
                            if query.ends_with(b"\x1b[6n") {
                                let (row, col) = parser.screen().cursor_position();
                                let _ =
                                    tt.write(format!("\x1b[{};{}R", row + 1, col + 1).as_bytes());
                                query.clear();
                            } else if query.ends_with(b"\x1b[c") || query.ends_with(b"\x1b[0c") {
                                let _ = tt.write(b"\x1b[?1;2c");
                                query.clear();
                            } else if query.ends_with(b"\x1b[>c") {
                                let _ = tt.write(b"\x1b[>0;1;0c");
                                query.clear();
                            }
                        }
                    }
                    let mut seq = tt.seq.lock().unwrap();
                    *seq += n as u64;
                    {
                        let mut h = tt.history.lock().unwrap();
                        h.extend_from_slice(&buffer[..n]);
                        if h.len() > 16 * 1024 * 1024 {
                            let drain = h.len() - 12 * 1024 * 1024;
                            h.drain(..drain);
                        }
                    }
                    if let Some(sid) = &session_id {
                        core.emit(
                            "terminal",
                            json!({"id":sid,"runId":tt.run_id,"seq":*seq,"data":B64.encode(&buffer[..n])}),
                        );
                    }
                }
            }
        }
    })
}

pub struct LoginTerminal {
    provider: String,
    account_id: String,
    pub terminal: Arc<LiveTerminal>,
}

pub fn login(core: &Arc<Core>, action: &str, args: &Value) -> Result<Value> {
    let _guard = (action == "start").then(|| core.operations.lock().unwrap());
    let mut current = core.login_terminal.lock().unwrap();
    if action == "start" {
        let provider = string(args, "provider")?;
        if !["codex", "claude", "copilot"].contains(&provider) {
            bail!("Sağlayıcı geçersiz")
        }
        if current
            .as_ref()
            .is_some_and(|login| !login.terminal.exited.load(Ordering::SeqCst))
        {
            bail!("Bir hesap girişi zaten açık")
        }
        let mut arguments = login_arguments(provider);
        let profile = crate::accounts::Profile::get(core, provider, &args["accountId"])?;
        if let Some(directory) = &profile.directory {
            let db = core.db.lock().unwrap();
            let terms = core.terms.lock().unwrap();
            let running = db["sessions"].as_array().into_iter().flatten().any(|s| {
                s["provider"] == provider
                    && s["accountId"] == profile.id
                    && s["id"]
                        .as_str()
                        .and_then(|id| terms.get(id))
                        .is_some_and(|t| !t.exited.load(Ordering::SeqCst))
            });
            if running {
                bail!("Yeniden giriş yapmadan önce bu hesaba bağlı oturumları durdur.");
            }
            fs::create_dir_all(directory)?;
        }
        let fixture = std::env::var_os("WORKFLOW_TEST_PROVIDER")
            .filter(|_| std::env::var("WORKFLOW_TEST_MODE").ok().as_deref() == Some("1"));
        let exe = if let Some(fixture) = fixture {
            arguments = vec!["--login-fixture".into(), provider.into()];
            PathBuf::from(fixture)
        } else {
            crate::usage::executable(core, provider)
                .context("Sağlayıcının CLI kurulumu bulunamadı")?
        };
        let mut cmd = command(&exe, &arguments);
        profile.apply_pty(&mut cmd);
        cmd.cwd(&core.directory);
        cmd.env("TERM", "xterm-256color");
        let (terminal, mut child, reader) = spawn_terminal(cmd, id())?;
        let output = read_terminal(core.clone(), terminal.clone(), reader, None);
        let finished = terminal.clone();
        std::thread::spawn(move || {
            let _ = child.wait();
            #[cfg(windows)]
            finished.job.lock().unwrap().take();
            // Closing the master lets the reader drain the final output and exit.
            let master = finished.master.lock().unwrap().take();
            drop(master);
            let _ = output.join();
            finished.exited.store(true, Ordering::SeqCst);
        });
        let run = json!({"runId":terminal.run_id});
        *current = Some(LoginTerminal {
            provider: provider.into(),
            account_id: profile.id,
            terminal,
        });
        return Ok(run);
    }

    let run_id = string(args, "runId")?;
    let Some(terminal) = current
        .as_ref()
        .map(|login| &login.terminal)
        .filter(|t| t.run_id == run_id)
    else {
        if action == "close" {
            return Ok(json!(true));
        }
        bail!("Hesap girişi kapalı")
    };
    match action {
        "poll" => {
            let seq = terminal.seq.lock().unwrap();
            let history = terminal.history.lock().unwrap();
            let since = args["since"].as_u64().unwrap_or(0);
            let count = seq.saturating_sub(since).min(history.len() as u64) as usize;
            Ok(json!({
                "seq":*seq,
                "data":B64.encode(&history[history.len()-count..]),
                "exited":terminal.exited.load(Ordering::SeqCst)
            }))
        }
        "write" => {
            terminal.write(string(args, "data")?.as_bytes())?;
            Ok(json!(true))
        }
        "resize" => resize_terminal(terminal, args),
        "close" => {
            terminal.stop();
            *current = None;
            Ok(json!(true))
        }
        _ => bail!("Geçersiz hesap girişi işlemi"),
    }
}

pub fn get(core: &Core, args: &Value) -> Result<Arc<LiveTerminal>> {
    let s = string(args, "id")?;
    let t = core
        .terms
        .lock()
        .unwrap()
        .get(s)
        .cloned()
        .context("Terminal çalışmıyor")?;
    if args["runId"].as_str().is_some_and(|v| v != t.run_id) {
        bail!("Oturum çalıştırması değişti")
    };
    Ok(t)
}
fn resize_terminal(t: &LiveTerminal, args: &Value) -> Result<Value> {
    let cols = args["cols"].as_u64().unwrap_or(120).clamp(20, 500) as u16;
    let rows = args["rows"].as_u64().unwrap_or(36).clamp(5, 300) as u16;
    t.master
        .lock()
        .unwrap()
        .as_ref()
        .context("Terminal kapalı")?
        .resize(PtySize {
            cols,
            rows,
            pixel_width: 0,
            pixel_height: 0,
        })?;
    t.screen.lock().unwrap().screen_mut().set_size(rows, cols);
    Ok(json!(true))
}

pub fn dispatch(core: &Arc<Core>, action: &str, args: &Value) -> Result<Value> {
    match action {
        "start" => start(core, string(args, "id")?),
        "attach" => {
            let t = core.terms.lock().unwrap().get(string(args, "id")?).cloned();
            if let Some(t) = t {
                Ok(t.snapshot())
            } else {
                let log = core
                    .directory
                    .join("transcripts")
                    .join(format!("{}.ansi", string(args, "id")?));
                Ok(
                    json!({"data":B64.encode(fs::read(log).unwrap_or_default()),"exited":true,"seq":0}),
                )
            }
        }
        "write" => {
            get(core, args)?.write(string(args, "data")?.as_bytes())?;
            Ok(json!(true))
        }
        "resize" => resize_terminal(get(core, args)?.as_ref(), args),
        "stop" => {
            get(core, args)?.stop();
            Ok(json!(true))
        }
        _ => bail!("Geçersiz terminal işlemi"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn login_never_receives_conversation_options() {
        for provider in ["codex", "claude", "copilot"] {
            let session = json!({"id":"session-id","provider":provider,"login":true,"model":"chosen-model","effort":"high"});
            let expected = if provider == "claude" {
                vec!["auth", "login"]
            } else {
                vec!["login"]
            };
            assert_eq!(
                provider_arguments(&session, Some("old-conversation")).unwrap(),
                expected
            );
        }
    }
    #[test]
    fn conversation_keeps_model_and_exact_resume_id() {
        let session = json!({"id":"session-id","provider":"codex","login":false,"model":"chosen-model","effort":"high"});
        assert_eq!(
            provider_arguments(&session, Some("exact-thread")).unwrap(),
            vec![
                "resume",
                "exact-thread",
                "--model",
                "chosen-model",
                "-c",
                "model_reasoning_effort='high'"
            ]
        );
        let session =
            json!({"id":"session-id","provider":"claude","login":false,"model":"chosen-model"});
        assert_eq!(
            provider_arguments(&session, None).unwrap(),
            vec!["--session-id", "session-id", "--model", "chosen-model"]
        );
    }
}
