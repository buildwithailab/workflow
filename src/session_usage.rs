//! Read the last provider-reported measurements for one bound conversation.
//! Polling reads local journals and never sends anything to a provider process.
use crate::{accounts::Profile, core::Core, history, terminal};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::VecDeque,
    fs::File,
    io::{Read, Seek, SeekFrom},
    path::{Component, Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant, SystemTime},
};

const CACHE_ENTRIES: usize = 16;
const SEARCH_ENTRIES: usize = 16_384;
const CANDIDATES: usize = 8;
const TAIL_BYTES: u64 = 1024 * 1024;
const RECORD_BYTES: usize = 256 * 1024;
const MAX_COUNT: u64 = (1 << 53) - 1; // IPC numbers must be exact in JavaScript.

#[derive(Default, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct Window {
    used_tokens: Option<u64>,
    limit_tokens: Option<u64>,
    used_percent: Option<f64>,
    source: Option<&'static str>,
    basis: Option<&'static str>,
}

impl Window {
    fn new(used: Option<u64>, limit: Option<u64>, source: &'static str) -> Self {
        let limit = limit.filter(|n| *n > 0);
        Self {
            used_tokens: used,
            limit_tokens: limit,
            used_percent: used.zip(limit).map(|(u, l)| 100.0 * u as f64 / l as f64),
            source: (used.is_some() || limit.is_some()).then_some(source),
            basis: used.map(|_| {
                if source == "assistant.usage" {
                    "last-request-input"
                } else {
                    "context-window"
                }
            }),
        }
    }
}

#[derive(Default, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct Tokens {
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    cached_input_tokens: Option<u64>,
    total_tokens: Option<u64>,
    source: Option<&'static str>,
}

impl Tokens {
    fn new(
        input: Option<u64>,
        output: Option<u64>,
        cached: Option<u64>,
        total: Option<u64>,
        source: &'static str,
    ) -> Self {
        Self {
            input_tokens: input,
            output_tokens: output,
            cached_input_tokens: cached,
            total_tokens: total,
            source: [input, output, cached, total]
                .iter()
                .any(Option::is_some)
                .then_some(source),
        }
    }
}

#[derive(Default, Clone)]
struct Reading {
    context: Window,
    tokens: Tokens,
}

impl Reading {
    fn response(&self, session: &Value, observed: Option<&str>) -> Value {
        let available = self.context.source.is_some() || self.tokens.source.is_some();
        json!({
            "sessionId":session["id"], "provider":session["provider"], "accountId":session["accountId"],
            "runId":session["runId"], "conversationId":session["conversationId"],
            "context":self.context, "tokens":self.tokens, "available":available,
            "source":available.then_some("provider-log"), "observedAt":observed.filter(|_| available)
        })
    }
}

fn count(v: &Value) -> Option<u64> {
    v.as_u64().filter(|v| *v <= MAX_COUNT)
}

fn plus(a: Option<u64>, b: Option<u64>) -> Option<u64> {
    a?.checked_add(b?).filter(|n| *n <= MAX_COUNT)
}

// Each field is independently usable only when every model in the snapshot
// reports it. Cache/reasoning subcounts must never be charged twice.
fn model_totals<'a>(
    rows: impl Iterator<Item = &'a Value>,
    claude: bool,
    source: &'static str,
) -> Tokens {
    let mut count_rows = 0;
    let (mut input, mut output, mut cached) = (Some(0), Some(0), Some(0));
    for row in rows {
        count_rows += 1;
        let read = count(
            &row[if claude {
                "cacheReadInputTokens"
            } else {
                "cacheReadTokens"
            }],
        );
        let fresh = count(&row["inputTokens"]);
        let all_input = if claude {
            plus(plus(fresh, read), count(&row["cacheCreationInputTokens"]))
        } else {
            fresh
        };
        input = plus(input, all_input);
        output = plus(output, count(&row["outputTokens"]));
        cached = plus(cached, read);
    }
    if count_rows == 0 {
        return Tokens::default();
    }
    Tokens::new(input, output, cached, plus(input, output), source)
}

