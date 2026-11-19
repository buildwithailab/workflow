use crate::{
    core::{external_path, string, Core},
    history, terminal,
};
use anyhow::{bail, Result};
use serde::Serialize;
use serde_json::{json, Value};
use std::{
    collections::{HashSet, VecDeque},
    fs::File,
    io::{BufRead, BufReader, Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime},
};

const MAX_LOG_BYTES: u64 = 8 * 1024 * 1024;
const MAX_TURNS: usize = 60;
const MAX_STEPS: usize = 50;
const MAX_TEXT_CHARS: usize = 24_000;
#[cfg(windows)]
pub(crate) const MAX_SCREENSHOT_BYTES: usize = 40 * 1024 * 1024;

#[derive(Default)]
pub(crate) struct LogCache {
    scope: String,
    file: Option<PathBuf>,
    retry_after: Option<Instant>,
}

fn locate_log(core: &Core, session: &Value, thread: &str) -> Result<Option<PathBuf>> {
    let scope = json!([
        session["id"],
        session["provider"],
        session["accountId"],
        session["directory"],
        session["previousDirectory"],
        thread
    ])
    .to_string();
    let (cached, waiting) = {
        let cache = core.chat_log.lock().unwrap();
        (
            (cache.scope == scope).then(|| cache.file.clone()).flatten(),
            cache.scope == scope
                && cache
                    .retry_after
                    .is_some_and(|until| Instant::now() < until),
        )
    };
    if let Some(file) = cached.filter(|path| history::matches_bound_log(path, session, thread)) {
        return Ok(Some(file));
    }
    if waiting {
        return Ok(None);
    }
    let file = history::bound_log(core, session, thread)?;
    *core.chat_log.lock().unwrap() = LogCache {
        scope,
        retry_after: file
            .is_none()
            .then(|| Instant::now() + Duration::from_secs(3)),
        file: file.clone(),
    };
    Ok(file)
}

#[derive(Default, Serialize)]
struct Turn {
    prompt: String,
    steps: Vec<Step>,
    answer: String,
    state: &'static str,
    #[serde(skip)]
    legacy_answer: String,
    id: String,
    position: u64,
    #[serde(skip)]
    started_ms: Option<i64>,
    #[serde(skip)]
    calls: HashSet<String>,
    #[serde(skip)]
    prompt_from_item: bool,
}

#[derive(Serialize)]
struct Step {
    kind: &'static str,
    text: String,
}

fn clipped(text: &str) -> String {
    let mut chars = text.trim().chars();
    let mut value: String = chars.by_ref().take(MAX_TEXT_CHARS).collect();
    if chars.next().is_some() {
        value.push('…');
    }
    value
}

impl Turn {
    fn step(&mut self, kind: &'static str, text: &str) {
        if self.steps.len() < MAX_STEPS {
            let text = clipped(text);
            if !self
                .steps
                .last()
                .is_some_and(|last| last.kind == kind && last.text == text && kind == "commentary")
            {
                self.steps.push(Step { kind, text });
            }
        }
    }

    fn message(&mut self, item: &Value, text: &str) {
        if text.trim().is_empty() {
            return;
        }
        match item["phase"].as_str() {
            Some("final_answer") => self.answer = clipped(text),
            Some("commentary" | "partial_answer") => self.step("commentary", text),
            _ => self.legacy_answer = clipped(text),
        }
    }

    fn finish(&mut self, state: &'static str, fallback: Option<&str>) {
        // Providers may report a final idle/duration event after a cancellation
        // or API failure. Only the next user turn can reset that outcome.
        if self.state == "interrupted" && state == "completed" {
            return;
        }
        self.state = state;
        if state == "completed" && self.answer.is_empty() {
            self.answer = fallback
                .filter(|value| !value.trim().is_empty())
                .map(clipped)
                .unwrap_or_else(|| std::mem::take(&mut self.legacy_answer));
        }
    }
}

