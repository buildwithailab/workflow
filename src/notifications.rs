use crate::{
    chat::{NoticeLog, NoticeTurn},
    core::{id, string, Core},
    terminal::{self, NoticeScreen},
};
use anyhow::{bail, Result};
use serde::Serialize;
use serde_json::{json, Value};
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time::Duration,
};
use tauri::Manager;

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct Entry {
    id: String,
    session_id: String,
    project_id: String,
    kind: &'static str,
    created_at: i64,
    dismissed: bool,
    silent: bool,
    presentation: Option<&'static str>,
    presented: bool,
    #[serde(skip)]
    thread: Option<String>,
}

#[derive(Default)]
pub(crate) struct Notifications {
    entries: Vec<Entry>,
    revision: u64,
    generation: u64,
    disabled: bool,
    focused: bool,
    foreground: bool,
    selected: Option<String>,
    turkish: bool,
    enabled_since: i64,
    overlay_kind: Option<bool>,
    overlay_applied: bool,
    desktop_attempts: u64,
    desktop_shown: u64,
    context_token: String,
    context_revision: u64,
    desktop_current: Option<String>,
}

impl Notifications {
    fn eligible_desktop(&self, event: &str) -> bool {
        !self.disabled
            && self.desktop_current.as_deref() == Some(event)
            && self.entries.iter().any(|e| {
                e.id == event && e.presentation == Some("desktop") && !e.silent && !e.dismissed
            })
    }
    fn desktop_candidate(&self) -> Option<String> {
        let current = self
            .desktop_current
            .as_ref()
            .and_then(|id| self.entries.iter().find(|e| &e.id == id && !e.dismissed));
        let next = self
            .entries
            .iter()
            .filter(|e| {
                e.presentation == Some("desktop") && !e.silent && !e.dismissed && !e.presented
            })
            .min_by_key(|e| (e.kind == "completed", std::cmp::Reverse(e.created_at)));
        match (current, next) {
            (None, next) => next.map(|e| e.id.clone()),
            (Some(current), Some(next))
                if current.kind == "completed"
                    && (next.kind != "completed" || next.created_at > current.created_at) =>
            {
                Some(next.id.clone())
            }
            (Some(current), _) => Some(current.id.clone()),
        }
    }
    fn snapshot(&self) -> Value {
        let mut snapshot = json!({"enabled": !self.disabled, "entries": self.entries, "revision": self.revision, "foreground": self.foreground});
        if std::env::var("WORKFLOW_TEST_MODE").as_deref() == Ok("1") {
            snapshot["diagnostics"] = json!({
                "overlayKind": self.overlay_kind.map(|waiting| if waiting { "attention" } else { "completed" }),
                "overlayApplied": self.overlay_applied, "desktopAttempts": self.desktop_attempts, "desktopShown": self.desktop_shown
            });
        }
        snapshot
    }

    fn followed(&self, session: &str, foreground: bool) -> bool {
        foreground && self.focused && self.selected.as_deref() == Some(session)
    }

    fn insert(
        &mut self,
        sid: &str,
        pid: &str,
        kind: &'static str,
        silent: bool,
        foreground: bool,
    ) -> Option<String> {
        self.entries.retain(|entry| entry.session_id != sid);
        let followed = self.followed(sid, foreground);
        if self.disabled || (kind == "completed" && followed) {
            return None;
        }
        let event = id();
        self.entries.push(Entry {
            id: event.clone(),
            session_id: sid.into(),
            project_id: pid.into(),
            kind,
            created_at: chrono::Utc::now().timestamp_millis(),
            dismissed: false,
            silent,
            presentation: (!silent && !followed).then_some(if foreground {
                "in_app"
            } else {
                "desktop"
            }),
            presented: false,
            thread: None,
        });
        Some(event)
    }

    fn clear_pending(&mut self, sid: &str) {
        self.entries
            .retain(|entry| entry.session_id != sid || entry.kind == "completed");
    }

    fn overlay(&self, foreground: bool) -> Option<bool> {
        let mut done = false;
        for entry in &self.entries {
            if self.followed(&entry.session_id, foreground) {
                continue;
            }
            if entry.kind != "completed" {
                return Some(true);
            }
            done = true;
        }
        done.then_some(false)
    }
}

pub(crate) fn sync_enabled(core: &Core) {
    let disabled = core.db.lock().unwrap()["preferences"]["notificationsEnabled"] == false;
    let mut state = core.notifications.lock().unwrap();
    if state.disabled != disabled {
        state.disabled = disabled;
        state.enabled_since = chrono::Utc::now().timestamp_millis();
        state.generation += 1;
        state.entries.clear();
        state.revision += 1;
        core.emit("notifications-updated", state.snapshot());
    }
}