#[derive(Deserialize)]
struct Header<'a> {
    #[serde(rename = "type")]
    kind: &'a str,
    #[serde(rename = "sessionId")]
    thread: Option<&'a str>,
    #[serde(rename = "agentId")]
    agent: Option<&'a str>,
    #[serde(rename = "isSidechain")]
    sidechain: Option<bool>,
    #[serde(rename = "isApiErrorMessage")]
    error: Option<bool>,
    subtype: Option<&'a str>,
}

#[derive(Deserialize)]
struct ClaudeAssistant<'a> {
    #[serde(borrow)]
    message: ClaudeMessage<'a>,
}

#[derive(Deserialize)]
struct ClaudeMessage<'a> {
    role: &'a str,
    model: Option<&'a str>,
    usage: Option<ClaudeInput>,
}

#[derive(Deserialize)]
struct ClaudeInput {
    input_tokens: Option<u64>,
    cache_read_input_tokens: Option<u64>,
    cache_creation_input_tokens: Option<u64>,
}

fn apply(reading: &mut Reading, provider: &str, thread: &str, line: &[u8]) {
    let Ok(header) = serde_json::from_slice::<Header>(line) else {
        return;
    };
    let kind = header.kind;
    if header.agent.is_some()
        || header.sidechain == Some(true)
        || (provider == "claude" && header.thread != Some(thread))
    {
        return;
    }
    // Deserialize measurements only. Tool arguments or assistant text can
    // occupy most of the read budget in one otherwise valid native record.
    if provider == "claude" && kind == "assistant" {
        if header.error == Some(true) {
            return;
        }
        let Ok(row) = serde_json::from_slice::<ClaudeAssistant>(line) else {
            reading.context = Window::default();
            return;
        };
        if row.message.role == "assistant" && row.message.model != Some("<synthetic>") {
            let used = row.message.usage.and_then(|u| {
                plus(
                    plus(u.input_tokens, u.cache_read_input_tokens),
                    u.cache_creation_input_tokens,
                )
            });
            reading.context = Window::new(used.filter(|n| *n > 0), None, "assistant.usage");
        }
        return;
    }
    match (provider, kind) {
        ("codex", "event_msg" | "compacted" | "turn_context")
        | ("claude", "cost-state" | "system")
        | (
            "copilot",
            "session.shutdown"
            | "session.usage_info"
            | "session.usage_checkpoint"
            | "session.context_cleared"
            | "session.model_change"
            | "session.model_deselected"
            | "session.compaction_complete"
            | "session.resume",
        ) => {}
        _ => return,
    }
    if line.len() > RECORD_BYTES {
        // An oversized authoritative row can replace an older observation.
        // Discard fields of unknown value without allocating its full JSON tree.
        match (provider, kind) {
            ("claude", "cost-state") | ("copilot", "session.usage_checkpoint") => {
                reading.tokens = Tokens::default()
            }
            ("claude", "system") if header.subtype != Some("compact_boundary") => {}
            ("codex", "event_msg") | ("copilot", "session.shutdown") => {
                *reading = Reading::default()
            }
            _ => reading.context = Window::default(),
        }
        return;
    }
    let Ok(v) = serde_json::from_slice::<Value>(line) else {
        return;
    };
    match provider {
        "codex" => {
            let p = &v["payload"];
            // A new turn can switch model or context tier. Its previous
            // percentage is not authoritative until the next measurement.
            if matches!(kind, "compacted" | "turn_context") || p["type"] == "context_compacted" {
                reading.context = Window::default();
            } else if p["type"] == "token_count" && p["info"].is_object() {
                let info = &p["info"];
                let t = &info["total_token_usage"];
                reading.context = Window::new(
                    count(&info["last_token_usage"]["total_tokens"]),
                    count(&info["model_context_window"]),
                    "token_count",
                );
                reading.tokens = Tokens::new(
                    count(&t["input_tokens"]),
                    count(&t["output_tokens"]),
                    count(&t["cached_input_tokens"]),
                    count(&t["total_tokens"]),
                    "token_count",
                );
            }
        }
        "claude" => {
            if kind == "cost-state" {
                reading.tokens = v["modelUsage"]
                    .as_object()
                    .map(|m| model_totals(m.values(), true, "cost-state"))
                    .unwrap_or_default();
            } else if kind == "system" && v["subtype"] == "compact_boundary" {
                reading.context = Window::default();
            }
        }
        "copilot" => {
            let d = &v["data"];
            match kind {
                "session.usage_info" => {
                    reading.context = Window::new(
                        count(&d["currentTokens"]),
                        count(&d["tokenLimit"]),
                        "session.usage_info",
                    );
                }
                "session.shutdown" | "session.usage_checkpoint" => {
                    let (metrics, source) = if kind == "session.shutdown" {
                        reading.context =
                            Window::new(count(&d["currentTokens"]), None, "session.shutdown");
                        (d, "session.shutdown")
                    } else {
                        (&d["accountingSnapshot"], "session.usage_checkpoint")
                    };
                    if let Some(models) = metrics["modelMetrics"].as_object() {
                        reading.tokens =
                            model_totals(models.values().map(|m| &m["usage"]), false, source);
                    } else if let Some(models) = metrics["providerModelMetrics"]
                        .as_array()
                        .or_else(|| d["providerModelMetrics"].as_array())
                    {
                        reading.tokens = model_totals(
                            models.iter().map(|m| &m["metrics"]["usage"]),
                            false,
                            source,
                        );
                    } else {
                        reading.tokens = Tokens::default();
                    }
                }
                "session.compaction_complete" if d["success"] != true => {}
                _ => reading.context = Window::default(),
            }
        }
        _ => {}
    }
}

