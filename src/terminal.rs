use crate::{
    core::{home, id, now, string, Core},
    history,
};
use anyhow::{bail, Context, Result};
use base64::{engine::general_purpose::STANDARD as B64, Engine};
use portable_pty::{native_pty_system, ChildKiller, CommandBuilder, MasterPty, PtySize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};

pub struct LiveTerminal {
    pub run_id: String,
    pub pid: u32,
    pub exited: AtomicBool,
    pub history: Mutex<Vec<u8>>,
    pub seq: Mutex<u64>,
    input: Mutex<TerminalInput>,
    chat_thread: Mutex<Option<String>>,
    provider: String,
    master: Mutex<Option<Box<dyn MasterPty + Send>>>,
    screen: Mutex<vt100::Parser>,
    killer: Mutex<Box<dyn ChildKiller + Send + Sync>>,
    #[cfg(windows)]
    job: Mutex<Option<Job>>,
}

struct TerminalInput {
    writer: Box<dyn Write + Send>,
    revision: u64,
    chat_pasting: bool,
    chat_pending: bool,
    terminal_slash: bool,
    thread_uncertain: bool,
    answered_prompt: Option<AnsweredPrompt>,
}

struct AnsweredPrompt {
    key: String,
    question_context: Option<String>,
    custom_choice: Option<usize>,
}

impl AnsweredPrompt {
    fn matches(&self, prompt: &TerminalPrompt) -> bool {
        prompt
            .menu
            .as_ref()
            .is_some_and(|menu| menu.key == self.key)
            || self.question_context.as_ref().is_some_and(|context| {
                prompt.kind == "question"
                    && custom_prompt_context(&prompt.text, self.custom_choice) == *context
            })
    }
}

impl TerminalInput {
    fn write(&mut self, data: &[u8]) -> Result<()> {
        self.revision += 1;
        self.write_bytes(data)
    }

    fn write_bytes(&mut self, data: &[u8]) -> Result<()> {
        self.writer.write_all(data)?;
        self.writer.flush()?;
        Ok(())
    }

    fn write_from_terminal(
        &mut self,
        mut data: &[u8],
        composer_empty: bool,
        visible_draft: Option<&str>,
    ) -> Result<()> {
        let original = data;
        let bracketed = data.starts_with(b"\x1b[200~") && data.ends_with(b"\x1b[201~");
        if bracketed {
            data = &data[6..data.len() - 6];
        }
        if composer_empty
            && (data.iter().copied().find(|c| !c.is_ascii_whitespace()) == Some(b'/')
                || data == b"\x1b[A")
        {
            self.terminal_slash = true;
        }
        if data == b"\x1b" || data == b"\x03" {
            self.terminal_slash = false;
        }
        if !bracketed && data.iter().any(|byte| matches!(byte, b'\r' | b'\n')) {
            let typed_command = std::str::from_utf8(data)
                .ok()
                .map(str::trim_start)
                .filter(|text| text.starts_with('/'));
            let maybe_switch = match typed_command.or(visible_draft) {
                Some(text) if text.starts_with('/') => !matches!(
                    text.split_whitespace().next(),
                    Some("/status" | "/help" | "/diff" | "/model" | "/permissions" | "/effort")
                ),
                Some(text) if !text.is_empty() && text != CODEX_EMPTY_PROMPT => false,
                _ => self.terminal_slash,
            };
            self.thread_uncertain |= maybe_switch;
            self.terminal_slash = false;
        }
        self.write(original)
    }
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
        self.input.lock().unwrap().write(data)
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
    #[cfg(windows)]
    folders.push(
        std::env::var_os("APPDATA")
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
            .unwrap_or_else(|| home().join("AppData/Roaming"))
            .join("npm"),
    );
    #[cfg(not(windows))]
    folders.push(home().join("AppData/Roaming/npm"));
    #[cfg(windows)]
    if let Some(local) = std::env::var_os("LOCALAPPDATA").map(PathBuf::from) {
        folders.extend([
            local.join("Microsoft/WinGet/Links"),
            local.join("Microsoft/WindowsApps"),
        ]);
    }
    if let Some(path) = folders
        .iter()
        .find_map(|folder| executable_in(folder, provider))
    {
        return Some(path);
    }
    #[cfg(windows)]
    if provider == "copilot" {
        return crate::provider_setup::npm_prefix()
            .and_then(|prefix| executable_in(&prefix, provider));
    }
    None
}