fn append_turn(turns: &mut VecDeque<Turn>, turn: &mut Turn) {
    let mut turn = std::mem::take(turn);
    if turn.prompt.is_empty() {
        return;
    }
    if turn.state.is_empty() {
        turn.state = "working";
    }
    if turns.len() == MAX_TURNS {
        turns.pop_front();
    }
    turns.push_back(turn);
}

fn message_text(item: &Value) -> String {
    item["content"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|part| part["text"].as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

fn timestamp_ms(event: &Value) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(event["timestamp"].as_str()?)
        .ok()
        .map(|time| time.timestamp_millis())
}

fn apply_provider_event(
    provider: &str,
    entry: &Value,
    turns: &mut VecDeque<Turn>,
    turn: &mut Turn,
    position: u64,
) {
    let kind = entry["type"].as_str().unwrap_or_default();
    let is_claude = provider == "claude";
    let message = if is_claude {
        &entry["message"]
    } else {
        &entry["data"]
    };
    let text = || {
        message["content"]
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| message_text(message))
    };
    if (is_claude && kind == "user") || (!is_claude && kind == "user.message") {
        let prompt = text();
        if prompt.trim().is_empty()
            || (is_claude
                && ["<local-command", "<command-name>", "<system-reminder>"]
                    .iter()
                    .any(|prefix| prompt.trim_start().starts_with(prefix)))
        {
            return;
        }
        if !turn.prompt.is_empty() && !turn.answer.is_empty() && turn.state == "working" {
            turn.finish("completed", None);
        }
        append_turn(turns, turn);
        turn.prompt = clipped(&prompt);
        turn.position = position;
        turn.started_ms = timestamp_ms(entry);
        turn.state = "working";
        return;
    }
    if turn.prompt.is_empty() {
        return;
    }
    let mut tool = |name: &str, id: Option<&str>| {
        if id.is_some_and(|id| !turn.calls.insert(id.to_owned())) {
            return;
        }
        if !turn.answer.is_empty() {
            let commentary = std::mem::take(&mut turn.answer);
            turn.step("commentary", &commentary);
        }
        let kind = match name.to_ascii_lowercase().as_str() {
            "bash" | "powershell" | "run_shell_command" => "command",
            "edit" | "write" | "apply_patch" | "create" => "edit",
            _ => "tool",
        };
        turn.step(kind, name);
    };
    if is_claude && kind == "assistant" {
        for part in message["content"].as_array().into_iter().flatten() {
            if part["type"] == "tool_use" {
                if let Some(name) = part["name"].as_str() {
                    tool(name, part["id"].as_str());
                }
            }
        }
    } else if kind == "tool.execution_start" {
        if let Some(name) = message["toolName"].as_str() {
            tool(name, message["toolCallId"].as_str());
        }
    }
    if kind == "assistant" || kind == "assistant.message" {
        let answer = text();
        if !answer.trim().is_empty() {
            if is_claude
                && message["content"]
                    .as_array()
                    .is_some_and(|parts| parts.iter().any(|part| part["type"] == "tool_use"))
            {
                turn.step("commentary", &answer);
            } else {
                turn.answer = clipped(&answer);
            }
        }
    }
    let api_error = entry["isApiErrorMessage"] == true
        || message["isApiErrorMessage"] == true
        || kind == "assistant" && (!entry["error"].is_null() || !message["error"].is_null());
    if api_error
        || matches!(kind, "session.error" | "assistant.turn_cancelled" | "abort")
        || is_claude
            && matches!(
                message["stop_reason"].as_str(),
                Some("max_tokens" | "model_context_window_exceeded")
            )
    {
        turn.finish("interrupted", None);
    } else if !turn.answer.is_empty()
        && (matches!(kind, "assistant.turn_end" | "session.idle")
            || kind == "system" && entry["subtype"] == "turn_duration"
            || is_claude
                && matches!(
                    message["stop_reason"].as_str(),
                    Some("end_turn" | "stop_sequence")
                ))
    {
        turn.finish("completed", None);
    }
}

