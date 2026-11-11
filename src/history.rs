use crate::{accounts::Profile, core::*};
use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::{
    fs,
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
    sync::Arc,
};
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
    for line in BufReader::new(f).lines().take(12).flatten() {
        let Ok(v) = serde_json::from_str::<Value>(&line) else {
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
        if let Some(cwd) = v["cwd"].as_str() {
            return Some(
                json!({"id":v["sessionId"],"cwd":cwd,"createdAt":v["timestamp"],"file":file}),
            );
        }
    }
    None
}
fn list(core: &Core, session: &Value) -> Result<Vec<Value>> {
    let provider = string(session, "provider")?;
    let directory = string(session, "directory")?;
    let canonical = Path::new(directory).canonicalize().ok();
    let legacy = session["previousDirectory"].as_str();
    let profile = Profile::get(core, provider, &session["accountId"])?;
    let mut out = logs(provider, &profile)
        .iter()
        .filter_map(|p| metadata(p, provider))
        .filter(|m| {
            m["cwd"]
                .as_str()
                .is_some_and(|cwd| matches_directory(cwd, canonical.as_deref(), legacy))
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
pub fn messages(core: &Core, sid: &str) -> Result<Value> {
    let s = core.session(sid)?;
    let provider = string(&s, "provider")?;
    let conversation = s["conversationId"].as_str();
    let list = list(core, &s)?;
    let file = list
        .iter()
        .find(|t| t["id"].as_str() == conversation)
        .and_then(|t| t["file"].as_str());
    let mut messages = vec![];
    if let Some(file) = file {
        let f = fs::File::open(file)?;
        for line in BufReader::new(f).lines().map_while(Result::ok) {
            if line.len() > 2 * 1024 * 1024 {
                continue;
            }
            let Ok(v) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if v["isSidechain"] == true || !v["agentId"].is_null() {
                continue;
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
            let role = if v["type"] == "user.message" {
                "user"
            } else if v["type"] == "assistant.message" {
                "assistant"
            } else {
                item["role"].as_str().unwrap_or("")
            };
            if !["user", "assistant"].contains(&role) {
                continue;
            }
            let mut text = String::new();
            if let Some(s) = item["content"].as_str() {
                text.push_str(s)
            }
            if let Some(parts) = item["content"].as_array() {
                for p in parts {
                    if let Some(s) = p["text"].as_str() {
                        text.push_str(s);
                        text.push('\n');
                    }
                }
            }
            if !text.trim().is_empty() {
                messages.push(json!({"role":role,"text":text}));
            }
        }
    }
    Ok(
        json!({"provider":provider,"conversationId":conversation,"messages":messages,"nativeAvailable":file.is_some()}),
    )
}
pub fn dispatch(core: &Arc<Core>, action: &str, args: &Value) -> Result<Value> {
    match action {
        "list" => {
            let s = core.session(string(args, "id")?)?;
            Ok(json!(list(core, &s)?))
        }
        "messages" => messages(core, string(args, "id")?),
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
