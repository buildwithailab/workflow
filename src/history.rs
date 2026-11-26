use crate::{accounts::Profile, core::*};
use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::{
    fs,
    io::{BufRead, BufReader, Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    sync::Arc,
};

const METADATA_BYTES: u64 = 256 * 1024;
const PAGE_MESSAGES: usize = 50;
const PAGE_TEXT_BYTES: usize = 256 * 1024;
const PAGE_SCAN_BYTES: u64 = 2 * 1024 * 1024;
const PAGE_BLOCK_BYTES: usize = 64 * 1024;
const MAX_RECORD_BYTES: usize = 1024 * 1024;

fn logs(provider: &str, profile: &Profile) -> Vec<PathBuf> {
    let root = profile.config_dir().join(match provider {
        "codex" => "sessions",
        "claude" => "projects",
        _ => "session-state",
    });
    if !root.exists() {
        return vec![];
    }
    let mut out = vec![];
    for e in ignore::WalkBuilder::new(root)
        .hidden(false)
        .git_ignore(false)
        .max_depth(Some(6))
        .build()
        .flatten()
    {
        if e.file_type().is_some_and(|t| t.is_file())
            && e.path().extension().is_some_and(|x| x == "jsonl")
        {
            out.push(e.path().to_path_buf());
            if out.len() >= 15000 {
                break;
            }
        }
    }
    out
}
fn metadata(file: &Path, provider: &str) -> Option<Value> {
    let f = fs::File::open(file).ok()?;
    read_metadata(&f, file, provider)
}

fn read_metadata(f: &fs::File, file: &Path, provider: &str) -> Option<Value> {
    let length = f.metadata().ok()?.len();
    let mut reader = BufReader::new(f.take(METADATA_BYTES));
    let mut bytes = Vec::new();
    let mut consumed = 0;
    for _ in 0..12 {
        bytes.clear();
        let count = reader.read_until(b'\n', &mut bytes).ok()?;
        consumed += count as u64;
        if count == 0 || (!bytes.ends_with(b"\n") && consumed < length) {
            break;
        }
        let Ok(v) = serde_json::from_slice::<Value>(&bytes) else {
            continue;
        };
        if provider == "codex" && v["type"] == "session_meta" {
            return Some(
                json!({"id":v["payload"]["id"],"cwd":v["payload"]["cwd"],"createdAt":v["payload"]["timestamp"],"file":file}),
            );
        }
        if provider == "copilot" && v["type"] == "session.start" {
            let cwd = v["data"]["context"]["cwd"]
                .as_str()
                .or_else(|| v["data"]["cwd"].as_str())?;
            let thread = v["data"]["sessionId"]
                .as_str()
                .map(str::to_string)
                .or_else(|| file.parent()?.file_name()?.to_str().map(str::to_string))?;
            return Some(json!({"id":thread,"cwd":cwd,"createdAt":v["timestamp"],"file":file}));
        }
        if provider == "claude" && v["isSidechain"] != true {
            let (Some(thread), Some(cwd)) = (v["sessionId"].as_str(), v["cwd"].as_str()) else {
                continue;
            };
            return Some(json!({"id":thread,"cwd":cwd,"createdAt":v["timestamp"],"file":file}));
        }
    }
    None
}
fn list(core: &Core, session: &Value) -> Result<Vec<Value>> {
    let provider = string(session, "provider")?;
    let directory = string(session, "directory")?;
    let canonical = Path::new(directory).canonicalize().ok();
    let profile = Profile::for_history(core, provider, &session["accountId"])?;
    let mut out = logs(provider, &profile)
        .iter()
        .filter_map(|p| metadata(p, provider))
        .filter(|m| {
            m["cwd"]
                .as_str()
                .is_some_and(|cwd| matches_session_directory(cwd, canonical.as_deref(), session))
        })
        .collect::<Vec<_>>();
    out.sort_by(|a, b| b["createdAt"].as_str().cmp(&a["createdAt"].as_str()));
    out.truncate(300);
    Ok(out)
}

fn matches_directory(cwd: &str, current: Option<&Path>, previous: Option<&str>) -> bool {
    let cwd = external_path(Path::new(cwd));
    if previous.is_some_and(|old| {
        let old = external_path(Path::new(old));
        #[cfg(windows)]
        return cwd
            .to_string_lossy()
            .replace('/', "\\")
            .eq_ignore_ascii_case(&old.to_string_lossy().replace('/', "\\"));
        #[cfg(not(windows))]
        return cwd == old;
    }) {
        return true;
    }
    current.is_some_and(|directory| cwd.canonicalize().is_ok_and(|path| path == directory))
}

fn matches_session_directory(cwd: &str, current: Option<&Path>, session: &Value) -> bool {
    matches_directory(cwd, current, session["previousDirectory"].as_str())
        || session["archivedFromDirectory"]
            .as_str()
            .is_some_and(|directory| matches_directory(cwd, None, Some(directory)))
}

pub(crate) fn bound_log(core: &Core, session: &Value, thread: &str) -> Result<Option<PathBuf>> {
    let provider = string(session, "provider")?;
    let profile = Profile::for_history(core, provider, &session["accountId"])?;
    Ok(find_bound_log(
        &logs(provider, &profile),
        session,
        provider,
        thread,
    ))
}

fn find_bound_log(
    paths: &[PathBuf],
    session: &Value,
    provider: &str,
    thread: &str,
) -> Option<PathBuf> {
    let suffix = format!("-{thread}");
    let named = |path: &Path| {
        path.file_stem()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name == thread || name.ends_with(&suffix))
            || provider == "copilot"
                && path
                    .parent()
                    .and_then(Path::file_name)
                    .is_some_and(|name| name == thread)
    };
    paths
        .iter()
        .filter(|path| named(path))
        .chain(paths.iter().filter(|path| !named(path)))
        .find(|path| matches_bound_log(path, session, thread))
        .cloned()
}