pub(crate) fn dispatch(core: &Core, action: &str, args: &Value) -> Result<Value> {
    let is_foreground = foreground(core);
    let mut state = core.notifications.lock().unwrap();
    state.foreground = is_foreground;
    let changed = match action {
        "read" => false,
        "attach" => {
            state.context_token = id();
            state.context_revision = 0;
            state.selected = None;
            state.focused = false;
            let mut result = state.snapshot();
            result["contextToken"] = json!(state.context_token);
            return Ok(result);
        }
        "context" => {
            let revision = args["contextRevision"].as_u64().unwrap_or(0);
            if args["contextToken"].as_str() != Some(state.context_token.as_str())
                || revision <= state.context_revision
            {
                return Ok(state.snapshot());
            }
            state.context_revision = revision;
            state.focused = args["focused"] == true;
            state.selected = args["sessionId"].as_str().map(str::to_owned);
            if let Some(lang) = args["language"].as_str() {
                state.turkish = lang == "tr";
            }
            let count = state.entries.len();
            let followed = state
                .selected
                .clone()
                .filter(|sid| state.followed(sid, is_foreground));
            state
                .entries
                .retain(|e| e.kind != "completed" || followed.as_deref() != Some(&e.session_id));
            state.entries.len() != count
        }
        "dismiss" => {
            let event = string(args, "id")?;
            state
                .entries
                .iter_mut()
                .find(|e| e.id == event && !e.dismissed)
                .is_some_and(|e| {
                    e.dismissed = true;
                    true
                })
        }
        "seen" => {
            let event = string(args, "id")?;
            let count = state.entries.len();
            if is_foreground {
                state
                    .entries
                    .retain(|e| e.id != event || e.kind != "completed");
            }
            state.entries.len() != count
        }
        "presented" => {
            let event = string(args, "id")?;
            state
                .entries
                .iter_mut()
                .find(|e| {
                    e.id == event
                        && e.presentation == Some("in_app")
                        && !e.dismissed
                        && !e.silent
                        && !e.presented
                        && is_foreground
                })
                .is_some_and(|e| {
                    e.presented = true;
                    true
                })
        }
        _ => bail!("Geçersiz bildirim işlemi"),
    };
    if changed {
        state.revision += 1;
    }
    let mut snapshot = state.snapshot();
    if changed {
        core.emit("notifications-updated", snapshot.clone());
    }
    drop(state);
    if std::env::var("WORKFLOW_TEST_MODE").as_deref() == Ok("1") {
        let popup = core.app.get_window(crate::notification_popup::LABEL);
        snapshot["diagnostics"]["popupVisible"] = json!(popup
            .as_ref()
            .is_some_and(|w| w.is_visible().unwrap_or(false)));
        snapshot["diagnostics"]["popupMinimized"] = json!(popup
            .as_ref()
            .is_some_and(|w| w.is_minimized().unwrap_or(true)));
        snapshot["diagnostics"]["mainMinimized"] = json!(core
            .app
            .get_window("main")
            .is_some_and(|w| w.is_minimized().unwrap_or(false)));
    }
    Ok(snapshot)
}

// Presentation is acknowledged by the renderer only after the card is visible.
// Until then a focus transition can move the same event to the other surface.
fn present_pending(core: &Arc<Core>) {
    let is_foreground = foreground(core);
    let mut state = core.notifications.lock().unwrap();
    let mut changed = false;
    let followed = state
        .selected
        .clone()
        .filter(|sid| state.followed(sid, is_foreground));
    for entry in &mut state.entries {
        if entry.presented || entry.dismissed || entry.silent {
            continue;
        }
        let next = if followed.as_deref() == Some(&entry.session_id) {
            None
        } else {
            Some(if is_foreground { "in_app" } else { "desktop" })
        };
        if entry.presentation != next {
            entry.presentation = next;
            changed = true;
        }
    }
    if is_foreground || state.disabled {
        state.desktop_current = None;
    } else {
        let next = state.desktop_candidate();
        if next != state.desktop_current {
            state.desktop_current = next;
            state.desktop_attempts += u64::from(state.desktop_current.is_some());
        }
    }
    let card = state
        .desktop_current
        .as_ref()
        .and_then(|id| state.entries.iter().find(|e| &e.id == id))
        .map(|e| (e.id.clone(), e.kind, state.turkish));
    if changed {
        state.revision += 1;
        core.emit("notifications-updated", state.snapshot());
    }
    drop(state);
    if let Some((id, kind, turkish)) = card {
        let _ = crate::notification_popup::present(core, &id, kind, turkish);
    } else {
        crate::notification_popup::hide(core);
    }
}

