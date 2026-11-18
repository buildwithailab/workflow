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
    io::{Read, Seek, SeekFrom},
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
    if matches!(kind, "assistant.turn_end" | "session.idle")
        || kind == "system" && entry["subtype"] == "turn_duration"
        || is_claude
            && matches!(
                message["stop_reason"].as_str(),
                Some("end_turn" | "stop_sequence" | "max_tokens")
            )
    {
        turn.finish("completed", None);
    } else if matches!(kind, "session.error" | "assistant.turn_cancelled" | "abort") {
        turn.finish("interrupted", None);
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
        if line.len() > 2 * 1024 * 1024 {
            continue;
        }
        let Ok(entry) = serde_json::from_slice::<Value>(line) else {
            continue;
        };
        let item = &entry["payload"];
        if entry["isSidechain"] == true || !entry["agentId"].is_null() || entry["isMeta"] == true {
            continue;
        }
        if provider != "codex" {
            apply_provider_event(provider, &entry, &mut turns, &mut turn, line_position);
            continue;
        }
        match (entry["type"].as_str(), item["type"].as_str()) {
            (Some("event_msg"), Some("task_started" | "turn_started")) => {
                if !turn.prompt.is_empty()
                    && (turn.state == "completed"
                        || turn.state == "interrupted"
                        || !turn.id.is_empty()
                            && item["turn_id"].as_str().is_some_and(|id| id != turn.id))
                {
                    append_turn(&mut turns, &mut turn);
                }
                if let Some(id) = item["turn_id"].as_str() {
                    turn.id = id.to_owned();
                }
            }
            (Some("event_msg"), Some("user_message")) => {
                if let Some(text) = item["message"].as_str() {
                    if turn.prompt_from_item && turn.prompt == clipped(text) {
                        turn.prompt_from_item = false;
                        continue;
                    }
                    if !turn.prompt.is_empty() {
                        append_turn(&mut turns, &mut turn);
                    }
                    turn.prompt = clipped(text);
                    turn.position = line_position;
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
                            && metadata["content_item_metadata"][*index]["provenance"]["type"]
                                == "user"
                    })
                    .filter_map(|(_, part)| part["text"].as_str())
                    .collect::<Vec<_>>()
                    .join("\n");
                if !text.trim().is_empty() {
                    turn.prompt = clipped(&text);
                    turn.prompt_from_item = true;
                    turn.position = line_position;
                }
            }
            (Some("event_msg"), Some("task_complete" | "turn_complete"))
                if !turn.prompt.is_empty() =>
            {
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
    append_turn(&mut turns, &mut turn);
    turns
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