pub(crate) fn matches_bound_log(path: &Path, session: &Value, thread: &str) -> bool {
    let provider = session["provider"].as_str().unwrap_or_default();
    metadata(path, provider).is_some_and(|entry| matches_binding(&entry, session, thread))
}

pub(crate) fn matches_open_bound_log(file:&fs::File,path:&Path,session:&Value,thread:&str)->bool {
    let mut handle=file;
    let Ok(position)=handle.stream_position() else {return false;};
    if handle.seek(SeekFrom::Start(0)).is_err() {return false;}
    let matched=read_metadata(file,path,session["provider"].as_str().unwrap_or_default())
        .is_some_and(|entry|matches_binding(&entry,session,thread));
    handle.seek(SeekFrom::Start(position)).is_ok() && matched
}

fn matches_binding(entry: &Value, session: &Value, thread: &str) -> bool {
    entry["id"] == thread
        && entry["cwd"].as_str().is_some_and(|cwd| {
            let current = session["directory"]
                .as_str()
                .and_then(|dir| Path::new(dir).canonicalize().ok());
            matches_session_directory(cwd, current.as_deref(), session)
        })
}

pub fn discover(core: &Core, id: &str) -> Result<Option<String>> {
    let s = core.session(id)?;
    // A shared project path cannot identify which client started a conversation.
    if s["workspaceKind"] == "direct" {
        return Ok(None);
    }
    let list = list(core, &s)?;
    let created = chrono::DateTime::parse_from_rfc3339(string(&s, "createdAt")?)?;
    let candidates = list
        .iter()
        .filter(|m| {
            m["createdAt"]
                .as_str()
                .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                .is_some_and(|time| time >= created - chrono::Duration::seconds(2))
        })
        .collect::<Vec<_>>();
    Ok(if candidates.len() == 1 {
        candidates[0]["id"].as_str().map(str::to_string)
    } else {
        None
    })
}
fn message(line: &[u8], provider: &str) -> Option<(&'static str, String)> {
    let v: Value = serde_json::from_slice(line).ok()?;
    if v["isSidechain"] == true || !v["agentId"].is_null() {
        return None;
    }
    let item = if v["type"] == "response_item" {
        &v["payload"]
    } else if v["message"].is_object() {
        &v["message"]
    } else if provider == "copilot" {
        &v["data"]
    } else {
        &v
    };
    let role = match (v["type"].as_str(), item["role"].as_str()) {
        (Some("user.message"), _) => "user",
        (Some("assistant.message"), _) => "assistant",
        (_, Some("user")) => "user",
        (_, Some("assistant")) => "assistant",
        _ => return None,
    };
    let mut text = String::new();
    if let Some(content) = item["content"].as_str() {
        text.push_str(content);
    } else if let Some(parts) = item["content"].as_array() {
        for part in parts {
            if let Some(content) = part["text"].as_str() {
                text.push_str(content);
                text.push('\n');
            }
        }
    }
    (!text.trim().is_empty()).then_some((role, text))
}