fn parse_log(data: &[u8], start: u64, provider: &str) -> VecDeque<Turn> {
    let mut turns = VecDeque::new();
    let mut turn = Turn::default();
    let mut lines = data.split_inclusive(|&byte| byte == b'\n');
    let mut position = start;
    if start != 0 {
        position += lines.next().map_or(0, |line| line.len() as u64);
    }
    for line in lines.take_while(|line| line.ends_with(b"\n")) {
        let line_position = position;
        position += line.len() as u64;
        apply_log_line(line, line_position, provider, &mut turns, &mut turn);
    }
    append_turn(&mut turns, &mut turn);
    turns
}

const MAX_LOG_LINE: usize = 2 * 1024 * 1024;

fn apply_log_line(
    line: &[u8],
    line_position: u64,
    provider: &str,
    turns: &mut VecDeque<Turn>,
    turn: &mut Turn,
) {
    if line.len() > MAX_LOG_LINE {
        return;
    }
    let Ok(entry) = serde_json::from_slice::<Value>(line) else {
        return;
    };
    let item = &entry["payload"];
    if entry["isSidechain"] == true || !entry["agentId"].is_null() || entry["isMeta"] == true {
        return;
    }
    if provider != "codex" {
        apply_provider_event(provider, &entry, turns, turn, line_position);
        return;
    }
    match (entry["type"].as_str(), item["type"].as_str()) {
        (Some("event_msg"), Some("task_started" | "turn_started")) => {
            if !turn.prompt.is_empty()
                && (turn.state == "completed"
                    || turn.state == "interrupted"
                    || !turn.id.is_empty()
                        && item["turn_id"].as_str().is_some_and(|id| id != turn.id))
            {
                append_turn(turns, turn);
            }
            if let Some(id) = item["turn_id"].as_str() {
                turn.id = id.to_owned();
            }
        }
        (Some("event_msg"), Some("user_message")) => {
            if let Some(text) = item["message"].as_str() {
                if turn.prompt_from_item && turn.prompt == clipped(text) {
                    turn.prompt_from_item = false;
                    return;
                }
                if !turn.prompt.is_empty() {
                    append_turn(turns, turn);
                }
                turn.prompt = clipped(text);
                turn.position = line_position;
                turn.started_ms = timestamp_ms(&entry);
            }
        }
        (Some("response_item"), Some("message"))
            if item["role"] == "user" && turn.prompt.is_empty() =>
        {
            let metadata = &item["internal_chat_message_metadata_passthrough"];
            let parts = item["content"].as_array().into_iter().flatten();
            let text = parts
                .enumerate()
                .filter(|(index, _)| {
                    metadata["content_item_kinds"][*index] == "user.text"
                        && metadata["content_item_metadata"][*index]["provenance"]["type"] == "user"
                })
                .filter_map(|(_, part)| part["text"].as_str())
                .collect::<Vec<_>>()
                .join("\n");
            if !text.trim().is_empty() {
                turn.prompt = clipped(&text);
                turn.prompt_from_item = true;
                turn.position = line_position;
                turn.started_ms = timestamp_ms(&entry);
            }
        }
        (Some("event_msg"), Some("task_complete" | "turn_complete")) if !turn.prompt.is_empty() => {
            turn.finish("completed", item["last_agent_message"].as_str());
        }
        (Some("event_msg"), Some("turn_aborted")) if !turn.prompt.is_empty() => {
            turn.finish("interrupted", None)
        }
        (Some("event_msg"), Some("agent_message")) if !turn.prompt.is_empty() => {
            turn.message(item, item["message"].as_str().unwrap_or_default());
        }
        (Some("response_item"), Some("message"))
            if item["role"] == "assistant" && !turn.prompt.is_empty() =>
        {
            turn.message(item, &message_text(item));
        }
        (Some("response_item"), Some("function_call" | "custom_tool_call"))
            if !turn.prompt.is_empty() =>
        {
            if let Some(name) = item["name"].as_str() {
                if item["call_id"]
                    .as_str()
                    .is_none_or(|id| turn.calls.insert(id.to_owned()))
                {
                    let kind = match name {
                        "apply_patch" => "edit",
                        "exec_command" | "shell_command" | "shell" | "write_stdin" => "command",
                        _ => "tool",
                    };
                    turn.step(kind, name);
                }
            }
        }
        _ => {}
    }
}