fn executable_in(folder: &Path, program: &str) -> Option<PathBuf> {
    let extensions: &[&str] = if cfg!(windows) {
        &[".exe", ".cmd", ".bat", ".ps1", ""]
    } else {
        &[""]
    };
    extensions
        .iter()
        .map(|ext| folder.join(format!("{program}{ext}")))
        .find(|path| path.is_file())
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
pub(crate) fn codex_config_string(value: &str, windows_script: bool) -> String {
    if !windows_script {
        return json!(value).to_string();
    }
    // Preserve TOML quotes through PowerShell and encode batch-shim metacharacters.
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
    match (provider, session["approvalMode"].as_str()) {
        ("codex", Some(mode @ ("manual" | "auto"))) => {
            let reviewer = if mode == "auto" {
                "auto_review"
            } else {
                "user"
            };
            args.extend([
                "-a".into(),
                "on-request".into(),
                "-c".into(),
                format!("approvals_reviewer='{reviewer}'"),
            ]);
        }
        ("claude", Some(mode @ ("manual" | "auto" | "edits" | "plan"))) => {
            let value = match mode {
                "manual" => "default",
                "edits" => "acceptEdits",
                _ => mode,
            };
            args.extend(["--permission-mode".into(), value.into()]);
        }
        _ => {}
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
    let instructions = crate::instructions::prepare(core, &session, &directory, &exe, &profile)?;
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
    args.extend(instructions.arguments);
    let openspec_path = if session["workspaceKind"] != "direct"
        && session["addons"]
            .as_array()
            .is_some_and(|a| a.iter().any(|v| v == "openspec"))
    {
        let bin = crate::addons::openspec_directory(core)?;
        let mut paths = vec![bin];
        paths.extend(std::env::split_paths(
            &std::env::var_os("PATH").unwrap_or_default(),
        ));
        Some(std::env::join_paths(paths)?)
    } else {
        None
    };
    crate::addons::prepare(core, &session)?;
    let mut cmd = command(&exe, &args);
    profile.apply_pty(&mut cmd);
    cmd.cwd(&directory);
    cmd.env("TERM", "xterm-256color");
    cmd.env("COLORTERM", "truecolor");
    cmd.env("TERM_PROGRAM", "workflow");
    if let Some(directories) = instructions.copilot_directories {
        cmd.env(crate::instructions::COPILOT_DIRECTORIES, directories);
    }
    if let Some(path) = openspec_path {
        cmd.env("PATH", path);
    }
    let folder = core.directory.join("transcripts");
    fs::create_dir_all(&folder)?;
    let log = fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(folder.join(format!("{session_id}.ansi")))?;
    let (t, mut child, reader) = spawn_terminal(cmd, run_id.clone(), provider)?;
    *t.chat_thread.lock().unwrap() = conversation.clone().or_else(|| {
        (provider != "codex" && session["login"] != true).then(|| session_id.to_owned())
    });
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
        let terminal = t.clone();
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
                            let mut bound = terminal.chat_thread.lock().unwrap();
                            if c.update_session(&sid, json!({"conversationId":thread}))
                                .is_ok()
                            {
                                *bound = Some(thread);
                            }
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

fn spawn_terminal(
    mut cmd: CommandBuilder,
    run_id: String,
    provider: &str,
) -> Result<SpawnedTerminal> {
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
        input: Mutex::new(TerminalInput {
            writer,
            revision: 0,
            chat_pasting: false,
            chat_pending: false,
            terminal_slash: false,
            thread_uncertain: false,
            answered_prompt: None,
        }),
        chat_thread: Mutex::new(None),
        provider: provider.to_owned(),
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
                                let _ = tt.input.lock().unwrap().write_bytes(
                                    format!("\x1b[{};{}R", row + 1, col + 1).as_bytes(),
                                );
                                query.clear();
                            } else if query.ends_with(b"\x1b[c") || query.ends_with(b"\x1b[0c") {
                                let _ = tt.input.lock().unwrap().write_bytes(b"\x1b[?1;2c");
                                query.clear();
                            } else if query.ends_with(b"\x1b[>c") {
                                let _ = tt.input.lock().unwrap().write_bytes(b"\x1b[>0;1;0c");
                                query.clear();
                            }
                        }
                        // A fast turn can redraw straight from the verified draft to the empty composer.
                        let mut input = tt.input.lock().unwrap();
                        input.chat_pending = false;
                        if let Some(answered) = &input.answered_prompt {
                            let different_menu = !input.chat_pasting
                                && terminal_prompt(parser.screen(), &tt.provider).is_some_and(
                                    |prompt| prompt.menu.is_some() && !answered.matches(&prompt),
                                );
                            if different_menu
                                || provider_prompt_ready(parser.screen(), &tt.provider)
                            {
                                input.answered_prompt = None;
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
        let (terminal, mut child, reader) = spawn_terminal(cmd, id(), provider)?;
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

const CODEX_EMPTY_PROMPT: &str = "Ask Codex to do anything";

fn codex_composer_text(screen: &vt100::Screen) -> Option<String> {
    screen
        .rows(0, screen.size().1)
        .nth(screen.cursor_position().0 as usize)
        .and_then(|line| {
            line.trim_start()
                .strip_prefix(['›', '❯', '>'])
                .map(|input| input.trim().to_owned())
        })
}

fn empty_codex_prompt(screen: &vt100::Screen) -> bool {
    // Confirm the empty composer at the cursor, not a prompt quoted in scrollback.
    screen.bracketed_paste()
        && screen.cursor_position().1 <= 6
        && codex_composer_text(screen).as_deref() == Some(CODEX_EMPTY_PROMPT)
}

fn codex_prompt_ready(screen: &vt100::Screen) -> bool {
    empty_codex_prompt(screen) && codex_composer_unblocked(screen)
}

fn provider_prompt_ready(screen: &vt100::Screen, provider: &str) -> bool {
    native_prompt_ready(screen, provider) && terminal_prompt(screen, provider).is_none()
}

fn native_prompt_ready(screen: &vt100::Screen, provider: &str) -> bool {
    if provider == "codex" {
        return codex_prompt_ready(screen);
    }
    let tags: &[&str] = match provider {
        "claude" => &["for shortcuts", "/effort", "mode on", "accept edits on"],
        "copilot" => &["manual approval", "assisted approval"],
        _ => return false,
    };
    provider_composer(screen).is_some_and(|(draft, footer)| {
        draft.trim().is_empty() && tags.iter().any(|tag| footer.contains(tag))
    })
}

fn provider_composer(screen: &vt100::Screen) -> Option<(String, String)> {
    if !screen.bracketed_paste() {
        return None;
    }
    // These renderers draw a cursor independently from the terminal's hardware cursor.
    // Identify their composer by its framing lines and provider-specific footer.
    let lines = screen.rows(0, screen.size().1).collect::<Vec<_>>();
    let divider = |line: &str| line.trim_start().starts_with("─────");
    (1..lines.len()).rev().find_map(|start| {
        lines[start].trim_start().strip_prefix('❯')?;
        if !divider(&lines[start - 1]) {
            return None;
        }
        let end = (start + 1..lines.len()).find(|&index| divider(&lines[index]))?;
        let footer = lines[end + 1..].join(" ").to_ascii_lowercase();
        if [
            "esc to cancel",
            "sign in to continue",
            "cancel with esc",
            "enter to confirm",
            "esc to interrupt",
            "ctrl+c to interrupt",
            "ctrl-c to interrupt",
        ]
        .iter()
        .any(|tag| footer.contains(tag))
        {
            return None;
        }
        // Cells beyond the drawn frame can still hold an earlier, wider render.
        let frame_width = lines[start - 1].chars().take_while(|&ch| ch == '─').count();
        let width = if frame_width >= 20 {
            frame_width as u16
        } else {
            screen.size().1
        };
        let text = screen
            .rows(0, width)
            .skip(start)
            .take(end - start)
            .enumerate()
            .map(|(index, line)| {
                let line = line.trim();
                if index == 0 {
                    line.strip_prefix('❯').unwrap_or(line).trim().to_owned()
                } else {
                    line.to_owned()
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        Some((text, footer))
    })
}

fn codex_composer_unblocked(screen: &vt100::Screen) -> bool {
    let cursor_row = screen.cursor_position().0 as usize;
    let footer = screen
        .rows(0, screen.size().1)
        .skip(cursor_row + 1)
        .collect::<Vec<_>>()
        .join("\n")
        .to_ascii_lowercase();
    // Codex can render its running indicator immediately above the composer.
    // Match the status line itself so plain history text cannot block input.
    let running_above = screen
        .rows(0, screen.size().1)
        .take(cursor_row)
        .any(|line| {
            let line = line.trim_start();
            let Some(status) = line.strip_prefix("• ") else {
                return false;
            };
            (status.starts_with("Working (") || status.starts_with("Working…"))
                && status.contains("esc to interrupt")
        });
    if running_above {
        return false;
    }
    ![
        "esc to interrupt",
        "ctrl+c to interrupt",
        "ctrl-c to interrupt",
        "working (",
        "working…",
        "do you want to proceed",
        "do you trust",
        "do you want to make this edit",
        "this command requires approval",
        "would you like to run the following",
        "approve for this session",
        "allow for this session",
        "update available",
    ]
    .iter()
    .any(|text| footer.contains(text))
}

fn chat_thread_matches(terminal: &LiveTerminal, session: &Value) -> bool {
    terminal.chat_thread.lock().unwrap().as_deref() == session["conversationId"].as_str()
}

#[derive(serde::Serialize)]
pub(crate) struct TerminalPrompt {
    kind: &'static str,
    text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    menu: Option<TerminalMenu>,
    #[serde(skip)]
    first_row: usize,
    #[serde(skip)]
    end_row: usize,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct TerminalMenu {
    key: String,
    selected: usize,
    choices: Vec<String>,
    descriptions: Vec<String>,
    question: String,
    custom_choice: Option<usize>,
    #[serde(skip)]
    context: String,
    input_revision: u64,
}

fn question_context(text: &str) -> String {
    text.lines()
        .take_while(|line| {
            !(line.trim_start().starts_with('›') && menu_line(line).is_none())
                && !question_footer(line)
        })
        .filter(|line| {
            !line.trim().is_empty() && !line.trim().chars().all(|ch| "─│╭╮╰╯".contains(ch))
        })
        .map(|line| {
            if let Some((number, label, _)) = menu_line(line) {
                format!("{number}. {label}")
            } else {
                line.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn question_counter(line: &str) -> bool {
    let lower = line.trim().to_ascii_lowercase();
    let Some(rest) = lower.strip_prefix("question ") else {
        return false;
    };
    let Some((index, rest)) = rest.split_once('/') else {
        return false;
    };
    let total = rest.split_whitespace().next().unwrap_or("");
    !index.is_empty()
        && index.bytes().all(|b| b.is_ascii_digit())
        && !total.is_empty()
        && total.bytes().all(|b| b.is_ascii_digit())
        && (rest == total || rest[total.len()..].trim().starts_with('('))
}

fn question_footer(line: &str) -> bool {
    let line = line
        .trim_start_matches(|ch: char| ch.is_whitespace() || ch == '│')
        .to_ascii_lowercase();
    [
        "enter to ",
        "press enter to ",
        "tab to ",
        "tab/arrow keys to ",
        "tab or esc to ",
        "esc to ",
        "escape to ",
        "↑/↓ to navigate",
        "↑↓ to navigate",
        "use ↑",
    ]
    .iter()
    .any(|prefix| line.starts_with(prefix))
}

fn custom_prompt_context(text: &str, custom: Option<usize>) -> String {
    if let Some(custom) = custom {
        return inline_question_context(text, custom).0;
    }
    let rows = text.lines().collect::<Vec<_>>();
    let frames = rows
        .iter()
        .enumerate()
        .filter(|(_, line)| frame_line(line))
        .map(|(row, _)| row)
        .collect::<Vec<_>>();
    if frames.len() >= 2 {
        question_context(&rows[..frames[frames.len() - 2]].join("\n"))
    } else {
        question_context(text)
    }
}

fn frame_line(line: &str) -> bool {
    let line = line.trim();
    line.chars().count() >= 5 && line.chars().all(|ch| "─╭╮╰╯".contains(ch))
}

fn inline_question_context(text: &str, custom_choice: usize) -> (String, Option<(String, bool)>) {
    let mut context = Vec::new();
    let mut draft: Option<(String, bool)> = None;
    let mut in_input = false;
    for line in text.lines().take_while(|line| !question_footer(line)) {
        if let Some((number, label, focused)) = menu_line(line) {
            in_input = number == custom_choice;
            if in_input {
                if draft.is_some() {
                    return (String::new(), None);
                }
                draft = Some((label.to_owned(), focused));
                context.push(format!("{number}. <custom>"));
            } else {
                context.push(format!("{number}. {label}"));
            }
        } else if !line.trim().is_empty() && !line.trim().chars().all(|ch| "─│╭╮╰╯".contains(ch))
        {
            if in_input {
                if let Some((draft, _)) = &mut draft {
                    draft.push_str(line.trim());
                }
            } else {
                context.push(line.to_owned());
            }
        }
    }
    (context.join("\n"), draft)
}

fn custom_question_placeholder(provider: &str, label: &str) -> bool {
    match provider {
        "codex" => label == "None of the above",
        "claude" => matches!(label, "Type something." | "Type something"),
        "copilot" => matches!(
            label,
            "Other (type your answer)" | "Other (type to respond)"
        ),
        _ => false,
    }
}

fn menu_line(line: &str) -> Option<(usize, &str, bool)> {
    let line = line.trim_matches(|ch: char| ch.is_whitespace() || ch == '│');
    let mut selected = false;
    let line = if let Some(rest) = line.strip_prefix(['›', '❯', '>', '▸']) {
        selected = true;
        rest.trim_start()
    } else {
        line
    };
    let number = usize::from(*line.as_bytes().first()?).checked_sub(usize::from(b'0'))?;
    if !(1..=9).contains(&number) {
        return None;
    }
    let text = [". ", ") ", " - ", " – ", " — "]
        .iter()
        .find_map(|separator| line[1..].strip_prefix(separator))?
        .trim();
    (!text.is_empty()).then_some((number, text, selected))
}

fn terminal_menu(lines: &[&str], footer: &str, kind: &str, provider: &str) -> Option<TerminalMenu> {
    // Require an identifiable keyboard menu. Leave notes, multi-select, custom
    // submit bindings, and incomplete or ambiguous lists to the native view.
    let enter_hint = |action: &str| {
        let hint = format!("enter to {action}");
        footer.split(['|', '·', '│']).any(|part| {
            let part = part.trim();
            part.strip_prefix("press ")
                .unwrap_or(part)
                .starts_with(&hint)
        })
    };
    let supported = match (kind, provider) {
        ("question", "codex") => {
            footer.contains("tab to add notes")
                && (enter_hint("submit answer") || enter_hint("submit all"))
        }
        ("question", "claude") => {
            footer.contains("tab/arrow keys to navigate") || enter_hint("select")
        }
        ("question", "copilot") => enter_hint("select"),
        ("approval", _) => enter_hint("confirm"),
        _ => false,
    };
    if !supported
        || [
            "space to toggle",
            "select all that apply",
            "select multiple",
            "clear notes",
            "ctrl+enter",
            "⌃enter",
            "shift+enter",
            "alt+enter",
        ]
        .iter()
        .any(|hint| footer.contains(hint))
    {
        return None;
    }
    let mut choices = Vec::new();
    let mut selected = None;
    let mut identity = String::new();
    let mut previous_number = 0;
    let mut options_ended = false;
    for line in lines {
        if let Some((number, text, focused)) = menu_line(line) {
            if number != previous_number + 1 || (focused && selected.replace(number).is_some()) {
                return None;
            }
            previous_number = number;
            if provider == "claude"
                && kind == "question"
                && matches!(
                    text,
                    "Chat about this" | "Skip interview and plan immediately"
                )
            {
                options_ended = true;
            } else if options_ended {
                return None;
            } else {
                choices.push(text.to_owned());
            }
            identity.push_str(&format!("{number}. {text}"));
        } else {
            identity.push_str(line);
        }
        identity.push('\n');
    }
    (choices.len() >= 2).then_some(())?;
    let selected = selected.filter(|number| *number <= choices.len())?;
    let first = lines.iter().position(|line| menu_line(line).is_some())?;
    let mut descriptions = vec![String::new(); choices.len()];
    let mut current = None;
    for line in &lines[first..] {
        if let Some((number, _, _)) = menu_line(line) {
            if number > choices.len() {
                break;
            }
            current = Some(number - 1);
        } else if !line.trim().is_empty() {
            let lower = line.to_ascii_lowercase();
            if question_footer(&lower) {
                break;
            }
            if let Some(index) = current {
                if !descriptions[index].is_empty() {
                    descriptions[index].push(' ');
                }
                descriptions[index].push_str(line.trim());
            }
        }
    }
    let question = lines[..first]
        .iter()
        .filter(|line| {
            let line = line.trim().to_ascii_lowercase();
            !line.is_empty() && !question_counter(&line)
        })
        .copied()
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_owned();
    if kind == "question" && question.is_empty() {
        return None;
    }
    let custom_choice = if kind == "question" {
        choices
            .iter()
            .position(|label| custom_question_placeholder(provider, label))
            .map(|index| index + 1)
    } else {
        None
    };
    let context = if let Some(custom) = custom_choice.filter(|_| provider != "codex") {
        inline_question_context(&lines.join("\n"), custom).0
    } else {
        question_context(&lines.join("\n"))
    };
    Some(TerminalMenu {
        key: format!("{:x}", Sha256::digest(identity.as_bytes())),
        selected,
        choices,
        descriptions,
        question,
        custom_choice,
        context,
        input_revision: 0,
    })
}

fn terminal_prompt(screen: &vt100::Screen, provider: &str) -> Option<TerminalPrompt> {
    let rows = screen.rows(0, screen.size().1).collect::<Vec<_>>();
    let end = rows.iter().rposition(|line| !line.trim().is_empty())? + 1;
    let mut display_start = 0;
    // An already answered dialog can remain visible above the normal Codex input.
    if provider == "codex" && empty_codex_prompt(screen) {
        display_start = usize::from(screen.cursor_position().0) + 1;
    } else if provider != "codex" && native_prompt_ready(screen, provider) {
        // Ignore old dialogs above the input while still checking for overlays
        // drawn below an otherwise recognizable native composer.
        if let Some(frame) = rows[..end].windows(3).rposition(|rows| {
            rows[0].trim_start().starts_with("─────")
                && rows[1].trim() == "❯"
                && rows[2].trim_start().starts_with("─────")
        }) {
            display_start = frame + 3;
        }
    }
    let visible = rows.get(display_start.max(end.saturating_sub(30))..end)?;
    let cues = visible
        .iter()
        .map(|line| {
            line.trim_start_matches(|ch: char| ch.is_whitespace() || "│›❯>●•▸".contains(ch))
                .trim_end()
                .to_ascii_lowercase()
        })
        .collect::<Vec<_>>();
    let footer = cues[cues.len().saturating_sub(5)..].join(" ");
    let cancel = ["esc to cancel", "cancel with esc", "esc to reject"]
        .iter()
        .any(|hint| footer.contains(hint));
    let confirm = footer.contains("to confirm") && footer.contains("to cancel");
    let approval_header = cues.iter().rposition(|line| {
        [
            "would you like to run",
            "would you like to make",
            "would you like to grant",
            "would you like to send input to",
            "do you want to approve",
            "do you want to proceed",
            "do you want to make this edit",
            "do you trust",
            "this command requires approval",
            "permission request",
            "approval required",
        ]
        .iter()
        .any(|prefix| line.starts_with(prefix))
            || line.ends_with("needs your approval.")
    });
    let decisions = cues
        .iter()
        .filter_map(|line| menu_line(line).map(|(_, text, _)| text))
        .collect::<Vec<_>>();
    let yes_no = decisions
        .iter()
        .any(|line| line.starts_with("yes") || line.starts_with("allow"))
        && decisions
            .iter()
            .any(|line| line.starts_with("no") || line.starts_with("deny"));
    let selected_option = visible.iter().zip(&cues).any(|(line, cue)| {
        line.trim_start().starts_with(['›', '❯', '>', '▸']) && menu_line(cue).is_some()
    });
    let question_header = cues.iter().rposition(|line| question_counter(line));
    let question_footer = [
        "to submit answer",
        "to submit all",
        "to navigate questions",
        "tab/arrow keys to navigate",
        "tab/arrow keys to switch",
        "enter to select",
    ]
    .iter()
    .any(|hint| footer.contains(hint));

    let (kind, first) =
        if approval_header.is_some() && (cancel || confirm || (yes_no && selected_option)) {
            ("approval", 0)
        } else if question_footer
            && (question_header.is_some()
                || !decisions.is_empty()
                || footer.contains("to submit answer"))
        {
            (
                "question",
                question_header.unwrap_or(visible.len().saturating_sub(18)),
            )
        } else if yes_no && (cancel || confirm) {
            ("approval", 0)
        } else {
            return None;
        };
    let lines = visible[first..]
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>();
    let menu_footer = cues[cues.len().saturating_sub(5)..].join(" | ");
    // Generic approvals may describe their target above the visible question.
    // Only offer action buttons when a recognized heading frames a visible target.
    let can_choose = kind == "question"
        || approval_header.is_some_and(|header| {
            let heading = &cues[header];
            let target = cues[header + 1..]
                .iter()
                .take_while(|line| menu_line(line).is_none());
            provider == "codex"
                && heading == "would you like to run the following command?"
                && target.clone().any(|line| {
                    line.strip_prefix("$ ")
                        .is_some_and(|cmd| !cmd.trim().is_empty())
                })
                && !target.clone().any(|line| line.contains('…'))
        });
    Some(TerminalPrompt {
        kind,
        text: if kind == "approval" {
            rows[display_start..end].join("\n")
        } else {
            lines.join("\n")
        }
        .trim()
        .to_owned(),
        menu: can_choose
            .then(|| terminal_menu(&lines, &menu_footer, kind, provider))
            .flatten(),
        first_row: if kind == "approval" {
            display_start
        } else {
            display_start.max(end.saturating_sub(30)) + first
        },
        end_row: end,
    })
}

#[derive(Default)]
pub(crate) struct ChatTerminalState {
    pub ticket: Value,
    pub mismatch: bool,
    pub approval_mode: Option<&'static str>,
    pub prompt: Option<TerminalPrompt>,
    pub output: Option<String>,
    pub run_id: Option<String>,
}

fn chat_is_bound(session: &Value) -> bool {
    session["workspaceKind"] != "direct" || !session["conversationId"].is_null()
}

pub(crate) fn chat_state(core: &Core, session: &Value, include_output: bool) -> ChatTerminalState {
    let mut state = ChatTerminalState::default();
    let Ok(terminal) = get(core, &json!({"id":session["id"]})) else {
        return state;
    };
    if terminal.exited.load(Ordering::SeqCst) {
        return state;
    }
    if !chat_thread_matches(&terminal, session) {
        state.mismatch = true;
        return state;
    }
    let screen = terminal.screen.lock().unwrap();
    let input = terminal.input.lock().unwrap();
    if input.thread_uncertain {
        state.mismatch = true;
        return state;
    }
    state.run_id = Some(terminal.run_id.clone());
    state.prompt = terminal_prompt(screen.screen(), &terminal.provider);
    if let Some(prompt) = &mut state.prompt {
        if !chat_is_bound(session)
            || input.chat_pasting
            || input
                .answered_prompt
                .as_ref()
                .is_some_and(|answered| answered.matches(prompt))
        {
            prompt.menu = None;
        } else if let Some(menu) = &mut prompt.menu {
            menu.input_revision = input.revision;
        }
    }
    if include_output && state.prompt.is_none() {
        let mut rows = screen
            .screen()
            .rows(0, screen.screen().size().1)
            .collect::<Vec<_>>();
        rows.truncate(
            rows.iter()
                .rposition(|line| !line.trim().is_empty())
                .map_or(0, |row| row + 1),
        );
        state.output = Some(rows[rows.len().saturating_sub(24)..].join("\n"));
    }
    state.approval_mode = if terminal.provider == "copilot" {
        provider_composer(screen.screen()).and_then(|(_, footer)| {
            if footer.contains("assisted approval") {
                Some("auto")
            } else if footer.contains("manual approval") {
                Some("manual")
            } else {
                None
            }
        })
    } else {
        None
    };
    if chat_is_bound(session)
        && !input.chat_pasting
        && !input.chat_pending
        && !input.terminal_slash
        && state.prompt.is_none()
        && native_prompt_ready(screen.screen(), &terminal.provider)
    {
        state.ticket = json!({"runId":terminal.run_id,"inputRevision":input.revision});
    }
    state
}

pub(crate) fn answer_chat_prompt(core: &Core, args: &Value) -> Result<Value> {
    let id = string(args, "id")?;
    let _ = string(args, "runId")?;
    let key = string(args, "key")?;
    let mut revision = args["inputRevision"]
        .as_u64()
        .context("Seçim güncel değil.")?;
    let mut selected = args["selected"].as_u64().context("Seçim güncel değil.")? as usize;
    let text = args["text"]
        .as_str()
        .map(custom_question_text)
        .transpose()?;
    if text.is_some() && !args["choice"].is_null() {
        bail!("Geçersiz seçim.");
    }
    let choice = if text.is_some() {
        0
    } else {
        args["choice"]
            .as_u64()
            .filter(|n| (1..=9).contains(n))
            .context("Geçersiz seçim.")? as usize
    };
    let mut awaiting = None;
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        {
            let _guard = core.operations.lock().unwrap();
            let session = core.session(id)?;
            let terminal = get(core, args)?;
            let screen = terminal.screen.lock().unwrap();
            let mut input = terminal.input.lock().unwrap();
            if input
                .answered_prompt
                .as_ref()
                .is_some_and(|answered| answered.key == key)
                || input.revision != revision
                || input.thread_uncertain
                || input.chat_pasting
                || input.terminal_slash
                || terminal.exited.load(Ordering::SeqCst)
                || session["archived"] == true
                || session["login"] == true
                || !chat_is_bound(&session)
                || !chat_thread_matches(&terminal, &session)
            {
                bail!("Soru değişti. Güncel seçeneklerden birini seç.");
            }
            let prompt = terminal_prompt(screen.screen(), &terminal.provider);
            if prompt.as_ref().is_some_and(|prompt| {
                input
                    .answered_prompt
                    .as_ref()
                    .is_some_and(|answered| answered.matches(prompt))
            }) {
                bail!("Soru değişti. Güncel seçeneklerden birini seç.");
            }
            let menu = prompt.and_then(|prompt| prompt.menu);
            if let Some(menu) = menu {
                let choice = if text.is_some() {
                    menu.custom_choice
                        .context("Bu soru için özel yanıtı Terminalde yaz.")?
                } else {
                    choice
                };
                if awaiting == Some(menu.selected) {
                    selected = menu.selected;
                    awaiting = None;
                }
                if menu.key != key || selected != menu.selected || choice > menu.choices.len() {
                    bail!("Soru değişti. Güncel seçeneklerden birini seç.");
                }
                if awaiting.is_none() {
                    if selected == choice {
                        if let Some(text) = &text {
                            input.chat_pasting = true;
                            let opened = if terminal.provider == "codex" {
                                input.write(b"\t")
                            } else {
                                Ok(())
                            };
                            let revision = input.revision;
                            drop(input);
                            drop(screen);
                            drop(_guard);
                            let result = opened.and_then(|()| {
                                submit_question_text(core, args, text, &menu, revision)
                            });
                            terminal.input.lock().unwrap().chat_pasting = false;
                            return result;
                        }
                        input.answered_prompt = Some(AnsweredPrompt {
                            key: menu.key,
                            question_context: None,
                            custom_choice: None,
                        });
                        input.write(b"\r")?;
                        return Ok(json!(true));
                    }
                    // Move one row at a time and verify the provider highlights
                    // the requested option before sending Enter.
                    let (key, next): (&[u8], usize) = if choice > selected {
                        (b"\x1b[B", selected + 1)
                    } else {
                        (b"\x1b[A", selected - 1)
                    };
                    input.write(key)?;
                    revision = input.revision;
                    awaiting = Some(next);
                }
            } else if awaiting.is_none() {
                bail!("Soru değişti. Güncel seçeneklerden birini seç.");
            }
        }
        if Instant::now() >= deadline {
            bail!("Sağlayıcı seçimi doğrulamadı. Terminali kontrol et.");
        }
        std::thread::sleep(Duration::from_millis(35));
    }
}

fn custom_question_text(text: &str) -> Result<String> {
    let text = text.trim();
    if text.is_empty()
        || text.len() > 8000
        || text.chars().count() > 4000
        || text.chars().any(char::is_control)
    {
        bail!("Özel yanıt boş, çok uzun veya birden fazla satır içeriyor.");
    }
    Ok(text.to_owned())
}

fn question_notes(screen: &vt100::Screen, context: &str) -> Option<String> {
    let prompt = terminal_prompt(screen, "codex")?;
    if prompt.kind != "question" || custom_prompt_context(&prompt.text, None) != context {
        return None;
    }
    let rows = screen.rows(0, screen.size().1).collect::<Vec<_>>();
    let footer = (prompt.first_row..prompt.end_row).rfind(|&row| {
        rows[row]
            .trim()
            .to_ascii_lowercase()
            .starts_with("tab or esc to clear notes")
    })?;
    let bottom = (prompt.first_row..footer).rfind(|&row| frame_line(&rows[row]))?;
    let top = (prompt.first_row..bottom).rfind(|&row| frame_line(&rows[row]))?;
    let row = top + 1;
    let line = rows.get(row)?.trim_start();
    let value = line.strip_prefix("› ")?;
    let column = rows[row].len() - value.len();
    let column = rows[row][..column].chars().count();
    verified_question_field(
        screen,
        row,
        column,
        bottom,
        &["Add notes", "Type your answer (optional)"],
    )
}

fn verified_question_field(
    screen: &vt100::Screen,
    row: usize,
    column: usize,
    end: usize,
    placeholders: &[&str],
) -> Option<String> {
    if !screen.bracketed_paste() || screen.hide_cursor() {
        return None;
    }
    let (cursor_row, cursor_col) = screen.cursor_position();
    let cursor_row = usize::from(cursor_row);
    let cursor_col = usize::from(cursor_col);
    let width = usize::from(screen.size().1);
    if !(row..end).contains(&cursor_row)
        || column > width
        || (cursor_row == row && cursor_col < column)
        || (row..cursor_row).any(|previous| !screen.row_wrapped(previous as u16))
    {
        return None;
    }
    let mut draft = String::new();
    for index in row..=cursor_row {
        let start = if index == row { column } else { 0 };
        let limit = if index == cursor_row {
            cursor_col
        } else {
            width
        };
        for col in start..limit {
            let cell = screen.cell(index as u16, col as u16)?;
            if !cell.is_wide_continuation() {
                if cell.contents().is_empty() {
                    draft.push(' ');
                } else {
                    draft.push_str(cell.contents());
                }
            }
        }
    }
    let tail = screen
        .rows(cursor_col as u16, (width - cursor_col) as u16)
        .nth(cursor_row)?;
    let later_empty = screen
        .rows(0, width as u16)
        .skip(cursor_row + 1)
        .take(end.saturating_sub(cursor_row + 1))
        .all(|line| line.trim().is_empty());
    if !later_empty {
        return None;
    }
    if tail.trim().is_empty() {
        return Some(draft);
    }
    // A placeholder is only empty when the visible native caret is at the
    // beginning and the provider renders the remaining text as a placeholder.
    let placeholder_style = screen
        .cell(row as u16, column as u16)
        .is_some_and(|cell| cell.dim() || cell.fgcolor() == vt100::Color::Idx(8));
    (draft.is_empty() && placeholder_style && placeholders.contains(&tail.trim())).then_some(draft)
}

fn inline_question_input(
    screen: &vt100::Screen,
    provider: &str,
    menu: &TerminalMenu,
) -> Option<String> {
    let custom = menu.custom_choice?;
    let prompt = terminal_prompt(screen, provider)?;
    if prompt.kind != "question" {
        return None;
    }
    let (context, input) = inline_question_context(&prompt.text, custom);
    let (_, focused) = input?;
    if context != menu.context || !focused {
        return None;
    }
    let rows = screen.rows(0, screen.size().1).collect::<Vec<_>>();
    let row = rows
        .iter()
        .enumerate()
        .take(prompt.end_row)
        .skip(prompt.first_row)
        .find_map(|(index, line)| {
            menu_line(line)
                .filter(|(number, _, selected)| *number == custom && *selected)
                .map(|_| index)
        })?;
    let end = rows
        .iter()
        .enumerate()
        .skip(row + 1)
        .find_map(|(index, line)| {
            (menu_line(line).is_some() || question_footer(line)).then_some(index)
        })
        .unwrap_or(prompt.end_row)
        .min(prompt.end_row);
    let line = rows[row].trim_start_matches(|ch: char| ch.is_whitespace() || ch == '│');
    let rest = line.strip_prefix(['›', '❯', '>', '▸'])?.trim_start();
    let rest = &rest[1..];
    let value = [". ", ") ", " - ", " – ", " — "]
        .iter()
        .find_map(|separator| rest.strip_prefix(separator))?;
    let column = rows[row][..rows[row].len() - value.len()].chars().count();
    let placeholder = menu.choices.get(custom - 1)?;
    verified_question_field(screen, row, column, end, &[placeholder])
}

fn submit_question_text(
    core: &Core,
    args: &Value,
    text: &str,
    menu: &TerminalMenu,
    mut revision: u64,
) -> Result<Value> {
    let deadline = Instant::now() + Duration::from_secs(4);
    let mut pasted = false;
    loop {
        {
            let _guard = core.operations.lock().unwrap();
            let session = core.session(string(args, "id")?)?;
            let terminal = get(core, args)?;
            let screen = terminal.screen.lock().unwrap();
            let mut input = terminal.input.lock().unwrap();
            if terminal.exited.load(Ordering::SeqCst)
                || input.revision != revision
                || input.thread_uncertain
                || !chat_is_bound(&session)
                || !chat_thread_matches(&terminal, &session)
                || session["archived"] == true
            {
                bail!("Soru değişti. Güncel seçeneklerden birini seç.");
            }
            let draft = if terminal.provider == "codex" {
                question_notes(screen.screen(), &menu.context)
            } else {
                inline_question_input(screen.screen(), &terminal.provider, menu)
            };
            if let Some(draft) = draft {
                let empty = draft.is_empty();
                if !pasted && empty {
                    input.write(format!("\x1b[200~{text}\x1b[201~").as_bytes())?;
                    revision = input.revision;
                    pasted = true;
                } else if pasted && draft == text {
                    input.answered_prompt = Some(AnsweredPrompt {
                        key: string(args, "key")?.to_owned(),
                        question_context: Some(menu.context.clone()),
                        custom_choice: if terminal.provider == "codex" {
                            None
                        } else {
                            menu.custom_choice
                        },
                    });
                    input.write(b"\r")?;
                    return Ok(json!(true));
                } else if !(pasted && (empty || text.starts_with(&draft))) {
                    bail!("Özel yanıt alanı doğrulanamadı. Terminali kontrol et.");
                }
            } else if terminal_prompt(screen.screen(), &terminal.provider).is_some_and(|prompt| {
                let current = if terminal.provider == "codex" {
                    custom_prompt_context(&prompt.text, None)
                } else {
                    inline_question_context(&prompt.text, menu.custom_choice.unwrap_or(0)).0
                };
                current != menu.context
            }) {
                bail!("Soru değişti. Güncel seçeneklerden birini seç.");
            }
        }
        if Instant::now() >= deadline {
            bail!("Özel yanıt alanı doğrulanamadı. Terminali kontrol et.");
        }
        std::thread::sleep(Duration::from_millis(80));
    }
}

fn chat_input(text: &str) -> Result<String> {
    chat_input_for(text, "codex")
}

fn chat_input_for(text: &str, provider: &str) -> Result<String> {
    let text = text.replace("\r\n", "\n");
    if text.trim().is_empty() || text.len() > 32 * 1024 || text.chars().count() > 16_000 {
        bail!("Sohbet mesajı boş veya çok uzun.");
    }
    if text
        .chars()
        .any(|c| c.is_control() && !matches!(c, '\n' | '\t'))
    {
        bail!("Sohbet mesajında terminal denetim karakterleri var.");
    }
    if text.trim_start().starts_with(['/', '!']) || (provider == "copilot" && text.trim() == "$") {
        bail!("Codex ve kabuk komutlarını Terminal görünümünde çalıştır.");
    }
    // ConPTY decodes these Windows input records as Shift+Enter key down and up.
    // The Codex composer drops pasted CR/LF in this mode.
    #[cfg(windows)]
    const NEWLINE: &str = "\x1b[201~\x1b[13;28;13;1;16;1_\x1b[13;28;13;0;16;1_\x1b[200~";
    #[cfg(not(windows))]
    const NEWLINE: &str = "\n";
    let text = text
        .trim_end()
        .replace('\n', if provider == "codex" { NEWLINE } else { "\r" });
    Ok(format!("\x1b[200~{text}\x1b[201~"))
}

fn codex_draft_visible(screen: &vt100::Screen, text: &str, indent: usize) -> bool {
    if !screen.bracketed_paste() || empty_codex_prompt(screen) || !codex_composer_unblocked(screen)
    {
        return false;
    }
    let first_line = text.trim_start().lines().next().unwrap_or_default();
    let excerpt: String = first_line
        .chars()
        .take_while(|&c| c != '\t')
        .take(12)
        .collect();
    let placeholder = format!("[Pasted Content {} chars]", first_line.chars().count());
    let (cursor_row, cursor_col) = screen.cursor_position();
    let mut last_line_start = cursor_row;
    while last_line_start > 0 && screen.row_wrapped(last_line_start - 1) {
        last_line_start -= 1;
    }
    let cursor_text = screen.contents_between(last_line_start, 0, cursor_row, cursor_col);
    let cursor_text = cursor_text.trim_end();
    let last_line = text.trim_end().lines().last().unwrap_or_default();
    let last_placeholder = format!("[Pasted Content {} chars]", last_line.chars().count());
    let suffix: String = last_line
        .chars()
        .rev()
        .take(8)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    if !cursor_text.ends_with(&suffix) && !cursor_text.ends_with(&last_placeholder) {
        return false;
    }
    screen
        .rows(0, screen.size().1)
        .take(cursor_row as usize + 1)
        .filter_map(|line| {
            let trimmed = line.trim_start();
            if line.len() - trimmed.len() != indent {
                return None;
            }
            trimmed
                .strip_prefix(['›', '❯', '>'])
                .map(|text| text.trim().to_owned())
        })
        .last()
        .is_some_and(|draft| draft.starts_with(&excerpt) || draft.starts_with(&placeholder))
}

pub(crate) fn send_chat(core: &Core, args: &Value) -> Result<Value> {
    let text = string(args, "text")?;
    let terminal = get(core, args)?;
    let input = if terminal.provider == "codex" {
        chat_input(text)?
    } else {
        chat_input_for(text, &terminal.provider)?
    };
    submit_chat_input(core, args, text, &input, |_, _| Ok(()))?;
    Ok(json!(true))
}

pub(crate) fn apply_chat_model(
    core: &Core,
    args: &Value,
    model: &str,
    effort: Option<&str>,
) -> Result<()> {
    let mut command = format!("/model {model}");
    if let Some(effort) = effort {
        command.push_str(&format!(" --reasoning={effort}"));
    }
    let confirmation = format!("• Model changed to {model} ");
    let terminal = get(core, args)?;
    let confirmations_before = codex_model_confirmations(
        terminal.screen.lock().unwrap().screen(),
        &confirmation,
        effort,
    );
    submit_chat_input(core, args, &command, &command, |terminal, revision| {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            {
                let screen = terminal.screen.lock().unwrap();
                let input = terminal.input.lock().unwrap();
                if terminal.exited.load(Ordering::SeqCst) || input.revision != revision {
                    bail!("Terminal değişti; ajan hazır olunca yeniden seç");
                }
                let screen = screen.screen();
                if empty_codex_prompt(screen)
                    && codex_model_confirmations(screen, &confirmation, effort)
                        > confirmations_before
                {
                    return Ok(());
                }
            }
            if Instant::now() >= deadline {
                bail!("Codex model değişikliğini doğrulamadı. Terminali kontrol et.");
            }
            std::thread::sleep(Duration::from_millis(35));
        }
    })
}

pub(crate) fn apply_copilot_approval(core: &Core, args: &Value, mode: &str) -> Result<()> {
    let (option, label) = if mode == "auto" {
        ("assisted", "assisted approval")
    } else {
        ("default", "manual approval")
    };
    let command = format!("/permissions {option}");
    submit_chat_input(core, args, &command, &command, |terminal, revision| {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            {
                let parser = terminal.screen.lock().unwrap();
                let input = terminal.input.lock().unwrap();
                if terminal.exited.load(Ordering::SeqCst) || input.revision != revision {
                    bail!("Terminal değişti; ajan hazır olunca yeniden seç");
                }
                let screen = parser.screen();
                if provider_composer(screen).is_some_and(|(draft, footer)| {
                    draft.trim().is_empty() && footer.contains(label)
                }) {
                    return Ok(());
                }
            }
            if Instant::now() >= deadline {
                bail!("Sağlayıcı onay değişikliğini doğrulamadı.");
            }
            std::thread::sleep(Duration::from_millis(35));
        }
    })
}

fn codex_model_confirmations(screen: &vt100::Screen, prefix: &str, effort: Option<&str>) -> usize {
    let rows = screen.rows(0, screen.size().1).collect::<Vec<_>>();
    let matches = |line: &str| {
        line.trim()
            .strip_prefix(prefix)
            .and_then(|text| text.strip_suffix(" for this session only"))
            .is_some_and(|value| effort.is_none_or(|expected| value == expected))
    };
    rows.iter()
        .enumerate()
        .filter(|(index, line)| {
            matches(line)
                || (line.trim_start().starts_with("• Model changed to ")
                    && rows
                        .get(index + 1)
                        .is_some_and(|next| matches(&format!("{} {}", line.trim(), next.trim()))))
        })
        .count()
}

fn submit_chat_input(
    core: &Core,
    args: &Value,
    text: &str,
    data: &str,
    confirm: impl FnOnce(&LiveTerminal, u64) -> Result<()>,
) -> Result<()> {
    let guard = core.operations.lock().unwrap();
    let id = string(args, "id")?;
    let s = core.session(id)?;
    if s["login"] == true || s["archived"] == true {
        bail!("Sohbet bu oturumda kullanılamıyor.");
    }
    let _ = string(args, "runId")?;
    let terminal = get(core, args)?;
    if !chat_thread_matches(&terminal, &s) || !chat_is_bound(&s) {
        bail!("Bu konuşmayı Terminal görünümünde bağlayıp yeniden başlat.");
    }
    let screen = terminal.screen.lock().unwrap();
    let mut input = terminal.input.lock().unwrap();
    if terminal.exited.load(Ordering::SeqCst)
        || input.chat_pasting
        || input.chat_pending
        || input.terminal_slash
        || input.thread_uncertain
        || args["inputRevision"].as_u64() != Some(input.revision)
        || !provider_prompt_ready(screen.screen(), &terminal.provider)
    {
        bail!("Sağlayıcı hazır değil. Terminalde onay veya taslak olup olmadığına bak.");
    }
    let composer_row = screen.screen().cursor_position().0 as usize;
    let indent = screen
        .screen()
        .rows(0, screen.screen().size().1)
        .nth(composer_row)
        .map_or(0, |line| line.len() - line.trim_start().len());
    input.write(data.as_bytes())?;
    input.chat_pasting = true;
    let pasted_revision = input.revision;
    drop(input);
    drop(screen);
    drop(guard);

    let result = submit_chat_draft(core, args, id, text, indent, pasted_revision)
        .and_then(|revision| confirm(&terminal, revision));
    terminal.input.lock().unwrap().chat_pasting = false;
    result
}

fn submit_chat_draft(
    core: &Core,
    args: &Value,
    id: &str,
    text: &str,
    indent: usize,
    pasted_revision: u64,
) -> Result<u64> {
    // Codex may treat Enter as part of the paste burst until the draft is rendered.
    let timeout = if get(core, args)?.provider == "codex" {
        2
    } else {
        8
    };
    let deadline = Instant::now() + Duration::from_secs(timeout);
    std::thread::sleep(Duration::from_millis(150));
    loop {
        {
            let _guard = core.operations.lock().unwrap();
            let current = get(core, args)?;
            let screen = current.screen.lock().unwrap();
            let input = &mut *current.input.lock().unwrap();
            if current.exited.load(Ordering::SeqCst)
                || input.revision != pasted_revision
                || input.thread_uncertain
                || !chat_thread_matches(&current, &core.session(id)?)
            {
                bail!("Terminal değişti. Göndermeden önce terminaldeki taslağı kontrol et.");
            }
            let verified = if terminal_prompt(screen.screen(), &current.provider).is_some() {
                false
            } else if current.provider == "codex" {
                codex_draft_visible(screen.screen(), text, indent)
            } else {
                provider_composer(screen.screen()).is_some_and(|(draft, _)| {
                    draft
                        .chars()
                        .filter(|c| !c.is_whitespace())
                        .eq(text.chars().filter(|c| !c.is_whitespace()))
                })
            };
            if verified {
                input.write(b"\r")?;
                input.chat_pending = true;
                return Ok(input.revision);
            }
        }
        if Instant::now() >= deadline {
            bail!("Sağlayıcı taslağı doğrulanamadı. Terminalden kontrol et.");
        }
        std::thread::sleep(Duration::from_millis(25));
    }
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
            let terminal = get(core, args)?;
            let data = string(args, "data")?.as_bytes();
            if matches!(terminal.provider.as_str(), "codex" | "claude" | "copilot") {
                let screen = terminal.screen.lock().unwrap();
                if terminal.exited.load(Ordering::SeqCst) {
                    bail!("Oturum kapalı");
                }
                let composer_empty = provider_prompt_ready(screen.screen(), &terminal.provider);
                let visible_draft = data
                    .iter()
                    .any(|byte| matches!(byte, b'\r' | b'\n'))
                    .then(|| {
                        if terminal.provider == "codex" {
                            codex_composer_text(screen.screen())
                        } else {
                            provider_composer(screen.screen())
                                .map(|(text, _)| text.trim().to_owned())
                        }
                    })
                    .flatten();
                let mut input = terminal.input.lock().unwrap();
                if terminal.provider == "copilot"
                    && ((composer_empty && matches!(data, b"\x1b[D" | b"\t"))
                        || data.starts_with(b"\x1b[<0;"))
                {
                    input.thread_uncertain = true;
                }
                input.write_from_terminal(data, composer_empty, visible_draft.as_deref())?;
            } else {
                terminal.write(data)?;
            }
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
    fn chat_submission_frames_multiline_text_once_and_rejects_terminal_commands() {
        #[cfg(windows)]
        let expected = "\x1b[200~First\x1b[201~\x1b[13;28;13;1;16;1_\x1b[13;28;13;0;16;1_\x1b[200~second\x1b[201~";
        #[cfg(not(windows))]
        let expected = "\x1b[200~First\nsecond\x1b[201~";
        assert_eq!(chat_input("First\r\nsecond").unwrap(), expected);
        for invalid in [
            "",
            " \n",
            " /model",
            "\t!dir",
            "injected\x1b[201~",
            "bypass\rnewline",
        ] {
            assert!(chat_input(invalid).is_err(), "{invalid:?}");
        }
    }
    #[test]
    fn chat_requires_an_idle_bracketed_codex_composer() {
        let mut parser = vt100::Parser::new(10, 100, 0);
        parser.process("› Ask Codex to do anything\x1b[1;3H".as_bytes());
        assert!(!codex_prompt_ready(parser.screen()));
        parser.process(b"\x1b[?2004h");
        assert!(codex_prompt_ready(parser.screen()));
        parser.process(b"\x1b[2;1HWorking (esc to interrupt)\x1b[1;3H");
        assert!(!codex_prompt_ready(parser.screen()));
        parser.process(
            "\x1b[2J\x1b[H• Working (3s · esc to interrupt)\r\n› Ask Codex to do anything\x1b[2;3H"
                .as_bytes(),
        );
        assert!(!codex_prompt_ready(parser.screen()));
        parser.process("\x1b[2J\x1b[H› /resume".as_bytes());
        assert_eq!(
            codex_composer_text(parser.screen()).as_deref(),
            Some("/resume")
        );
    }

    #[test]
    fn chat_ignores_activity_phrases_in_history_and_drafts() {
        let mut parser = vt100::Parser::new(10, 100, 0);
        parser.process(
            "\x1b[?2004hThe message says update available\r\n› Ask Codex to do anything\x1b[2;3H"
                .as_bytes(),
        );
        assert!(codex_prompt_ready(parser.screen()));
        let draft = "Explain why it says do you trust";
        parser.process(format!("\x1b[2;1H\x1b[2K› {draft}").as_bytes());
        assert!(codex_draft_visible(parser.screen(), draft, 0));

        parser.process(
            format!("\x1b[2J\x1b[HAn update available\r\n─────────\r\n❯ {draft}\r\n─────────\r\nmanual approval")
                .as_bytes(),
        );
        assert!(provider_composer(parser.screen()).is_some_and(|(actual, _)| actual == draft));
        parser.process(b" \x1b[6;1HEsc to cancel");
        assert!(provider_composer(parser.screen()).is_none());
    }

    #[test]
    fn chat_distinguishes_live_requests_and_old_terminal_output() {
        for provider in ["codex", "claude", "copilot"] {
            let approval = "\x1b[?2004hWould you like to run the following command?\r\nRead build notes\r\n› 1. Yes\r\n  2. No\r\nEnter to confirm · Esc to cancel";
            let mut parser = vt100::Parser::new(14, 100, 0);
            parser.process(approval.as_bytes());
            let prompt = terminal_prompt(parser.screen(), provider).unwrap();
            assert_eq!(prompt.kind, "approval");
            assert!(prompt.text.contains("Read build notes"));
            let composer = match provider {
                "codex" => "\r\n› Ask Codex to do anything\x1b[6;3H",
                "claude" => "\r\n─────────\r\n❯ \r\n─────────\r\n? for shortcuts",
                _ => "\r\n─────────\r\n❯ \r\n─────────\r\nInteractive · Manual Approval",
            };
            parser.process(composer.as_bytes());
            assert!(terminal_prompt(parser.screen(), provider).is_none());
            assert!(provider_prompt_ready(parser.screen(), provider));

            parser.process("\x1b[2J\x1b[HQuestion 1/1 (1 unanswered)\r\nWhich format should we use?\r\n› 1. CSV\r\n  2. JSON\r\nEnter to submit answer · Esc to interrupt".as_bytes());
            let prompt = terminal_prompt(parser.screen(), provider).unwrap();
            assert_eq!(prompt.kind, "question");
            assert!(prompt.text.contains("Which format"));
            assert!(!provider_prompt_ready(parser.screen(), provider));

            parser.process("\x1b[2J\x1b[HThe guide says:\r\nWould you like to run a command?\r\n1. Yes\r\n2. No\r\nWorking…".as_bytes());
            assert!(terminal_prompt(parser.screen(), provider).is_none());
        }
    }

    #[test]
    fn approval_below_the_copilot_composer_prevents_chat_submission() {
        let mut parser = vt100::Parser::new(14, 100, 0);
        parser.process("\x1b[?2004h─────────\r\n❯ \r\n─────────\r\nInteractive · Manual Approval\r\nApproval required\r\n❯ 1. Yes\r\n  2. No".as_bytes());
        assert!(native_prompt_ready(parser.screen(), "copilot"));
        assert!(!provider_prompt_ready(parser.screen(), "copilot"));
        assert_eq!(
            terminal_prompt(parser.screen(), "copilot").unwrap().kind,
            "approval"
        );
    }

    #[test]
    fn approval_choices_require_a_recognized_visible_target() {
        let mut parser = vt100::Parser::new(38, 100, 0);
        let choices = "› 1. Yes\r\n  2. No\r\nEnter to confirm · Esc to cancel";
        let recognized = "Would you like to run the following command?";
        let mut show = |input: &str| {
            parser.process(format!("\x1b[2J\x1b[H{input}").as_bytes());
            terminal_prompt(parser.screen(), "codex").unwrap()
        };
        let generic = show(&format!(
            "Read sample A\r\nDo you want to proceed?\r\n{choices}"
        ));
        assert!(generic.text.contains("Read sample A"));
        assert!(generic.menu.is_none());
        let a = show(&format!("{recognized}\r\n$ echo sample A\r\n{choices}"));
        let b = show(&format!("{recognized}\r\n$ echo sample B\r\n{choices}"));
        assert_ne!(a.menu.unwrap().key, b.menu.unwrap().key);
        let a = show(&format!(
            "{recognized}\r\n$ echo sample\r\n  continuation\r\n{choices}"
        ));
        let b = show(&format!(
            "{recognized}\r\n$ echo sample\r\n continuation\r\n{choices}"
        ));
        assert_ne!(a.menu.unwrap().key, b.menu.unwrap().key);
        for incomplete in ["Reason: Read the notes", "────────", "$ echo sample…"]
        {
            assert!(show(&format!("{recognized}\r\n{incomplete}\r\n{choices}"))
                .menu
                .is_none());
        }
        let cropped = show(&format!(
            "{recognized}\r\n$ echo sample A\r\n{}Limited output\r\n{choices}",
            "more context\r\n".repeat(31)
        ));
        assert_eq!(cropped.kind, "approval");
        assert!(cropped.text.contains("$ echo sample A"));
        assert!(cropped.menu.is_none());

        assert!(!chat_is_bound(
            &json!({"workspaceKind":"direct","conversationId":null})
        ));
        assert!(chat_is_bound(
            &json!({"workspaceKind":"direct","conversationId":"fixture"})
        ));
    }

    #[test]
    fn native_choice_menu_requires_numbered_options_and_one_highlight() {
        let initial = [
            "Which should I change?",
            "› 1 - Only one",
            "  2 - Both. Include tests.",
        ];
        let moved = [
            "Which should I change?",
            "  1 - Only one",
            "› 2 - Both. Include tests.",
        ];
        let footer = "tab to add notes | enter to submit answer";
        let first = terminal_menu(&initial, footer, "question", "codex").unwrap();
        let second = terminal_menu(&moved, footer, "question", "codex").unwrap();
        assert_eq!(first.key, second.key);
        assert_eq!(second.choices[1], "Both. Include tests.");
        assert_eq!((first.selected, second.selected), (1, 2));
        for footer in [
            "tab to add notes | ⌃enter to submit answer",
            "tab to add notes | enter to submit answer | select multiple",
        ] {
            assert!(terminal_menu(&initial, footer, "question", "codex").is_none());
        }
        assert!(terminal_menu(&["› 1. One", "› 2. Two"], footer, "question", "codex").is_none());
        assert!(terminal_menu(&["› 1. One", "  3. Three"], footer, "question", "codex").is_none());
    }

    #[test]
    fn custom_question_reply_requires_the_same_question_and_a_verified_note_field() {
        let question = "Question 1/1 (1 unanswered)\r\nWhat should I do instead?\r\n  1. Only one\r\n    Leave other files unchanged.\r\n  2. All of them\r\n    Apply to every file.\r\n› 3. None of the above\r\n    Optionally, add details in notes (tab)";
        let mut parser = vt100::Parser::new(25, 110, 0);
        parser.process(format!("\x1b[?2004h{question}\r\nTab to add notes · Enter to submit answer · Esc to interrupt").as_bytes());
        let menu = terminal_prompt(parser.screen(), "codex")
            .unwrap()
            .menu
            .unwrap();
        assert_eq!(menu.custom_choice, Some(3));
        assert_eq!(menu.question, "What should I do instead?");
        assert_eq!(menu.descriptions[0], "Leave other files unchanged.");
        for note in [
            "",
            "Change only the configuration file.",
            "1. Change one file",
            "a b",
            "Show me how to select a profile",
        ] {
            let visible = if note.is_empty() {
                "\x1b[2mAdd notes\x1b[22m"
            } else {
                note
            };
            parser.process(format!("\x1b[2J\x1b[H{question}\r\n────────\r\n› {visible}\r\n────────\r\nTab or Esc to clear notes · Enter to submit answer\x1b[10;{}H", 3 + note.chars().count()).as_bytes());
            assert_eq!(
                question_notes(parser.screen(), &menu.context).as_deref(),
                Some(note)
            );
            assert!(question_notes(parser.screen(), "different question").is_none());
            let prompt = terminal_prompt(parser.screen(), "codex").unwrap();
            let answered = AnsweredPrompt {
                key: menu.key.clone(),
                question_context: Some(menu.context.clone()),
                custom_choice: None,
            };
            assert!(answered.matches(&prompt));
            parser.process(b"\x1b[?25l");
            assert!(question_notes(parser.screen(), &menu.context).is_none());
            parser.process(b"\x1b[?25h");
        }
        for invalid in ["", "two\nlines", "unsafe\x1b", "control\ttext"] {
            assert!(custom_question_text(invalid).is_err());
        }
        assert_eq!(
            custom_question_text("   Kendi yanıtım  ").unwrap(),
            "Kendi yanıtım"
        );
    }

    #[test]
    fn provider_inline_question_inputs_preserve_the_question_and_require_a_text_cursor() {
        for (provider, placeholder) in [
            ("claude", "Type something."),
            ("copilot", "Other (type your answer)"),
        ] {
            let question = "Question 1/2\r\nWhich files should I change?\r\n  1. Only one\r\n    Keep the others unchanged.\r\n  2. All of them";
            let actions = if provider == "claude" {
                "\r\n  4. Chat about this"
            } else {
                ""
            };
            let footer = "\r\nEnter to select · ↑/↓ to navigate · Esc to cancel";
            let mut parser = vt100::Parser::new(20, 110, 0);
            let show = |parser: &mut vt100::Parser, draft: &str| {
                parser.process(
                    format!(
                        "\x1b[?2004h\x1b[2J\x1b[H{question}\r\n❯ 3. {}{actions}{footer}\x1b[6;{}H",
                        if draft.is_empty() {
                            format!("\x1b[2m{placeholder}\x1b[22m")
                        } else {
                            draft.to_owned()
                        },
                        6 + draft.chars().count()
                    )
                    .as_bytes(),
                );
            };
            show(&mut parser, "");
            let menu = terminal_prompt(parser.screen(), provider)
                .unwrap()
                .menu
                .unwrap();
            assert_eq!(menu.custom_choice, Some(3));
            assert_eq!(menu.choices.len(), 3);
            assert_eq!(
                inline_question_input(parser.screen(), provider, &menu).as_deref(),
                Some("")
            );
            let reply = "Yalnızca yapılandırma dosyasını güncelle.";
            show(&mut parser, reply);
            assert_eq!(
                inline_question_input(parser.screen(), provider, &menu).as_deref(),
                Some(reply)
            );
            let prompt = terminal_prompt(parser.screen(), provider).unwrap();
            let answered = AnsweredPrompt {
                key: menu.key.clone(),
                question_context: Some(menu.context.clone()),
                custom_choice: Some(3),
            };
            assert!(answered.matches(&prompt));
            for exact in [
                "a b",
                "ab",
                "1. One file",
                "Show me how to select a profile",
                "Other: keep the files",
                placeholder,
            ] {
                show(&mut parser, exact);
                assert_eq!(
                    inline_question_input(parser.screen(), provider, &menu).as_deref(),
                    Some(exact)
                );
            }
            show(&mut parser, reply);
            parser.process(b"\x1b[?25l");
            assert!(inline_question_input(parser.screen(), provider, &menu).is_none());
            parser.process(b"\x1b[?25h\x1b[?2004l");
            assert!(inline_question_input(parser.screen(), provider, &menu).is_none());
            show(&mut parser, reply);
            parser.process(b"\x1b[12;1H");
            assert!(inline_question_input(parser.screen(), provider, &menu).is_none());
            show(&mut parser, reply);
            parser.process(b"\x1b[2;1HWhich tests should I run?\x1b[K\x1b[6;6H");
            assert!(inline_question_input(parser.screen(), provider, &menu).is_none());
            parser.process(format!("\x1b[2J\x1b[HDo you want to proceed?\r\n❯ 1. Yes\r\n  2. No\r\n  3. {placeholder}\r\nEnter to confirm · Esc to cancel").as_bytes());
            assert!(terminal_prompt(parser.screen(), provider)
                .unwrap()
                .menu
                .is_none());
        }
    }

    #[test]
    fn chat_recognizes_a_screenshot_path_wrapped_before_the_cursor() {
        let mut parser = vt100::Parser::new(10, 30, 0);
        let draft = "Read the screenshot in \"file.png\"";
        parser.process(format!("\x1b[?2004h› {draft}").as_bytes());
        let (row, col) = parser.screen().cursor_position();
        assert_eq!(row, 1);
        assert!(col > 0 && col < 8);
        assert!(codex_draft_visible(parser.screen(), draft, 0));
    }
    #[test]
    fn terminal_navigation_invalidates_chat_when_thread_may_have_changed() {
        let mut input = TerminalInput {
            writer: Box::new(Vec::new()),
            revision: 0,
            chat_pasting: false,
            chat_pending: false,
            terminal_slash: false,
            thread_uncertain: false,
            answered_prompt: None,
        };
        input.write_from_terminal(b"/", true, None).unwrap();
        input
            .write_from_terminal(b"\r", false, Some("/status"))
            .unwrap();
        assert!(!input.thread_uncertain);
        input.write_from_terminal(b"\x1b[A", true, None).unwrap();
        input
            .write_from_terminal(b"\r", false, Some("Recalled prompt"))
            .unwrap();
        assert!(!input.thread_uncertain);
        input.write_from_terminal(b"\x1b[A", true, None).unwrap();
        input
            .write_from_terminal(b"\r", false, Some("/resume"))
            .unwrap();
        assert!(input.thread_uncertain);
    }
    #[test]
    fn recognizes_mcp_config_failure_before_any_conversation_started() {
        let failure = b"\x1b[HError: \x1b[?25linvalid type: string \"[mcp,C:\\\\lease.json]\", expected a sequence\r\nin `mcp_servers.workflow.args`\x1b[5;1H";
        assert!(is_codex_mcp_bootstrap_error(failure));
        assert!(!is_codex_mcp_bootstrap_error(
            b"Error: authentication failed"
        ));
        assert!(!is_codex_mcp_bootstrap_error(
            &[
                b"Earlier conversation\r\n",
                failure.strip_prefix(b"\x1b[H").unwrap()
            ]
            .concat()
        ));
    }

    #[test]
    fn login_never_receives_conversation_options() {
        for provider in ["codex", "claude", "copilot"] {
            let session = json!({"id":"session-id","provider":provider,"login":true,"model":"chosen-model","effort":"high","approvalMode":"auto"});
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

    #[test]
    fn launches_and_resumes_with_the_selected_approval_mode() {
        for resume in [None, Some("conversation")] {
            let codex = json!({"id":"session","provider":"codex","approvalMode":"auto"});
            let args = provider_arguments(&codex, resume).unwrap();
            assert!(args.ends_with(&[
                "-a".into(),
                "on-request".into(),
                "-c".into(),
                "approvals_reviewer='auto_review'".into(),
            ]));
            let codex = json!({"id":"session","provider":"codex","approvalMode":"manual"});
            let args = provider_arguments(&codex, resume).unwrap();
            assert!(args.ends_with(&[
                "-a".into(),
                "on-request".into(),
                "-c".into(),
                "approvals_reviewer='user'".into(),
            ]));
            for (mode, flag) in [
                ("edits", "acceptEdits"),
                ("manual", "default"),
                ("plan", "plan"),
                ("auto", "auto"),
            ] {
                let claude = json!({"id":"session","provider":"claude","approvalMode":mode});
                assert!(provider_arguments(&claude, resume)
                    .unwrap()
                    .ends_with(&["--permission-mode".into(), flag.into()]));
            }
            for provider in ["codex", "claude"] {
                let default = json!({"id":"session","provider":provider,"approvalMode":"default"});
                let args = provider_arguments(&default, resume).unwrap();
                assert!(!args
                    .iter()
                    .any(|arg| arg == "-a" || arg == "--permission-mode"));
            }
        }
    }
}