#[derive(Default)]
struct MessagePage {
    messages: Vec<Value>,
    next_before_byte: Option<u64>,
    text_bytes: usize,
    truncated_messages: usize,
    skipped_oversize: usize,
}

impl MessagePage {
    // False means this complete record belongs on the next page.
    fn include(&mut self, line: &[u8], provider: &str) -> bool {
        let line = line.strip_suffix(b"\n").unwrap_or(line);
        if line.len() > MAX_RECORD_BYTES {
            self.skipped_oversize += 1;
            return true;
        }
        let Some((role, mut text)) = message(line, provider) else {
            return true;
        };
        if !self.messages.is_empty() && self.text_bytes + text.len() > PAGE_TEXT_BYTES {
            return false;
        }
        if text.len() > PAGE_TEXT_BYTES {
            let mut end = PAGE_TEXT_BYTES;
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            text.truncate(end);
            self.truncated_messages += 1;
        }
        self.text_bytes += text.len();
        self.messages.push(json!({"role":role,"text":text}));
        true
    }

    fn finish(mut self, before: u64) -> Self {
        self.next_before_byte = (before > 0).then_some(before);
        self.messages.reverse();
        self
    }
}

fn read_page(
    file: &mut (impl Read + Seek),
    length: u64,
    before: Option<u64>,
    provider: &str,
) -> Result<MessagePage> {
    let end = before.unwrap_or(length).min(length);
    let start = end.saturating_sub(PAGE_SCAN_BYTES);
    let mut bytes = vec![0; (end - start) as usize];
    let mut loaded = bytes.len();
    let mut row_end = loaded;
    let mut partial_end = false;
    let mut page = MessagePage::default();
    while loaded > 0 {
        let previous = loaded;
        loaded = loaded.saturating_sub(PAGE_BLOCK_BYTES);
        file.seek(SeekFrom::Start(start + loaded as u64))?;
        file.read_exact(&mut bytes[loaded..previous])?;
        if previous == bytes.len() {
            // A long row can span several scan windows. Never parse its prefix
            // as a standalone message, even when it resembles complete JSON.
            partial_end = end < length && !bytes.ends_with(b"\n");
        }
        for i in (loaded..previous).rev() {
            if bytes[i] != b'\n' || i + 1 == row_end {
                continue;
            }
            let row_start = i + 1;
            if !partial_end && !page.include(&bytes[row_start..row_end], provider) {
                return Ok(page.finish(start + row_end as u64));
            }
            partial_end = false;
            row_end = row_start;
            if page.messages.len() == PAGE_MESSAGES || page.text_bytes == PAGE_TEXT_BYTES {
                return Ok(page.finish(start + row_end as u64));
            }
        }
    }
    if start == 0 {
        if !partial_end && !page.include(&bytes[..row_end], provider) {
            return Ok(page.finish(row_end as u64));
        }
        return Ok(page.finish(0));
    }
    if partial_end || row_end > MAX_RECORD_BYTES + 1 {
        if !partial_end {
            page.skipped_oversize += 1;
        }
        return Ok(page.finish(start));
    }
    // Retry the boundary row on the next page with its preceding bytes available.
    Ok(page.finish(start + row_end as u64))
}

pub fn messages(core: &Core, sid: &str) -> Result<Value> {
    messages_page(core, sid, None)
}