// Uses the same explicit choice format as the reply card. A question mark alone
// is common in explanations and must not mark a session as waiting.
fn reply_needs_choice(answer: &str) -> bool {
    use std::sync::LazyLock;
    static CHOICE: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new(r"^([1-9])(?:[.)]\s+|\s+[-–—]\s+)(.+)$").unwrap());
    static ASKS: LazyLock<regex::Regex> = LazyLock::new(|| {
        regex::Regex::new(r"(?i)\b(?:which|choose|select|prefer|would you|do you want|should (?:i|we)|how should)\b|hangisi|hangileri|hangi |neyi |neleri |seç|tercih|ister misin|nasıl (?:ilerle|yap|devam)").unwrap()
    });
    let mut lines: Vec<_> = answer.trim_end().lines().collect();
    if lines
        .iter()
        .any(|l| l.trim_start().starts_with("```") || l.trim_start().starts_with("~~~"))
    {
        return false;
    }
    let after = lines
        .last()
        .copied()
        .filter(|l| !l.starts_with(char::is_whitespace) && l.ends_with('?') && !CHOICE.is_match(l));
    if after.is_some() {
        lines.pop();
    }
    let Some(start) = lines
        .iter()
        .rposition(|l| CHOICE.captures(l).is_some_and(|c| &c[1] == "1"))
    else {
        return false;
    };
    let before = lines[..start]
        .iter()
        .rev()
        .map(|l| l.trim())
        .find(|l| !l.is_empty());
    if !after
        .or(before)
        .is_some_and(|q| q.ends_with('?') && ASKS.is_match(q))
    {
        return false;
    }
    let mut count = 0;
    let mut gap = false;
    for line in &lines[start..] {
        if let Some(choice) = CHOICE.captures(line) {
            count += 1;
            if choice[1].parse::<usize>().ok() != Some(count) || choice[2].trim().is_empty() {
                return false;
            }
            gap = false;
        } else if line.trim().is_empty() {
            gap = true;
        } else if gap || !line.starts_with([' ', '\t']) {
            return false;
        }
    }
    count >= 2
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct NoticeTurn {
    pub position: u64,
    pub started_ms: Option<i64>,
    pub state: &'static str,
}

impl Turn {
    fn notice(&self) -> Option<NoticeTurn> {
        if self.prompt.is_empty() {
            return None;
        }
        Some(NoticeTurn {
            position: self.position,
            started_ms: self.started_ms,
            state: match self.state {
                "completed" if reply_needs_choice(&self.answer) => "question",
                "" => "working",
                state => state,
            },
        })
    }
}

#[derive(Default)]
struct NoticeCursor {
    offset: u64,
    skip_partial: bool,
    turn: Turn,
    previous: Option<NoticeTurn>,
}

impl NoticeCursor {
    fn latest(&self) -> Option<NoticeTurn> {
        self.turn.notice().or(self.previous)
    }

    // Only complete rows advance the cursor. Oversize records are discarded as
    // a stream, including when the writer finishes them in a later append.
    fn advance(&mut self, mut reader: impl BufRead, provider: &str) -> std::io::Result<()> {
        let mut row = Vec::new();
        let mut shifted = VecDeque::new();
        loop {
            if self.skip_partial {
                let available = reader.fill_buf()?;
                if available.is_empty() {
                    break;
                }
                let newline = available.iter().position(|&b| b == b'\n');
                let count = newline.map_or(available.len(), |i| i + 1);
                reader.consume(count);
                self.offset += count as u64;
                self.skip_partial = newline.is_none();
                continue;
            }
            row.clear();
            let count = reader
                .by_ref()
                .take(MAX_LOG_LINE as u64 + 1)
                .read_until(b'\n', &mut row)?;
            if count > MAX_LOG_LINE {
                self.offset += count as u64;
                self.skip_partial = !row.ends_with(b"\n");
                continue;
            }
            if !row.ends_with(b"\n") {
                break;
            }
            apply_log_line(&row, self.offset, provider, &mut shifted, &mut self.turn);
            self.offset += count as u64;
            if let Some(turn) = shifted.pop_back() {
                self.previous = turn.notice();
            }
            // Notification state needs the current prompt/answer and outcomes,
            // not the conversation's historical tool calls or step descriptions.
            self.turn.steps.clear();
            if self.turn.calls.len() > MAX_STEPS {
                self.turn.calls.clear();
            }
        }
        Ok(())
    }
}