pub(crate) fn popup_event(core: &Core, event: &str) -> Option<Value> {
    if foreground(core) {
        return None;
    }
    let state = core.notifications.lock().unwrap();
    if !state.eligible_desktop(event) {
        return None;
    }
    state
        .entries
        .iter()
        .find(|e| e.id == event && e.presentation == Some("desktop") && !e.silent && !e.dismissed)
        .map(|e| json!({"id": e.id, "kind": e.kind, "turkish": state.turkish}))
}

pub(crate) fn popup_action(core: &Arc<Core>, action: &str, args: &Value) -> Result<Value> {
    let event = string(args, "id")?;
    if foreground(core) {
        return Ok(json!(false));
    }
    if action == "open" {
        let sid = {
            let state = core.notifications.lock().unwrap();
            if !state.eligible_desktop(event) {
                return Ok(json!(false));
            }
            state
                .entries
                .iter()
                .find(|e| e.id == event)
                .map(|e| e.session_id.clone())
        };
        let Some(sid) = sid else {
            return Ok(json!(false));
        };
        let owner = core.session(&sid)?;
        let project_exists =
            core.db.lock().unwrap()["projects"]
                .as_array()
                .is_some_and(|projects| {
                    projects
                        .iter()
                        .any(|p| p["id"] == owner["projectId"] && p["removed"] != true)
                });
        if owner["archived"] == true || !project_exists {
            return Ok(json!(false));
        }
        let window = core
            .app
            .get_window("main")
            .ok_or_else(|| anyhow::anyhow!("Workflow penceresi bulunamadı."))?;
        window.show()?;
        window.unminimize()?;
        window.set_focus()?;
        let mut state = core.notifications.lock().unwrap();
        state
            .entries
            .retain(|e| e.id != event || e.kind != "completed");
        if let Some(entry) = state.entries.iter_mut().find(|e| e.id == event) {
            entry.dismissed = true;
        }
        if state.desktop_current.as_deref() == Some(event) {
            state.desktop_current = None;
        }
        state.revision += 1;
        core.emit("notifications-updated", state.snapshot());
        drop(state);
        core.emit("notification-session", json!(sid));
        return Ok(json!(true));
    }
    let visible = core
        .app
        .get_window(crate::notification_popup::LABEL)
        .is_some_and(|w| w.is_visible().unwrap_or(false) && !w.is_minimized().unwrap_or(true));
    let mut state = core.notifications.lock().unwrap();
    if !state.eligible_desktop(event) {
        return Ok(json!(false));
    }
    let mut changed = false;
    if let Some(entry) = state.entries.iter_mut().find(|e| e.id == event) {
        if action == "presented" && visible && !entry.presented {
            entry.presented = true;
            state.desktop_shown += 1;
            changed = true;
        } else if action == "dismiss" {
            entry.dismissed = true;
            entry.presented = true;
            state.desktop_current = None;
            changed = true;
        }
    }
    if changed {
        state.revision += 1;
        core.emit("notifications-updated", state.snapshot());
    }
    drop(state);
    Ok(json!(changed || action == "presented" && visible))
}

#[derive(Default)]
struct Watch {
    run: String,
    thread: Option<String>,
    log: NoticeLog,
    latest: Option<NoticeTurn>,
    sampled_log: bool,
    prompt: Option<(&'static str, String)>,
    prompt_input: u64,
    semantic_prompt: bool,
    choice: Option<(u64, u64)>,
    finished_submission: u64,
    semantic_sequence: u64,
    current: CurrentTurn,
    previous_position: Option<u64>,
}

// A PTY cycle, a verified submission and a user row may become known in
// different polls. Keep their identities together until new work is observed.
#[derive(Default)]
struct CurrentTurn {
    cycle: Option<u64>,
    submission: u64,
    position: Option<u64>,
    outcome: &'static str,
    announced: bool,
}

enum Change {
    Waiting(&'static str),
    RestoreQuestion,
    Completed,
    Clear,
    Reset,
    None,
}

impl Watch {
    fn next_turn(&mut self) {
        if let Some(position) = self.current.position {
            self.previous_position = Some(position);
        }
        self.current = CurrentTurn::default();
    }