pub(crate) fn messages_page(core: &Core, sid: &str, before: Option<u64>) -> Result<Value> {
    let session = core.session(sid)?;
    let provider = string(&session, "provider")?;
    let conversation = session["conversationId"].as_str();
    let mut page = MessagePage::default();
    let mut found = false;
    if let Some(thread) = conversation {
        if let Some(path) = crate::chat::locate_log(core, &session, thread)? {
            let mut file = fs::File::open(&path)?;
            if read_metadata(&file, &path, provider)
                .is_some_and(|entry| matches_binding(&entry, &session, thread))
            {
                let length = file.metadata()?.len();
                page = read_page(&mut file, length, before, provider)?;
                found = true;
            }
        }
    }
    Ok(
        json!({"provider":provider,"conversationId":conversation,"messages":page.messages,
        "nativeAvailable":found,"nextBeforeByte":page.next_before_byte,
        "truncatedMessages":page.truncated_messages,"skippedOversize":page.skipped_oversize}),
    )
}
pub fn dispatch(core: &Arc<Core>, action: &str, args: &Value) -> Result<Value> {
    match action {
        "list" => {
            let s = core.session(string(args, "id")?)?;
            Ok(json!(list(core, &s)?))
        }
        "messages" => {
            let before = if args["beforeByte"].is_null() {
                None
            } else {
                Some(
                    args["beforeByte"]
                        .as_u64()
                        .context("Geçersiz geçmiş sayfası")?,
                )
            };
            messages_page(core, string(args, "id")?, before)
        }
        "bind" => {
            let _guard = core.operations.lock().unwrap();
            let sid = string(args, "id")?;
            let s = core.session(sid)?;
            let thread = string(args, "conversationId")?;
            if s["conversationId"]
                .as_str()
                .is_some_and(|current| current != thread)
                && core.terms.lock().unwrap().get(sid).is_some_and(|terminal| {
                    !terminal.exited.load(std::sync::atomic::Ordering::SeqCst)
                })
            {
                anyhow::bail!("Konuşmayı değiştirmek için önce oturumu durdur")
            }
            let list = list(core, &s)?;
            list.iter()
                .find(|v| v["id"] == thread)
                .context("Konuşma bu çalışma alanında bulunamadı")?;
            core.update_session(sid, json!({"conversationId":thread}))
        }
        "discover" => {
            let _guard = core.operations.lock().unwrap();
            let sid = string(args, "id")?;
            if !core.session(sid)?["conversationId"].is_null() {
                return Ok(json!({"found":false}));
            }
            if let Some(t) = discover(core, sid)? {
                core.update_session(sid, json!({"conversationId":t}))
            } else {
                Ok(json!({"found":false}))
            }
        }
        _ => anyhow::bail!("Geçmiş işlemi geçersiz"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Cursor, Write};

    fn row(provider: &str, text: &str) -> Vec<u8> {
        let entry = match provider {
            "codex" => {
                json!({"type":"response_item","payload":{"role":"user","content":[{"text":text}]}})
            }
            "claude" => json!({"message":{"role":"assistant","content":text}}),
            _ => json!({"type":"user.message","data":{"content":text}}),
        };
        let mut bytes = serde_json::to_vec(&entry).unwrap();
        bytes.extend_from_slice(b"\r\n");
        bytes
    }

    #[test]
    fn all_providers_page_back_chronologically_without_losing_or_repeating_messages() {
        for provider in ["codex", "claude", "copilot"] {
            let mut bytes = Vec::new();
            for number in 0..113 {
                bytes.extend(row(provider, &number.to_string()));
                bytes.extend_from_slice(b"{\"type\":\"tool\"}\n");
            }
            bytes.extend(row(provider, "113"));
            bytes.truncate(bytes.len() - 2);
            let mut file = Cursor::new(&bytes);
            let mut before = None;
            let mut count = 114;
            for expected in [50, 50, 14] {
                let page = read_page(&mut file, bytes.len() as u64, before, provider).unwrap();
                assert_eq!(page.messages.len(), expected);
                count -= expected;
                for (index, msg) in page.messages.iter().enumerate() {
                    assert_eq!(
                        msg["text"].as_str().unwrap().trim(),
                        (count + index).to_string()
                    );
                }
                if let Some(older) = page.next_before_byte {
                    assert!(older < before.unwrap_or(bytes.len() as u64));
                }
                before = page.next_before_byte;
            }
            assert!(before.is_none());
        }
    }

    #[test]
    fn oversized_partial_rows_use_bounded_windows_and_do_not_turn_into_messages() {
        for provider in ["codex", "claude", "copilot"] {
            let mut bytes = row(provider, "oldest");
            // A suffix and prefix that each look like a valid message still belong
            // to one oversize physical record. Fill it with more than two windows.
            bytes.extend(row(provider, "partial prefix").split_last().unwrap().1);
            bytes.extend(std::iter::repeat_n(b' ', PAGE_SCAN_BYTES as usize * 3));
            bytes.extend(row(provider, "partial suffix"));
            bytes.extend(row(provider, "newest"));
            let mut file = Cursor::new(&bytes);
            let mut before = None;
            let mut seen = Vec::new();
            let mut skipped = 0;
            let mut empty_pages = 0;
            loop {
                let page = read_page(&mut file, bytes.len() as u64, before, provider).unwrap();
                empty_pages += usize::from(page.messages.is_empty());
                skipped += page.skipped_oversize;
                seen.extend(
                    page.messages
                        .iter()
                        .map(|m| m["text"].as_str().unwrap().trim().to_owned()),
                );
                let Some(older) = page.next_before_byte else {
                    break;
                };
                assert!(older < before.unwrap_or(bytes.len() as u64));
                before = Some(older);
            }
            assert_eq!(seen, ["newest", "oldest"]);
            assert!(empty_pages > 0);
            assert_eq!(skipped, 1);
        }
    }

    #[test]
    fn message_text_budget_preserves_utf8_and_defers_complete_rows() {
        let mut bytes = row("claude", "older");
        bytes.extend(row("claude", &"€".repeat(PAGE_TEXT_BYTES / 3 + 2)));
        bytes.extend(row("claude", "later"));
        let mut file = Cursor::new(&bytes);
        let length = bytes.len() as u64;
        let first = read_page(&mut file, length, None, "claude").unwrap();
        assert_eq!(first.messages.len(), 1);
        let second = read_page(&mut file, length, first.next_before_byte, "claude").unwrap();
        assert_eq!(second.truncated_messages, 1);
        assert_eq!(second.messages.len(), 1);
        assert_eq!(
            second.messages[0]["text"].as_str().unwrap().len(),
            PAGE_TEXT_BYTES / 3 * 3
        );
        let last = read_page(&mut file, length, second.next_before_byte, "claude").unwrap();
        assert_eq!(last.messages[0]["text"], "older");
        assert!(last.next_before_byte.is_none());
    }

    #[test]
    fn metadata_rejects_an_incomplete_oversized_record_before_parsing() {
        let path = std::env::temp_dir().join(format!("workflow-synthetic-metadata-{}", id()));
        let mut file = fs::File::create(&path).unwrap();
        let header = br#"{"type":"session_meta","payload":{"id":"synthetic","cwd":"fixture"}}"#;
        file.write_all(header).unwrap();
        file.set_len(METADATA_BYTES * 24).unwrap();
        drop(file);
        assert!(metadata(&path, "codex").is_none());
        fs::write(&path, header).unwrap();
        assert_eq!(metadata(&path, "codex").unwrap()["id"], "synthetic");
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn bound_history_resolves_an_older_conversation_outside_the_picker_limit() {
        let root = std::env::temp_dir().join(format!("workflow-history-test-{}", id()));
        fs::create_dir(&root).unwrap();
        let mut paths = (0..301)
            .map(|index| root.join(format!("{index}.jsonl")))
            .collect::<Vec<_>>();
        let path = root.join("old.jsonl");
        paths.push(path.clone());
        for provider in ["codex", "claude", "copilot"] {
            let entry = |thread: &str, timestamp: &str| match provider {
                "codex" => {
                    json!({"type":"session_meta","payload":{"id":thread,"cwd":root,"timestamp":timestamp}})
                }
                "claude" => json!({"sessionId":thread,"cwd":root,"timestamp":timestamp}),
                _ => {
                    json!({"type":"session.start","timestamp":timestamp,"data":{"sessionId":thread,"context":{"cwd":root}}})
                }
            };
            for (i, newer) in paths[..301].iter().enumerate() {
                fs::write(
                    newer,
                    serde_json::to_vec(&entry(&i.to_string(), "2025-01-02")).unwrap(),
                )
                .unwrap();
            }
            fs::write(
                &path,
                serde_json::to_vec(&entry("bound", "2025-01-01")).unwrap(),
            )
            .unwrap();
            let session = json!({"directory":root,"provider":provider});
            assert_eq!(
                find_bound_log(&paths, &session, provider, "bound").as_ref(),
                Some(&path)
            );
            assert!(find_bound_log(&paths, &session, provider, "different").is_none());
        }
        for path in paths {
            fs::remove_file(path).unwrap();
        }
        fs::remove_dir(root).unwrap();
    }

    #[test]
    fn migrated_history_matches_only_the_recorded_previous_workspace() {
        assert!(matches_directory(
            "/old/session",
            None,
            Some("/old/session")
        ));
        assert!(!matches_directory(
            "/other/session",
            None,
            Some("/old/session")
        ));
        assert!(!matches_directory("/missing/session", None, None));
    }
}