#[derive(Default)]
pub(crate) struct NoticeLog {
    path: Option<PathBuf>,
    retry: Option<Instant>,
    revision: Option<(u64, SystemTime)>,
    cursor: NoticeCursor,
    latest: Option<NoticeTurn>,
}

impl NoticeLog {
    pub(crate) fn has_revision(&self) -> bool {
        self.revision.is_some()
    }
    pub(crate) fn latest(
        &mut self,
        core: &Core,
        session: &Value,
        thread: &str,
    ) -> Option<NoticeTurn> {
        if self.path.is_none() {
            if self.retry.is_some_and(|until| Instant::now() < until) {
                return None;
            }
            self.retry = Some(Instant::now() + Duration::from_secs(5));
            self.path = history::bound_log(core, session, thread).ok().flatten();
        }
        let path = self.path.as_ref()?;
        let Ok(stat) = path.metadata() else {
            self.path = None;
            self.revision = None;
            self.cursor = NoticeCursor::default();
            return None;
        };
        let revision = (stat.len(), stat.modified().ok()?);
        if self.revision == Some(revision) {
            return self.latest;
        }
        if !history::matches_bound_log(path, session, thread) {
            self.path = None;
            self.revision = None;
            self.cursor = NoticeCursor::default();
            return None;
        }
        let provider = session["provider"].as_str()?;
        let reset = !self.revision.is_some_and(|(len, _)| len < stat.len())
            || stat.len() < self.cursor.offset;
        let start = stat.len().saturating_sub(MAX_LOG_BYTES);
        if reset {
            self.cursor = NoticeCursor {
                offset: start,
                skip_partial: start != 0,
                ..NoticeCursor::default()
            };
        }
        let mut file = File::open(path).ok()?;
        file.seek(SeekFrom::Start(self.cursor.offset)).ok()?;
        let remaining = stat.len() - self.cursor.offset;
        self.cursor
            .advance(BufReader::new((&mut file).take(remaining)), provider)
            .ok()?;
        // A single long-running turn can occupy the whole tail window on the
        // first observation. Recover its user row once, then follow appends.
        if reset && start != 0 && self.cursor.latest().is_none() {
            file.rewind().ok()?;
            self.cursor = NoticeCursor::default();
            self.cursor
                .advance(BufReader::new(file.take(stat.len())), provider)
                .ok()?;
        }
        self.revision = Some(revision);
        self.latest = self.cursor.latest();
        self.latest
    }
}

fn read(core: &Core, session: &Value, args: &Value) -> Result<Value> {
    let terminal = terminal::chat_state(core, session, args["terminalOutput"] == true);
    let thread = session["conversationId"].as_str();
    let mut result = json!({
        "conversationId":thread,"found":false,"turns":[],"ticket":terminal.ticket,
        "mismatch":terminal.mismatch,"liveApprovalMode":terminal.approval_mode,
        "terminalPrompt":terminal.prompt,"terminalOutput":terminal.output,"terminalRunId":terminal.run_id,
    });
    if terminal.mismatch {
        return Ok(result);
    }
    let Some(thread) = thread else {
        return Ok(result);
    };
    if read_turns(core, session, thread, &args["revision"], &mut result).is_err() {
        result["turns"] = Value::Null;
        result["logError"] = json!(true);
    }
    Ok(result)
}