fn read_tail(
    file: &mut File,
    length: u64,
    provider: &str,
    thread: &str,
    previous: Option<(u64, Reading)>,
) -> std::io::Result<Reading> {
    let (start, mut reading) =
        previous.unwrap_or_else(|| (length.saturating_sub(TAIL_BYTES), Reading::default()));
    file.seek(SeekFrom::Start(start.saturating_sub(1)))?;
    let mut bytes = Vec::with_capacity((length - start) as usize + 1);
    file.take(length - start.saturating_sub(1))
        .read_to_end(&mut bytes)?;
    let bytes = if start == 0 {
        &bytes[..]
    } else {
        bytes.splitn(2, |b| *b == b'\n').nth(1).unwrap_or(&[])
    };
    for line in bytes.split(|b| *b == b'\n').filter(|l| !l.is_empty()) {
        apply(&mut reading, provider, thread, line);
    }
    Ok(reading)
}

// Verify the old record boundary before reusing a reading after an append.
// Retain only the digest. Never retain the journal bytes themselves.
fn boundary(file: &mut File, end: u64) -> std::io::Result<Option<[u8; 32]>> {
    let mut buf = [0; 128];
    let size = end.min(buf.len() as u64) as usize;
    if size == 0 {
        return Ok(None);
    }
    file.seek(SeekFrom::Start(end - size as u64))?;
    file.read_exact(&mut buf[..size])?;
    Ok((buf[size - 1] == b'\n').then(|| Sha256::digest(&buf[..size]).into()))
}

// A bounded cache retains paths and numeric snapshots. Revalidate identity on
// the same open handle before accepting even an unchanged numeric snapshot.
#[derive(Clone)]
struct Entry {
    key: Value,
    path: Option<PathBuf>,
    checked: Instant,
    revision: Option<(u64, SystemTime)>,
    created: Option<SystemTime>,
    boundary: Option<[u8; 32]>,
    reading: Reading,
    observed: Option<String>,
}

impl Entry {
    fn update(
        &mut self,
        file: &mut File,
        old: Option<&Entry>,
        provider: &str,
        thread: &str,
    ) -> std::io::Result<()> {
        let stat = file.metadata()?;
        self.created = stat.created().ok();
        self.revision = stat.modified().ok().map(|mtime| (stat.len(), mtime));
        self.boundary = boundary(file, stat.len())?;
        let previous = old.filter(|e| e.path == self.path && e.created == self.created);
        if let Some(old) = previous.filter(|e| {
            self.revision.is_some() && self.revision == e.revision && self.boundary == e.boundary
        }) {
            self.reading = old.reading.clone();
            self.observed = old.observed.clone();
            return Ok(());
        }
        let mut continuation = None;
        if let Some(old) = previous.filter(|e| e.created.is_some() && e.boundary.is_some()) {
            if let Some((end, mtime)) = old.revision {
                if end < stat.len()
                    && stat.len() - end <= TAIL_BYTES
                    && stat.modified().is_ok_and(|t| t >= mtime)
                    && boundary(file, end)? == old.boundary
                {
                    continuation = Some((end, old.reading.clone()));
                }
            }
        }
        self.reading = read_tail(file, stat.len(), provider, thread, continuation)?;
        self.observed = Some(crate::core::now());
        Ok(())
    }
}