    fn observe(
        &mut self,
        mut screen: NoticeScreen,
        turn: Option<NoticeTurn>,
        since: i64,
    ) -> Change {
        let semantic = screen
            .semantic
            .filter(|s| s.sequence != self.semantic_sequence);
        if let Some(s) = semantic {
            self.semantic_sequence = s.sequence;
            if self.current.cycle.is_some_and(|cycle| cycle != s.cycle)
                || self.current.cycle.is_none()
                    && self.current.submission != 0
                    && s.submission != 0
                    && s.submission != self.current.submission
            {
                self.next_turn();
            }
            self.current.cycle = Some(s.cycle);
            self.current.submission = s.submission;
        }

        let previous_choice = self.choice;
        let changed = turn.is_some() && turn != self.latest;
        let eligible = changed
            && (self.sampled_log
                || turn.is_some_and(|t| t.started_ms.is_some_and(|start| start >= since)));
        if turn.is_some() {
            self.latest = turn;
            self.sampled_log = true;
        }
        if self
            .choice
            .is_some_and(|(_, submitted)| submitted != screen.submitted)
        {
            self.choice = None;
        }
        let current_log = turn.filter(|t| eligible && Some(t.position) != self.previous_position);
        if let Some(t) = current_log {
            if self
                .current
                .position
                .is_some_and(|position| position != t.position)
                || self.current.position.is_none()
                    && semantic.is_none()
                    && self.current.submission != 0
                    && screen.submitted > self.current.submission
            {
                self.next_turn();
            }
            self.current.position = Some(t.position);
            if self.current.submission == 0 {
                self.current.submission = screen.submitted;
            }
            self.choice = None;
            if t.state == "interrupted" || self.current.outcome != "interrupted" {
                match t.state {
                    "completed" | "interrupted" => self.current.outcome = t.state,
                    "question" => {
                        self.current.outcome = "question";
                        self.choice = Some((t.position, screen.submitted));
                    }
                    _ => {}
                }
            }
        }

        if self.thread.is_none() && screen.finished_submission > self.finished_submission {
            self.finished_submission = screen.finished_submission;
            if self.current.submission < screen.finished_submission {
                if self.current.submission != 0 {
                    self.next_turn();
                }
                self.current.submission = screen.finished_submission;
            }
            if self.current.submission == screen.finished_submission
                && self.current.outcome.is_empty()
            {
                self.current.outcome = "completed";
            }
        }

        let mut cleared = false;
        let mut defer_completion = false;
        if let Some(s) = semantic {
            match s.kind {
                "approval" | "question" => {
                    self.semantic_prompt = true;
                    if screen.prompt.is_none() {
                        if self.prompt.as_ref().is_some_and(|p| p.0 == s.kind) {
                            return Change::None;
                        }
                        screen.prompt = Some((s.kind, format!("osc:{}", s.sequence)));
                    }
                }
                "working" | "completed" | "interrupted" => {
                    cleared = self.prompt.take().is_some();
                    self.semantic_prompt = false;
                    if cleared {
                        screen.prompt = None;
                    }
                    if s.kind == "interrupted" {
                        self.current.outcome = "interrupted";
                        self.choice = None;
                    } else if s.kind == "working" && self.current.outcome != "question" {
                        self.choice = None;
                    } else if s.kind == "completed" && self.current.outcome.is_empty() {
                        self.current.outcome = "completed";
                        // Give a bound log one observation to classify a final
                        // reply as a question or cancellation. A stale/missing
                        // log cannot indefinitely prevent the OSC result.
                        defer_completion = self.thread.is_some();
                    }
                    if s.kind == "completed" && s.observed_ms < since {
                        // A new watch can see the PTY's last live signal after
                        // notifications are re-enabled. Consume that baseline.
                        self.current.announced = true;
                    }
                }
                _ => {}
            }
        }
        if self.current.outcome == "interrupted" && self.current.announced {
            self.current.announced = false;
            return Change::Reset;
        }

        if self.prompt.is_some()
            && screen.answered != self.prompt_input
            && !semantic.is_some_and(|s| matches!(s.kind, "approval" | "question"))
        {
            self.prompt = None;
            self.semantic_prompt = false;
            screen.prompt = None;
            cleared = true;
        }
        if let Some(prompt) = screen.prompt {
            if self
                .prompt
                .as_ref()
                .is_some_and(|old| old.0 == prompt.0 && old.1.starts_with("osc:"))
            {
                self.prompt = Some(prompt);
                return Change::None;
            }
            if self.prompt.as_ref() != Some(&prompt) {
                let kind = prompt.0;
                self.prompt_input = screen.answered;
                self.prompt = Some(prompt);
                return Change::Waiting(kind);
            }
            return Change::None;
        }
        if self.prompt.is_some() {
            let terminal_log = current_log.is_some_and(|t| t.state != "working");
            if terminal_log || !self.semantic_prompt && screen.ready {
                self.prompt = None;
                self.semantic_prompt = false;
                cleared = true;
            } else {
                return Change::None;
            }
        }
        if self.choice.is_some() && (self.choice != previous_choice || cleared) {
            return Change::Waiting("question");
        }
        if self.current.outcome == "completed" && !self.current.announced && !defer_completion {
            self.current.announced = true;
            return Change::Completed;
        }
        if cleared || previous_choice.is_some() && self.choice.is_none() {
            Change::Clear
        } else {
            Change::None
        }
    }
}

fn foreground(core: &Core) -> bool {
    // The main native window also hosts embedded browser WebViews. Tauri's
    // get_webview_window excludes such multi-WebView windows.
    core.app.get_window("main").is_some_and(|window| {
        if !window.is_visible().unwrap_or(false) || window.is_minimized().unwrap_or(true) {
            return false;
        }
        #[cfg(windows)]
        {
            window.hwnd().is_ok_and(|hwnd| unsafe {
                windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow().0 == hwnd.0
            })
        }
        #[cfg(not(windows))]
        {
            window.is_focused().unwrap_or(false)
        }
    })
}

#[cfg(windows)]
fn set_overlay(core: &Core, attention: Option<bool>) -> bool {
    let Some(window) = core.app.get_window("main") else {
        return false;
    };
    let icon = attention.map(|waiting| {
        let color = if waiting {
            [227, 184, 97, 255]
        } else {
            [190, 236, 145, 255]
        };
        let mut rgba = Vec::with_capacity(16 * 16 * 4);
        for y in 0..16i32 {
            for x in 0..16i32 {
                let circle = (2 * x - 15).pow(2) + (2 * y - 15).pow(2) <= 225;
                let ink = if waiting {
                    (7..=8).contains(&x) && ((3..=8).contains(&y) || (10..=11).contains(&y))
                } else {
                    (3..=7).contains(&x) && (y - x - 3).abs() < 2
                        || (7..=12).contains(&x) && (y + x - 17).abs() < 2
                };
                rgba.extend_from_slice(if !circle {
                    &[0, 0, 0, 0]
                } else if ink {
                    &[22, 25, 29, 255]
                } else {
                    &color
                });
            }
        }
        tauri::image::Image::new_owned(rgba, 16, 16)
    });
    window.set_overlay_icon(icon).is_ok()
}

#[cfg(not(windows))]
fn set_overlay(_core: &Core, _attention: Option<bool>) -> bool {
    true
}

pub(crate) fn start(core: &Arc<Core>) -> Result<()> {
    sync_enabled(core);
    let weak = Arc::downgrade(core);
    std::thread::spawn(move || {
        let mut watches: HashMap<String, Watch> = HashMap::new();
        let mut overlay = None;
        let mut last_generation = 0;
        loop {
            std::thread::sleep(Duration::from_millis(650));
            let Some(core) = weak.upgrade() else {
                break;
            };
            let (disabled, generation, since) = {
                let state = core.notifications.lock().unwrap();
                (state.disabled, state.generation, state.enabled_since)
            };
            let reenabled = generation != last_generation;
            last_generation = generation;
            if disabled || reenabled {
                watches.clear();
            }
            if !disabled {
                poll(&core, &mut watches, generation, reenabled, since);
            }
            present_pending(&core);
            let foreground = foreground(&core);
            let next = core.notifications.lock().unwrap().overlay(foreground);
            if next != overlay {
                let applied = set_overlay(&core, next);
                let mut state = core.notifications.lock().unwrap();
                state.overlay_kind = next;
                state.overlay_applied = applied;
                if applied {
                    overlay = next;
                }
            }
        }
    });
    Ok(())
}

fn poll(
    core: &Core,
    watches: &mut HashMap<String, Watch>,
    generation: u64,
    quiet: bool,
    enabled_since: i64,
) {
    let sessions: Vec<Value> = {
        let db = core.db.lock().unwrap();
        let projects: HashSet<_> = db["projects"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|p| p["removed"] != true)
            .filter_map(|p| p["id"].as_str())
            .collect();
        db["sessions"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|s| {
                s["archived"] != true
                    && s["login"] != true
                    && s["projectId"]
                        .as_str()
                        .is_some_and(|pid| projects.contains(pid))
            })
            .cloned()
            .collect()
    };
    let owners: HashMap<_, _> = sessions
        .iter()
        .filter_map(|s| Some((s["id"].as_str()?, s)))
        .collect();
    let terminals = core.terms.lock().unwrap().clone();
    let mut live = HashSet::new();
    let mut changes = Vec::new();
    for session in &sessions {
        let (Some(sid), Some(pid)) = (session["id"].as_str(), session["projectId"].as_str()) else {
            continue;
        };
        let Some(terminal) = terminals.get(sid) else {
            continue;
        };
        if session["runId"].as_str() != Some(terminal.run_id.as_str()) {
            continue;
        }
        let Some(screen) = terminal::notice_screen(terminal, session) else {
            continue;
        };
        live.insert(sid);
        let thread = session["conversationId"].as_str();
        let watch = watches.entry(sid.into()).or_default();
        if watch.run == terminal.run_id && watch.thread.is_none() && thread.is_some() {
            watch.thread = thread.map(str::to_owned);
        } else if watch.run != terminal.run_id || watch.thread.as_deref() != thread {
            if !watch.run.is_empty() {
                changes.push((sid, pid, Change::Reset, false));
            }
            *watch = Watch {
                run: terminal.run_id.clone(),
                thread: thread.map(str::to_owned),
                ..Watch::default()
            };
        }
        let turn = thread.and_then(|t| watch.log.latest(core, session, t));
        let since = session["lastUsedAt"]
            .as_str()
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            .map_or(i64::MAX, |time| time.timestamp_millis());
        // On re-enable an outstanding reply choice is still actionable even if
        // its log becomes available later. Restore only its badge, silently.
        let restore = !watch.sampled_log
            && screen.prompt.is_none()
            && turn.is_some_and(|t| {
                t.state == "question"
                    && t.started_ms
                        .is_some_and(|time| time >= since && time < enabled_since)
            });
        let mut change = watch.observe(
            screen,
            turn,
            if restore {
                since
            } else {
                since.max(enabled_since)
            },
        );
        if restore && matches!(change, Change::Waiting("question")) {
            change = Change::RestoreQuestion;
        }
        watch.sampled_log |= watch.log.has_revision();
        if !matches!(change, Change::None) {
            let started_after_enable = since >= enabled_since
                || turn
                    .is_some_and(|t| t.started_ms.is_some_and(|started| started >= enabled_since));
            let baseline = quiet && !started_after_enable;
            changes.push((sid, pid, change, baseline));
        }
    }
    watches.retain(|sid, _| live.contains(sid.as_str()));
    let is_foreground = foreground(core);
    let mut state = core.notifications.lock().unwrap();
    if state.disabled || state.generation != generation {
        return;
    }
    state.foreground = is_foreground;
    let before = state.entries.len();
    state.entries.retain_mut(|e| {
        let Some(owner) = owners.get(e.session_id.as_str()) else {
            return false;
        };
        let current = owner["conversationId"].as_str();
        if e.thread.is_some() && e.thread.as_deref() != current {
            return false;
        }
        if e.thread.is_none() {
            e.thread = current.map(str::to_owned);
        }
        e.kind == "completed" || live.contains(e.session_id.as_str())
    });
    let mut changed = before != state.entries.len();
    for (sid, pid, change, baseline) in changes {
        let before = state.entries.len();
        let event = match change {
            Change::Waiting(kind) => state.insert(sid, pid, kind, baseline, is_foreground),
            Change::RestoreQuestion => state.insert(sid, pid, "question", true, is_foreground),
            Change::Completed if baseline => None,
            Change::Completed => state.insert(sid, pid, "completed", false, is_foreground),
            Change::Clear => {
                state.clear_pending(sid);
                None
            }
            Change::Reset => {
                state.entries.retain(|e| e.session_id != sid);
                None
            }
            Change::None => None,
        };
        changed |= before != state.entries.len() || event.is_some();
        if event.is_some() {
            if let Some(entry) = state.entries.last_mut() {
                entry.thread = owners[sid]["conversationId"].as_str().map(str::to_owned);
            }
        }
    }
    if changed {
        state.revision += 1;
        core.emit("notifications-updated", state.snapshot());
    }
    drop(state);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn visible_pending_card_is_stable_and_stale_ids_cannot_ack_its_replacement() {
        let mut state = Notifications::default();
        let first = state.insert("one", "project", "approval", false, false).unwrap();
        let second = state.insert("two", "project", "question", false, false).unwrap();
        state.entries[0].created_at = 1;
        state.entries[1].created_at = 2;
        state.desktop_current = state.desktop_candidate();
        assert_eq!(state.desktop_current.as_deref(), Some(second.as_str()));
        state.entries[1].presented = true;
        assert_eq!(state.desktop_candidate().as_deref(), Some(second.as_str()));
        assert!(!state.eligible_desktop(&first));
        assert!(state.eligible_desktop(&second));
        state.entries[1].dismissed = true;
        state.desktop_current = state.desktop_candidate();
        assert_eq!(state.desktop_current.as_deref(), Some(first.as_str()));
        assert!(!state.eligible_desktop(&second));
        assert!(state.eligible_desktop(&first));
        state.disabled = true;
        assert!(!state.eligible_desktop(&first));
    }

    fn screen(kind: Option<&'static str>, submitted: u64, ready: bool) -> NoticeScreen {
        NoticeScreen {
            prompt: kind.map(|k| (k, "menu".into())),
            ready,
            submitted,
            answered: 0,
            finished_submission: 0,
            semantic: None,
        }
    }
    fn turn(position: u64, state: &'static str) -> Option<NoticeTurn> {
        Some(NoticeTurn {
            position,
            started_ms: Some(position as i64),
            state,
        })
    }

    #[test]
    fn current_turn_completes_once_without_replaying_resume() {
        let mut watch = Watch::default();
        assert!(matches!(
            watch.observe(screen(None, 0, true), turn(1, "completed"), 5),
            Change::None
        ));
        assert!(matches!(
            watch.observe(screen(None, 0, false), turn(10, "working"), 5),
            Change::None
        ));
        assert!(matches!(
            watch.observe(screen(None, 0, true), turn(10, "completed"), 5),
            Change::Completed
        ));
        assert!(matches!(
            watch.observe(screen(None, 0, true), turn(10, "completed"), 5),
            Change::None
        ));
        assert!(matches!(
            watch.observe(screen(None, 0, true), turn(12, "interrupted"), 5),
            Change::None
        ));
    }

    fn osc(submission: u64, sequence: u64, cycle: u64, kind: &'static str) -> NoticeScreen {
        let mut sample = screen(None, submission, true);
        sample.semantic = Some(crate::terminal_notifications::Signal {
            sequence,
            cycle,
            submission,
            observed_ms: i64::MAX,
            kind,
        });
        sample
    }

    #[test]
    fn footer_osc_and_late_log_share_one_completion_in_either_order() {
        for first_osc in [true, false] {
            let mut watch = Watch::default();
            let mut first = screen(None, 1, true);
            if first_osc {
                first = osc(1, 2, 1, "completed");
            } else {
                first.finished_submission = 1;
            }
            assert!(matches!(watch.observe(first, None, 0), Change::Completed));
            let mut both = osc(1, 2, 1, "completed");
            both.finished_submission = 1;
            assert!(matches!(watch.observe(both, None, 0), Change::None));
            watch.thread = Some("synthetic".into());
            assert!(matches!(
                watch.observe(screen(None, 1, true), turn(10, "completed"), 0),
                Change::None
            ));
        }
    }

    #[test]
    fn semantic_prompt_survives_idle_composer_until_answer_or_progress() {
        for resolution in ["working", "completed", "interrupted", "answer"] {
            let mut watch = Watch::default();
            assert!(matches!(
                watch.observe(osc(1, 2, 1, "approval"), None, 0),
                Change::Waiting("approval")
            ));
            assert!(matches!(
                watch.observe(osc(1, 2, 1, "approval"), None, 0),
                Change::None
            ));
            assert!(watch.prompt.is_some());
            let mut next = osc(1, 3, 1, resolution);
            if resolution == "answer" {
                next = osc(1, 2, 1, "approval");
                next.answered = 1;
            }
            let result = watch.observe(next, None, 0);
            assert!(matches!(result, Change::Clear | Change::Completed));
            assert!(watch.prompt.is_none());
        }
    }

    #[test]
    fn semantic_completion_outlives_a_lagging_bound_log_and_uses_its_question() {
        let mut watch = Watch {
            thread: Some("synthetic".into()),
            ..Watch::default()
        };
        assert!(matches!(
            watch.observe(osc(1, 1, 1, "working"), turn(1, "completed"), 5),
            Change::None
        ));
        assert!(matches!(
            watch.observe(osc(1, 2, 1, "completed"), turn(1, "completed"), 5),
            Change::None
        ));
        assert!(matches!(
            watch.observe(osc(1, 2, 1, "completed"), turn(1, "completed"), 5),
            Change::Completed
        ));
        assert!(matches!(
            watch.observe(osc(1, 2, 1, "completed"), turn(10, "completed"), 5),
            Change::None
        ));
        assert!(matches!(
            watch.observe(osc(2, 3, 2, "working"), turn(20, "working"), 5),
            Change::None
        ));
        assert!(matches!(
            watch.observe(osc(2, 4, 2, "completed"), turn(20, "working"), 5),
            Change::None
        ));
        assert!(matches!(
            watch.observe(osc(2, 4, 2, "completed"), turn(20, "question"), 5),
            Change::Waiting("question")
        ));
        assert!(matches!(
            watch.observe(osc(2, 4, 2, "completed"), turn(20, "question"), 5),
            Change::None
        ));
    }

    #[test]
    fn interruption_wins_across_different_observations_and_expires_on_new_work() {
        let mut watch = Watch::default();
        assert!(matches!(
            watch.observe(osc(1, 1, 1, "working"), turn(10, "working"), 0),
            Change::None
        ));
        assert!(matches!(
            watch.observe(osc(1, 2, 1, "interrupted"), turn(10, "working"), 0),
            Change::None
        ));
        let mut late = osc(1, 2, 1, "interrupted");
        late.finished_submission = 1;
        assert!(matches!(
            watch.observe(late, turn(10, "completed"), 0),
            Change::None
        ));
        assert!(matches!(
            watch.observe(osc(2, 4, 2, "completed"), turn(20, "completed"), 0),
            Change::Completed
        ));
        let mut other = Watch::default();
        assert!(matches!(
            other.observe(osc(1, 1, 1, "working"), turn(10, "interrupted"), 0),
            Change::None
        ));
        assert!(matches!(
            other.observe(osc(1, 2, 1, "completed"), turn(10, "interrupted"), 0),
            Change::None
        ));
    }

    #[test]
    fn osc_cycle_disambiguates_two_completions_without_submission_or_working_sample() {
        let mut watch = Watch::default();
        assert!(matches!(
            watch.observe(osc(0, 2, 1, "completed"), None, 0),
            Change::Completed
        ));
        assert!(matches!(
            watch.observe(osc(0, 2, 1, "completed"), None, 0),
            Change::None
        ));
        assert!(matches!(
            watch.observe(osc(0, 4, 2, "completed"), None, 0),
            Change::Completed
        ));
        assert!(matches!(
            watch.observe(osc(0, 4, 2, "completed"), None, 0),
            Change::None
        ));
    }

    #[test]
    fn muted_osc_result_does_not_escape_the_bound_log_reconciliation_poll() {
        let mut watch = Watch {
            thread: Some("synthetic".into()),
            ..Watch::default()
        };
        let mut old = osc(1, 2, 1, "completed");
        old.semantic.as_mut().unwrap().observed_ms = 10;
        assert!(matches!(watch.observe(old, None, 50), Change::None));
        assert!(matches!(
            watch.observe(screen(None, 1, true), None, 50),
            Change::None
        ));
        assert!(matches!(
            watch.observe(screen(None, 1, true), turn(5, "completed"), 50),
            Change::None
        ));
    }

    #[test]
    fn question_stays_through_redraw_and_resolution_delivers_delayed_completion() {
        let mut watch = Watch::default();
        assert!(matches!(
            watch.observe(screen(Some("approval"), 0, false), turn(10, "working"), 5),
            Change::Waiting("approval")
        ));
        assert!(matches!(
            watch.observe(screen(None, 0, false), None, 5),
            Change::None
        ));
        assert!(matches!(
            watch.observe(screen(None, 0, false), None, 5),
            Change::None
        ));
        assert!(watch.prompt.is_some());
        assert!(matches!(
            watch.observe(screen(Some("approval"), 0, false), turn(10, "completed"), 5),
            Change::None
        ));
        assert!(matches!(
            watch.observe(screen(None, 0, true), turn(10, "completed"), 5),
            Change::Completed
        ));
        assert!(matches!(
            watch.observe(screen(None, 0, true), turn(20, "question"), 5),
            Change::Waiting("question")
        ));
        assert!(matches!(
            watch.observe(screen(None, 0, true), turn(20, "question"), 5),
            Change::None
        ));
        assert!(matches!(
            watch.observe(screen(None, 1, true), turn(20, "question"), 5),
            Change::Clear
        ));
        assert!(matches!(
            watch.observe(screen(None, 1, true), turn(20, "question"), 5),
            Change::None
        ));
    }

    #[test]
    fn pending_survives_dismissal_following_and_outranks_unread_results() {
        let mut state = Notifications {
            focused: true,
            selected: Some("one".into()),
            ..Notifications::default()
        };
        assert!(state
            .insert("one", "left", "completed", false, true)
            .is_none());
        state.insert("one", "left", "question", false, true);
        assert_eq!(state.entries[0].presentation, None);
        state.entries[0].dismissed = true;
        state.insert("two", "right", "completed", false, true);
        assert_eq!(state.overlay(true), Some(false));
        assert_eq!(state.overlay(false), Some(true));
        state.clear_pending("one");
        assert_eq!(state.overlay(false), Some(false));
        state.clear_pending("two");
        assert_eq!(state.entries.len(), 1);
    }

    #[test]
    fn first_binding_reconciles_an_unbound_completion_without_replaying_it() {
        let mut watch = Watch::default();
        let mut idle = screen(None, 1, true);
        idle.finished_submission = 1;
        assert!(matches!(watch.observe(idle, None, 0), Change::Completed));
        watch.thread = Some("synthetic".into());
        assert!(matches!(
            watch.observe(screen(None, 1, true), turn(5, "completed"), 0),
            Change::None
        ));
        assert!(matches!(
            watch.observe(screen(None, 2, true), turn(8, "completed"), 0),
            Change::Completed
        ));
    }

    #[test]
    fn enable_time_discards_a_muted_turn_even_if_the_log_appears_later() {
        let mut watch = Watch::default();
        assert!(matches!(
            watch.observe(screen(None, 0, false), None, 25),
            Change::None
        ));
        assert!(matches!(
            watch.observe(screen(None, 0, true), turn(12, "completed"), 25),
            Change::None
        ));
        assert!(matches!(
            watch.observe(screen(None, 1, true), turn(35, "completed"), 25),
            Change::Completed
        ));
    }
}