fn read_turns(
    core: &Core,
    session: &Value,
    thread: &str,
    previous: &Value,
    result: &mut Value,
) -> Result<()> {
    let Some(path) = locate_log(core, session, thread)? else {
        return Ok(());
    };
    let mut file = File::open(path)?;
    let stat = file.metadata()?;
    let modified = stat
        .modified()
        .ok()
        .and_then(|time| time.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map_or(0, |time| time.as_nanos());
    let revision = format!("{thread}:{}:{modified}", stat.len());
    result["revision"] = json!(revision);
    result["found"] = json!(true);
    result["turns"] = Value::Null;
    if previous.as_str() != Some(&revision) {
        let start = stat.len().saturating_sub(MAX_LOG_BYTES);
        file.seek(SeekFrom::Start(start))?;
        let mut bytes = Vec::new();
        file.take(stat.len() - start).read_to_end(&mut bytes)?;
        result["turns"] = json!(parse_log(&bytes, start, string(session, "provider")?));
    }
    Ok(())
}

pub(crate) fn dispatch(core: &Core, action: &str, args: &Value) -> Result<Value> {
    let session = core.session(string(args, "id")?)?;
    if !matches!(
        session["provider"].as_str(),
        Some("codex" | "claude" | "copilot")
    ) || session["login"] == true
        || session["archived"] == true
    {
        bail!("Sohbet bu oturumda kullanılamıyor.");
    }
    match action {
        "read" => read(core, &session, args),
        "send" => terminal::send_chat(core, args),
        "answer_prompt" => terminal::answer_chat_prompt(core, args),
        "image_reference" => file_reference(&session, string(args, "path")?, true),
        "file_reference" => file_reference(&session, string(args, "path")?, false),
        #[cfg(windows)]
        "paste_screenshot" => {
            use base64::{engine::general_purpose::STANDARD, Engine};
            let payload = string(args, "png")?;
            if payload.len() > MAX_SCREENSHOT_BYTES.div_ceil(3) * 4 {
                bail!("Ekran görüntüsü çok büyük.");
            }
            save_screenshot(core, string(&session, "id")?, &STANDARD.decode(payload)?)
        }
        #[cfg(windows)]
        "discard_screenshot" => {
            let directory = screenshot_directory(core, string(&session, "id")?)?;
            let attachment = uuid::Uuid::parse_str(string(args, "attachmentId")?)?;
            let path = directory.join(format!("{attachment}.png"));
            match std::fs::remove_file(path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
            Ok(json!(true))
        }
        "approval" => {
            let mode = string(args, "mode")?;
            let provider = string(&session, "provider")?;
            match (provider, mode) {
                ("codex", "default" | "manual" | "auto")
                | ("claude", "default" | "manual" | "auto" | "edits" | "plan") => {}
                ("copilot", "manual" | "auto") => {
                    terminal::apply_copilot_approval(core, args, mode)?
                }
                _ => bail!("Onay seviyesi geçersiz."),
            }
            core.update_session(string(&session, "id")?, json!({"approvalMode":mode}))
        }
        _ => bail!("Sohbet işlemi geçersiz"),
    }
}

#[cfg(windows)]
pub(crate) fn valid_screen_dimensions(width: u32, height: u32) -> bool {
    width > 0 && height > 0 && u64::from(width) * u64::from(height) <= 40_000_000
}

#[cfg(windows)]
pub(crate) fn validate_png(bytes: &[u8]) -> Result<()> {
    if bytes.len() > MAX_SCREENSHOT_BYTES {
        bail!("Ekran görüntüsü çok büyük.");
    }
    let reader = png::Decoder::new(std::io::Cursor::new(bytes)).read_info()?;
    let info = reader.info();
    if !valid_screen_dimensions(info.width, info.height) {
        bail!("Ekran görüntüsü çok büyük.");
    }
    Ok(())
}

#[cfg(windows)]
fn screenshot_directory(core: &Core, session_id: &str) -> Result<PathBuf> {
    let id = uuid::Uuid::parse_str(session_id)?;
    Ok(core.directory.join("attachments").join(id.to_string()))
}

#[cfg(windows)]
pub(crate) fn save_screenshot(core: &Core, session_id: &str, bytes: &[u8]) -> Result<Value> {
    let session = core.session(session_id)?;
    if session["login"] == true || session["archived"] == true {
        bail!("Bu oturuma dosya eklenemiyor.");
    }
    validate_png(bytes)?;
    let attachment = uuid::Uuid::new_v4();
    let directory = screenshot_directory(core, session_id)?;
    std::fs::create_dir_all(&directory)?;
    let path = directory.join(format!("{attachment}.png"));
    crate::core::atomic(&path, bytes)?;
    let mut reference = file_reference(&session, &path.to_string_lossy(), true)?;
    reference["attachmentId"] = json!(attachment);
    Ok(reference)
}

fn file_reference(session: &Value, path: &str, image_only: bool) -> Result<Value> {
    let path = Path::new(path);
    let is_image = |p: &Path| {
        p.extension()
            .and_then(|ext| ext.to_str())
            .is_some_and(|ext| {
                ["png", "jpg", "jpeg", "webp", "gif"].contains(&ext.to_ascii_lowercase().as_str())
            })
    };
    if image_only && (!path.is_absolute() || !is_image(path)) {
        bail!("PNG, JPEG, WebP veya GIF görseli seç.");
    }
    if !path.is_absolute() {
        bail!("Yerel bir dosya seç.");
    }
    let file = path.canonicalize()?;
    if !file.is_file() || image_only && !is_image(&file) {
        bail!("Dosya bulunamadı veya desteklenmiyor.");
    }
    let image = is_image(&file);
    let file = external_path(&file);
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("File");
    let absolute = file
        .to_str()
        .filter(|path| !path.chars().any(|c| c.is_control() || c == '"'));
    let Some(absolute) = absolute else {
        bail!("Dosya yolu kullanılamıyor.");
    };
    let reference = if session["provider"] == "copilot" {
        format!("@\"{}\"", absolute.replace('\\', "/"))
    } else {
        format!("\"{absolute}\"")
    };
    Ok(json!({"name":name,"reference":reference,"kind":if image { "image" } else { "file" }}))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notifications_retain_large_turns_and_wait_for_a_complete_record() {
        let mut cursor = NoticeCursor::default();
        let user = b"{\"type\":\"user.message\",\"data\":{\"content\":\"Choose the files\"}}\n";
        cursor.advance(&user[..], "copilot").unwrap();
        let mut tool_output = vec![b'x'; MAX_LOG_BYTES as usize + 3];
        cursor.advance(&tool_output[..], "copilot").unwrap();
        assert!(cursor.skip_partial);
        tool_output.clear();
        let tail = b"\n{\"type\":\"assistant.message\",\"data\":{\"content\":\"Ready\"}}\n";
        cursor.advance(&tail[..], "copilot").unwrap();
        let finish = b"{\"type\":\"assistant.turn_end\"}\n";
        let before = cursor.offset;
        cursor
            .advance(&finish[..finish.len() - 1], "copilot")
            .unwrap();
        assert_eq!(cursor.offset, before);
        assert_eq!(cursor.latest().unwrap().state, "working");
        cursor.advance(&finish[..], "copilot").unwrap();
        let done = cursor.latest().unwrap();
        assert_eq!(done.position, 0);
        assert_eq!(done.state, "completed");
    }

    #[test]
    fn notifications_do_not_turn_cancellation_errors_or_truncation_into_success() {
        let parse = |provider, rows: Vec<Value>| {
            let log = rows
                .into_iter()
                .map(|row| row.to_string() + "\n")
                .collect::<String>();
            parse_log(log.as_bytes(), 0, provider)
                .pop_back()
                .unwrap()
                .state
        };
        for stop in ["session.error", "assistant.turn_cancelled"] {
            assert_eq!(
                parse(
                    "copilot",
                    vec![
                        json!({"type":"user.message","data":{"content":"Check the sample"}}),
                        json!({"type":"assistant.message","data":{"content":"Partial output"}}),
                        json!({"type":stop}),
                        json!({"type":"assistant.turn_end"}),
                        json!({"type":"session.idle"}),
                    ]
                ),
                "interrupted"
            );
        }
        for error in [
            json!({"type":"abort"}),
            json!({"type":"assistant","message":{"content":"Partial output","stop_reason":"max_tokens"}}),
            json!({"type":"assistant","isApiErrorMessage":true,"message":{"content":"Failed","stop_reason":"end_turn"}}),
        ] {
            assert_eq!(
                parse(
                    "claude",
                    vec![
                        json!({"type":"user","message":{"content":"Check the sample"}}),
                        json!({"type":"assistant","message":{"content":"Partial output"}}),
                        error,
                        json!({"type":"system","subtype":"turn_duration"}),
                    ]
                ),
                "interrupted"
            );
        }
        assert_eq!(
            parse(
                "copilot",
                vec![
                    json!({"type":"user.message","data":{"content":"Check"}}),
                    json!({"type":"session.idle"})
                ]
            ),
            "working"
        );
        assert_eq!(
            parse(
                "claude",
                vec![
                    json!({"type":"user","message":{"content":"Check"}}),
                    json!({"type":"system","subtype":"turn_duration"})
                ]
            ),
            "working"
        );
    }

    #[test]
    fn notifications_only_recognize_explicit_reply_choices() {
        assert!(reply_needs_choice(
            "Which one do you prefer?\n1. First\n2. Second"
        ));
        assert!(reply_needs_choice(
            "1) İlk\n   Birinci seçenek\n2) İkinci\n\nHangisi?"
        ));
        for ordinary in [
            "Done?",
            "Steps\n1. Run\n2. Check",
            "Which one?\n1. First\n3. Last",
            "Which one?\n```\n1. First\n2. Second\n```",
        ] {
            assert!(!reply_needs_choice(ordinary));
        }
    }

    #[test]
    fn codex_chat_uses_explicit_user_events_and_keeps_steps_separate() {
        let entries = [
            json!({"type":"event_msg","payload":{"type":"task_complete","last_agent_message":"an unrelated earlier answer"}}),
            json!({"type":"event_msg","payload":{"type":"task_started","turn_id":"turn"}}),
            json!({"type":"response_item","payload":{"type":"message","role":"user","content":[{"text":"injected context"}]}}),
            json!({"type":"event_msg","payload":{"type":"user_message","message":"Run the checks"}}),
            json!({"type":"response_item","payload":{"type":"reasoning","text":"private reasoning"}}),
            json!({"type":"response_item","payload":{"type":"message","role":"assistant","phase":"commentary","content":[{"text":"Checking now"}]}}),
            json!({"type":"event_msg","payload":{"type":"agent_message","phase":"commentary","message":"Checking now"}}),
            json!({"type":"response_item","payload":{"type":"function_call","name":"exec_command","call_id":"call"}}),
            json!({"type":"response_item","payload":{"type":"function_call_output","output":"raw output","call_id":"call"}}),
            json!({"type":"response_item","payload":{"type":"message","role":"assistant","phase":"final_answer","content":[{"text":"All tests passed"}]}}),
            json!({"type":"event_msg","payload":{"type":"task_complete","last_agent_message":"All tests passed"}}),
        ];
        let mut data = entries
            .into_iter()
            .map(|entry| entry.to_string() + "\n")
            .collect::<String>();
        data.push_str(
            r#"{"type":"event_msg","payload":{"type":"user_message","message":"partial line"}}"#,
        );
        let turns = parse_log(data.as_bytes(), 0, "codex");
        assert_eq!(turns.len(), 1);
        let turn = &turns[0];
        assert_eq!(turn.prompt, "Run the checks");
        assert_eq!(turn.steps.len(), 2);
        assert_eq!(turn.steps[0].text, "Checking now");
        assert_eq!(turn.steps[1].kind, "command");
        assert_eq!(turn.answer, "All tests passed");
        assert_eq!(turn.state, "completed");
    }
}