#[derive(Default)]
pub(crate) struct Cache(VecDeque<Entry>);

impl Cache {
    pub(crate) fn clear(&mut self) {
        self.0.clear();
    }
    fn remember(&mut self, entry: Entry) {
        self.0.retain(|e| e.key != entry.key);
        self.0.push_front(entry);
        self.0.truncate(CACHE_ENTRIES);
    }
}

fn key(session: &Value) -> Value {
    json!([
        session["id"],
        session["runId"],
        session["provider"],
        session["accountId"],
        session["directory"],
        session["previousDirectory"],
        session["archivedFromDirectory"],
        session["conversationId"]
    ])
}

fn open_bound(
    root: &Path,
    path: PathBuf,
    session: &Value,
    thread: &str,
) -> Option<(PathBuf, File)> {
    let canonical = path.canonicalize().ok()?;
    if !canonical.starts_with(root) || !canonical.is_file() {
        return None;
    }
    let file = File::open(&canonical).ok()?;
    history::matches_open_bound_log(&file, &canonical, session, thread).then_some((canonical, file))
}

// Native journals have provider-defined filenames. A cold poll enumerates
// names without opening unrelated conversations or accumulating all paths.
fn find_file(
    root: &Path,
    provider: &str,
    session: &Value,
    thread: &str,
) -> Option<(PathBuf, File)> {
    let mut components = Path::new(thread).components();
    if !matches!(
        (components.next(), components.next()),
        (Some(Component::Normal(_)), None)
    ) {
        return None;
    }
    if provider == "copilot" {
        return open_bound(
            root,
            root.join(thread).join("events.jsonl"),
            session,
            thread,
        );
    }
    let filename = format!("{thread}.jsonl");
    if provider == "claude" {
        return root
            .read_dir()
            .ok()?
            .take(SEARCH_ENTRIES)
            .flatten()
            .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
            .map(|e| e.path().join(&filename))
            .filter(|p| p.is_file())
            .take(CANDIDATES)
            .find_map(|p| open_bound(root, p, session, thread));
    }
    // Revert can leave several immutable rollouts with the same thread ID.
    // The filesystem cannot select the active one (Codex's state DB does).
    let suffix = format!("-{thread}");
    let mut candidate = None;
    for (index, entry) in ignore::WalkBuilder::new(root)
        .standard_filters(false)
        .max_depth(Some(5))
        .build()
        .enumerate()
    {
        if index >= SEARCH_ENTRIES {
            return None;
        }
        let entry = entry.ok()?;
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let Some(name) = entry.file_name().to_str() else {
            continue;
        };
        let Some(stem) = name
            .strip_suffix(".zst")
            .unwrap_or(name)
            .strip_suffix(".jsonl")
        else {
            continue;
        };
        let reverted = stem.rsplit_once('_').is_some_and(|(original, rollout)| {
            original.ends_with(&suffix) && uuid::Uuid::parse_str(rollout).is_ok()
        });
        if stem == thread || stem.ends_with(&suffix) || reverted {
            if candidate.is_some() || name.ends_with(".zst") {
                return None;
            }
            candidate = Some(entry.into_path());
        }
    }
    open_bound(root, candidate?, session, thread)
}

pub(crate) fn snapshot(core: &Arc<Core>, sid: &str) -> Result<Value> {
    let session = core.session(sid)?;
    let empty = || Reading::default().response(&session, None);
    let Some(thread) = session["conversationId"].as_str().filter(|v| !v.is_empty()) else {
        return Ok(empty());
    };
    if session["login"] == true || !terminal::usage_binding_matches(core, &session) {
        return Ok(empty());
    }
    let provider = session["provider"].as_str().unwrap_or_default();
    let dir = match provider {
        "claude" => "projects",
        "codex" => "sessions",
        "copilot" => "session-state",
        _ => return Ok(empty()),
    };
    let profile = Profile::for_history(core, provider, &session["accountId"])?;
    let Ok(root) = profile.config_dir().join(dir).canonicalize() else {
        return Ok(empty());
    };
    let scope = key(&session);
    let previous = core
        .session_usage
        .lock()
        .unwrap()
        .0
        .iter()
        .find(|e| e.key == scope)
        .cloned();
    if previous
        .as_ref()
        .is_some_and(|e| e.path.is_none() && e.checked.elapsed() < Duration::from_secs(8))
    {
        return Ok(empty());
    }
    let cached = previous
        .as_ref()
        .filter(|e| provider != "codex" || e.checked.elapsed() < Duration::from_secs(60))
        .and_then(|e| {
            open_bound(&root, e.path.clone()?, &session, thread).map(|file| (file, e.checked))
        });
    let (found, checked) = match cached {
        Some((file, checked)) => (Some(file), checked),
        None => (find_file(&root, provider, &session, thread), Instant::now()),
    };
    let mut entry = Entry {
        key: scope,
        path: None,
        checked,
        revision: None,
        created: None,
        boundary: None,
        reading: Reading::default(),
        observed: None,
    };
    if let Some((path, mut file)) = found {
        let stat = file.metadata()?;
        entry.path = Some(path.clone());
        entry.update(&mut file, previous.as_ref(), provider, thread)?;
        if !history::matches_open_bound_log(&file, &path, &session, thread) {
            return Ok(empty());
        }
        let after = file.metadata()?;
        if after.len() != stat.len() || stat.modified().ok() != after.modified().ok() {
            return Ok(empty());
        }
    }
    let current = core.session(sid)?;
    if key(&current) != entry.key || !terminal::usage_binding_matches(core, &current) {
        return Ok(Reading::default().response(&current, None));
    }
    let result = entry.reading.response(&current, entry.observed.as_deref());
    core.session_usage.lock().unwrap().remember(entry);
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, io::Write};

    fn feed(reading: &mut Reading, provider: &str, thread: &str, row: Value) {
        apply(
            reading,
            provider,
            thread,
            &serde_json::to_vec(&row).unwrap(),
        )
    }

    fn codex(context: u64, total: u64) -> Value {
        json!({"type":"event_msg","payload":{"type":"token_count","info":{
            "model_context_window":200,"last_token_usage":{"total_tokens":context},
            "total_token_usage":{"input_tokens":total-5,"output_tokens":5,"cached_input_tokens":8,"total_tokens":total}
        }}})
    }

    #[test]
    fn codex_separates_the_last_context_from_running_totals_and_plan_quota() {
        let mut r = Reading::default();
        feed(&mut r, "codex", "bound", codex(100, 150));
        feed(&mut r, "codex", "bound", codex(25, 600));
        feed(
            &mut r,
            "codex",
            "bound",
            json!({"type":"event_msg","payload":{"type":"token_count","info":null,"rate_limits":{"primary":{"used_percent":99}}}}),
        );
        assert_eq!(r.context.used_percent, Some(12.5));
        assert_eq!(r.tokens.total_tokens, Some(600));
        assert_eq!(r.tokens.cached_input_tokens, Some(8));
        feed(
            &mut r,
            "codex",
            "bound",
            json!({"type":"compacted","payload":{}}),
        );
        assert!(r.context.used_percent.is_none());
        assert_eq!(r.tokens.total_tokens, Some(600));
        feed(
            &mut r,
            "codex",
            "bound",
            json!({"type":"event_msg","payload":{"type":"token_count","info":{"model_context_window":0,"last_token_usage":{"total_tokens":20}}}}),
        );
        assert_eq!(r.context.used_tokens, Some(20));
        assert!(r.context.used_percent.is_none());
        assert!(r.tokens.total_tokens.is_none());
        feed(&mut r, "codex", "bound", codex(100, 180));
        feed(
            &mut r,
            "codex",
            "bound",
            json!({"type":"turn_context","payload":{"model":"other"}}),
        );
        assert!(r.context.used_percent.is_none());
        assert_eq!(r.tokens.total_tokens, Some(180));
    }

    #[test]
    fn claude_reads_authoritative_cost_state_and_ignores_stream_placeholders() {
        let mut r = Reading::default();
        let thread = crate::core::id();
        let mut assistant = json!({"type":"assistant","sessionId":thread,"isSidechain":false,"message":{
            "role":"assistant","model":"fixture","usage":{"input_tokens":10,"cache_read_input_tokens":25,
                "cache_creation_input_tokens":5,"output_tokens":1}
        }});
        for _ in 0..3 {
            feed(&mut r, "claude", &thread, assistant.clone());
        }
        assert_eq!(r.context.used_tokens, Some(40));
        assert!(r.context.limit_tokens.is_none());
        assert!(r.tokens.total_tokens.is_none());
        assistant["isSidechain"] = json!(true);
        assistant["message"]["usage"]["input_tokens"] = json!(1000);
        feed(&mut r, "claude", &thread, assistant);
        let model = json!({"inputTokens":11,"cacheReadInputTokens":25,"cacheCreationInputTokens":4,"outputTokens":20});
        let cost = json!({"type":"cost-state","sessionId":thread,"modelUsage":{"fixture-a":model,"fixture-b":model}});
        feed(&mut r, "claude", &thread, cost.clone());
        feed(&mut r, "claude", &thread, cost.clone());
        assert_eq!(r.tokens.total_tokens, Some(120));
        assert_eq!(r.tokens.input_tokens, Some(80));
        assert_eq!(r.tokens.cached_input_tokens, Some(50));
        assert_eq!(r.context.used_tokens, Some(40));
        let mut foreign = cost.clone();
        foreign["sessionId"] = json!(crate::core::id());
        foreign["modelUsage"]["fixture-a"]["inputTokens"] = json!(100);
        feed(&mut r, "claude", &thread, foreign);
        assert_eq!(r.tokens.total_tokens, Some(120));
        let mut partial = cost;
        partial["modelUsage"]["fixture-a"]
            .as_object_mut()
            .unwrap()
            .remove("outputTokens");
        feed(&mut r, "claude", &thread, partial);
        assert!(r.tokens.total_tokens.is_none());
        assert_eq!(r.tokens.input_tokens, Some(80));
        feed(
            &mut r,
            "claude",
            &thread,
            json!({"type":"system","subtype":"compact_boundary","sessionId":thread}),
        );
        assert!(r.context.used_tokens.is_none());
    }

    #[test]
    fn copilot_reads_durable_snapshots_and_never_adds_per_call_cost_or_quota() {
        let mut r = Reading::default();
        let user = json!({"type":"assistant.usage","data":{"inputTokens":500,"outputTokens":900,"quotaSnapshots":{"premium":{"remainingPercentage":5}}}});
        feed(&mut r, "copilot", "fixture", user);
        assert!(r.tokens.total_tokens.is_none());
        let usage = json!({"usage":{"inputTokens":80,"outputTokens":20,"cacheReadTokens":40,"reasoningTokens":12}});
        let checkpoint = json!({"type":"session.usage_checkpoint","data":{"accountingSnapshot":{"modelMetrics":{"active":usage}},"totalNanoAiu":999}});
        feed(&mut r, "copilot", "fixture", checkpoint.clone());
        feed(&mut r, "copilot", "fixture", checkpoint);
        assert_eq!(r.tokens.total_tokens, Some(100));
        assert!(r.context.used_tokens.is_none());
        let shutdown = json!({"type":"session.shutdown","data":{"currentTokens":75,"modelMetrics":{"one":usage,"two":usage}}});
        feed(&mut r, "copilot", "fixture", shutdown);
        assert_eq!(r.tokens.total_tokens, Some(200));
        assert_eq!(r.tokens.cached_input_tokens, Some(80));
        assert_eq!(r.context.used_tokens, Some(75));
        assert!(r.context.used_percent.is_none());
        feed(
            &mut r,
            "copilot",
            "fixture",
            json!({"type":"session.resume","data":{}}),
        );
        assert!(r.context.used_tokens.is_none());
        // Some event exporters can supply this ephemeral window signal; it is
        // optional and a normal durable Copilot journal need never contain it.
        feed(
            &mut r,
            "copilot",
            "fixture",
            json!({"type":"session.usage_info","data":{"currentTokens":37,"tokenLimit":100}}),
        );
        assert_eq!(r.context.used_percent, Some(37.0));
        feed(
            &mut r,
            "copilot",
            "fixture",
            json!({"type":"session.model_change","data":{"newModel":"fixture"}}),
        );
        assert!(r.context.used_percent.is_none());
        assert_eq!(r.tokens.total_tokens, Some(200));
    }

    #[test]
    fn large_inexact_and_missing_inputs_do_not_generate_warning_values() {
        assert!(count(&json!(MAX_COUNT + 1)).is_none());
        assert!(count(&json!(-1)).is_none());
        assert!(count(&json!(1.25)).is_none());
        assert!(count(&Value::Null).is_none());
        assert!(plus(Some(MAX_COUNT), Some(1)).is_none());
        assert_eq!(plus(Some(MAX_COUNT), Some(0)), Some(MAX_COUNT));
        let mut r = Reading::default();
        for provider in ["codex", "copilot", "claude"] {
            apply(
                &mut r,
                provider,
                "fixture",
                br#"{"type":"user","content":"token_count session.usage_info cost-state"}"#,
            );
            apply(
                &mut r,
                provider,
                "fixture",
                br#"{"type":"event_msg","payload":{"type":"token_count","info":"#,
            );
            let response = r.response(&json!({}), Some("unused"));
            assert_eq!(response["available"], false);
            assert!(response["observedAt"].is_null());
        }
    }

    #[test]
    fn oversized_assistant_and_compaction_rows_replace_old_context() {
        let body = "x".repeat(RECORD_BYTES + 1);
        let mut r = Reading::default();
        feed(&mut r, "codex", "fixture", codex(80, 200));
        feed(
            &mut r,
            "codex",
            "fixture",
            json!({"type":"compacted","payload":{"message":body}}),
        );
        assert!(r.context.used_tokens.is_none());
        assert_eq!(r.tokens.total_tokens, Some(200));
        feed(
            &mut r,
            "claude",
            "fixture",
            json!({"type":"assistant","sessionId":"fixture","message":{
                "role":"assistant","content":[{"text":body}],"usage":{"input_tokens":100,"cache_read_input_tokens":250,"cache_creation_input_tokens":25}
            }}),
        );
        assert_eq!(r.context.used_tokens, Some(375));
        feed(
            &mut r,
            "claude",
            "fixture",
            json!({"type":"assistant","sessionId":"fixture","message":{
                "role":"assistant","usage":{"input_tokens":0,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}
            }}),
        );
        assert!(r.context.used_tokens.is_none());
    }

    #[test]
    fn provider_lookup_validates_scope_and_reads_a_bounded_tail_on_the_same_handle() {
        let root =
            std::env::temp_dir().join(format!("workflow-usage-fixture-{}", crate::core::id()));
        fs::create_dir(&root).unwrap();
        let root = root.canonicalize().unwrap();
        let thread = crate::core::id();
        for provider in ["codex", "claude", "copilot"] {
            let scope_dir = root.join(provider);
            let ledger_dir = scope_dir.join(if provider == "copilot" {
                &thread
            } else {
                "fixture"
            });
            fs::create_dir_all(&ledger_dir).unwrap();
            let name = match provider {
                "copilot" => "events.jsonl".into(),
                "codex" => format!("rollout-{thread}.jsonl"),
                _ => format!("{thread}.jsonl"),
            };
            let path = ledger_dir.join(name);
            let header = match provider {
                "codex" => json!({"type":"session_meta","payload":{"id":thread,"cwd":root}}),
                "claude" => json!({"type":"user","sessionId":thread,"cwd":root}),
                _ => {
                    json!({"type":"session.start","data":{"sessionId":thread,"context":{"cwd":root}}})
                }
            };
            let metric = match provider {
                "codex" => codex(20, 50),
                "claude" => {
                    json!({"type":"cost-state","sessionId":thread,"modelUsage":{"test":{"inputTokens":10,"cacheReadInputTokens":15,"cacheCreationInputTokens":5,"outputTokens":20}}})
                }
                _ => {
                    json!({"type":"session.shutdown","data":{"modelMetrics":{"test":{"usage":{"inputTokens":30,"outputTokens":20}}}}})
                }
            };
            {
                let mut f = File::create(&path).unwrap();
                writeln!(f, "{header}").unwrap();
                f.write_all(&vec![b'x'; TAIL_BYTES as usize + RECORD_BYTES])
                    .unwrap();
                writeln!(f).unwrap();
                writeln!(f, "{metric}").unwrap();
                f.write_all(br#"{"type":"assistant""#).unwrap();
            }
            let session = json!({"id":"fixture-card","provider":provider,"directory":root,"conversationId":thread,"runId":crate::core::id()});
            let (_, mut f) = find_file(&scope_dir, provider, &session, &thread).unwrap();
            let len = f.metadata().unwrap().len();
            assert_eq!(
                read_tail(&mut f, len, provider, &thread, None)
                    .unwrap()
                    .tokens
                    .total_tokens,
                Some(50),
                "{provider}"
            );
            drop(f);
            let mut wrong = session.clone();
            wrong["directory"] = json!(scope_dir);
            assert!(find_file(&scope_dir, provider, &wrong, &thread).is_none());
            if provider == "codex" {
                let alt = ledger_dir.join(format!(
                    "rollout-fixture-{thread}_{}.jsonl",
                    crate::core::id()
                ));
                fs::copy(&path, &alt).unwrap();
                assert!(find_file(&scope_dir, provider, &session, &thread).is_none());
                fs::remove_file(&path).unwrap();
                assert!(find_file(&scope_dir, provider, &session, &thread).is_some());
                fs::rename(alt, &path).unwrap();
            }
            wrong = session.clone();
            wrong["runId"] = json!(crate::core::id());
            assert_ne!(key(&wrong), key(&session));
            wrong["runId"] = session["runId"].clone();
            wrong["accountId"] = json!(crate::core::id());
            assert_ne!(key(&wrong), key(&session));
            fs::write(&path, "{}\n").unwrap();
            assert!(find_file(&scope_dir, provider, &session, &thread).is_none());
            fs::remove_file(path).unwrap();
            fs::remove_dir(ledger_dir).unwrap();
            fs::remove_dir(scope_dir).unwrap();
        }
        fs::remove_dir(root).unwrap();
    }

    #[test]
    fn cached_numbers_and_missing_lookups_share_one_fixed_budget() {
        let mut cache = Cache::default();
        for i in 0..3 * CACHE_ENTRIES {
            cache.remember(Entry {
                key: json!(i),
                path: None,
                checked: Instant::now(),
                reading: Reading::default(),
                revision: None,
                created: None,
                boundary: None,
                observed: None,
            });
        }
        assert_eq!(cache.0.len(), CACHE_ENTRIES);
        assert_eq!(cache.0.front().unwrap().key, 3 * CACHE_ENTRIES - 1);
        cache.clear();
        assert!(cache.0.is_empty());
    }

    #[test]
    fn follows_complete_appends_beyond_the_tail_and_drops_a_rewritten_prefix() {
        let path =
            std::env::temp_dir().join(format!("workflow-usage-append-{}", crate::core::id()));
        let mut file = fs::OpenOptions::new()
            .create_new(true)
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        writeln!(file, "{}", codex(40, 200)).unwrap();
        let mut cache = Entry {
            path: Some(path.clone()),
            key: Value::Null,
            checked: Instant::now(),
            revision: None,
            created: None,
            boundary: None,
            reading: Reading::default(),
            observed: None,
        };
        cache.update(&mut file, None, "codex", "test").unwrap();
        let opaque = json!({"type":"response_item","text":"a".repeat(128*1024)});
        for _ in 0..12 {
            let previous = cache.clone();
            file.seek(SeekFrom::End(0)).unwrap();
            writeln!(file, "{opaque}").unwrap();
            cache
                .update(&mut file, Some(&previous), "codex", "test")
                .unwrap();
            assert_eq!(cache.reading.tokens.total_tokens, Some(200));
        }
        // An in-place rewrite and growth is not a continuation of the old log.
        let previous = cache.clone();
        file.seek(SeekFrom::End(-100)).unwrap();
        file.write_all(b"b").unwrap();
        file.seek(SeekFrom::End(0)).unwrap();
        writeln!(file, "{opaque}").unwrap();
        cache
            .update(&mut file, Some(&previous), "codex", "test")
            .unwrap();
        assert!(cache.reading.tokens.total_tokens.is_none());
        drop(file);
        fs::remove_file(path).unwrap();
    }
}
