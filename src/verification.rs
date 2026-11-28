//! User initiated, session-bound verification. Provider reports are never promoted
//! to application-observed test evidence. Files stay in the private workspace DB.
use crate::core::{hash, id, now, string, Core};
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

const MAX_ITEMS: usize = 20;

fn pending(state: &Value) -> bool {
    matches!(state["phase"].as_str(), Some("preparing" | "running"))
}

// Delivery acknowledgement is metadata, not a new verification request. A
// concurrent acknowledgement must not invalidate a scan of the same scope.
fn same_request(left: &Value, right: &Value) -> bool {
    let clean=|value:&Value| {
        let mut value=value.clone();
        if let Some(fields)=value.as_object_mut() {
            for key in ["delivery","receivedOperation","revision","updatedAt"] { fields.remove(key); }
        }
        value
    };
    clean(left)==clean(right)
}

pub(crate) fn recover(db: &mut Value) {
    if let Some(states) = db["verifications"].as_object_mut() {
        for state in states.values_mut().filter(|state| pending(state)) {
            interrupt(state, "interrupted", "restarted");
        }
    }
}

fn interrupt(state: &mut Value, phase: &str, reason: &str) {
    state["phase"] = json!(phase);
    state["endedOperation"] = state["operation"].take();
    state["operation"] = Value::Null;
    state["reason"] = json!(reason);
    state["updatedAt"] = json!(now());
}

fn load(core: &Core, sid: &str) -> Value {
    core.db.lock().unwrap()["verifications"][sid].clone()
}

// Compare-and-swap includes the operation and revision, so a late scan, failed
// delivery or old callback cannot overwrite a subsequent edit/cancel/request.
fn store(core: &Core, sid: &str, previous: &Value, mut state: Value) -> Result<Value> {
    state["revision"] = json!(previous["revision"].as_u64().unwrap_or(0) + 1);
    persist(core,sid,previous,state)
}

fn persist(core: &Core, sid: &str, previous: &Value, mut state: Value) -> Result<Value> {
    let mut db = core.db.lock().unwrap();
    if db["verifications"][sid] != *previous {
        bail!("Doğrulama değişti; güncel listeyi yeniden aç.");
    }
    state["updatedAt"] = json!(now());
    db["verifications"][sid] = state.clone();
    if let Err(error) = core.save_workspace(&db) {
        db["verifications"][sid] = previous.clone();
        return Err(error);
    }
    Ok(state)
}

fn fingerprint(core: &Core, sid: &str) -> Result<String> {
    let root = core.root(&json!({"sessionId":sid}))?;
    source_digest(&root)
}

fn source_digest(root: &std::path::Path) -> Result<String> {
    let mut paths = std::collections::BTreeSet::new();
    let mut git_scope = Value::Null;
    let walker = ignore::WalkBuilder::new(root)
        .add_custom_ignore_filename(".workflowignore").add_custom_ignore_filename(".agentloomignore")
        .hidden(false).git_global(false).require_git(false)
        .filter_entry(|e| !matches!(e.file_name().to_str(), Some(".git" | "node_modules" | "target" | ".workflow" | ".agentloom" | ".env" | ".ssh" | ".aws")))
        .build();
    for entry in walker {
        let entry = entry?;
        if entry.file_type().is_some_and(|kind| kind.is_file() || kind.is_symlink()) {
            paths.insert(crate::core::relative_name(entry.path().strip_prefix(root)?)?);
            if paths.len() > 100_000 { bail!("Doğrulama kaynak sınırı aşıldı."); }
        }
    }
    // Tracked files remain in scope even if an ignore pattern now excludes them.
    if let Ok(output) = crate::core::run(root, "git", &["ls-files", "-z", "--cached", "--", "."].map(str::to_owned)) {
        if output.status.success() {
            let head = crate::core::run(root,"git", &["rev-parse".into(),"--verify".into(),"HEAD".into()])?;
            let branch = crate::core::run(root,"git", &["symbolic-ref".into(),"-q".into(),"HEAD".into()])?;
            git_scope = json!({"head":head.status.success().then(||hash(&head.stdout)),
                "branch":branch.status.success().then(||hash(&branch.stdout))});
            for path in output.stdout.split(|byte| *byte == 0).filter(|path| !path.is_empty()) {
                paths.insert(std::str::from_utf8(path)?.to_owned());
                if paths.len() > 100_000 { bail!("Doğrulama kaynak sınırı aşıldı."); }
            }
        } else if root.join(".git").exists() { bail!("Doğrulama kaynak listesi okunamadı."); }
    } else if root.join(".git").exists() { bail!("Doğrulama kaynak listesi için Git bulunamadı."); }
    if paths.len() > 100_000 { bail!("Doğrulama kaynak sınırı aşıldı."); }
    let canonical = root.canonicalize()?;
    let mut files = std::collections::BTreeMap::new();
    for relative in paths {
        let path = root.join(&relative);
        if !path.starts_with(root) || std::path::Path::new(&relative).components()
            .any(|c| !matches!(c,std::path::Component::Normal(_))) { bail!("Doğrulama kaynak yolu geçersiz."); }
        let metadata = match std::fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                files.insert(relative, json!({"deleted":true})); continue;
            }
            Err(error) => return Err(error.into()),
        };
        if metadata.file_type().is_symlink() {
            files.insert(relative, json!({"link":std::fs::read_link(&path)?}));
        } else if metadata.is_dir() && path.join(".git").exists() {
            let output = crate::core::run(&path,"git", &["rev-parse".into(),"--verify".into(),"HEAD".into()])?;
            if !output.status.success() { bail!("Alt deponun doğrulama kapsamı okunamadı."); }
            files.insert(relative,json!({"gitlink":hash(&output.stdout)}));
        } else {
            if !metadata.is_file() || metadata.len() > 128 * 1024 * 1024 || !path.canonicalize()?.starts_with(&canonical) {
                bail!("Doğrulama kaynağı güvenle okunamadı.");
            }
            files.insert(relative, json!({"hash":hash(&std::fs::read(&path)?), "mode":crate::core::file_mode(&path)?}));
        }
    }
    Ok(hash(&serde_json::to_vec(&json!({"files":files,"git":git_scope}))?))
}

fn binding(core: &Core, sid: &str) -> Result<Value> {
    let session = core.session(sid)?;
    if session["archived"] == true || session["login"] == true
        || !matches!(session["provider"].as_str(), Some("codex" | "claude" | "copilot"))
    {
        bail!("Doğrulama bu oturumda kullanılamıyor.");
    }
    Ok(json!({"conversation":session["conversationId"], "provider":session["provider"],
        "root":core.root(&json!({"sessionId":sid}))?.canonicalize()?.to_str().context("Klasör yolu okunamadı")?}))
}

fn active_run(core: &Core, sid: &str) -> Result<String> {
    let session = core.session(sid)?;
    let state = crate::terminal::chat_state(core, &session, false);
    if state.mismatch { bail!("Konuşma değişti; Terminalde kontrol et."); }
    state.run_id.context("Doğrulama için önce bu oturumun terminalini başlat.")
}

fn check_live(core: &Core, sid: &str, state: &Value) -> Result<()> {
    if state["binding"] != binding(core, sid)? || state["runId"] != active_run(core, sid)? {
        bail!("Doğrulamanın oturumu veya çalıştırması değişti.");
    }
    Ok(())
}

// Only the trusted terminal discovery path calls this while holding operations.
pub(crate) fn bind_first_conversation(core: &Core, sid: &str, run: &str) -> Result<()> {
    let previous = load(core, sid);
    if previous.is_null() || previous["runId"] != run || !previous["binding"]["conversation"].is_null()
        || core.session(sid)?["workspaceKind"] == "direct"
        || !matches!(previous["phase"].as_str(), Some("preparing" | "ready" | "running" | "reported")) {
        return Ok(());
    }
    let current = binding(core, sid)?;
    let mut first = previous["binding"].clone();
    first["conversation"] = current["conversation"].clone();
    if first == current && !current["conversation"].is_null() && active_run(core,sid)? == run {
        let mut state = previous.clone();
        state["binding"] = current;
        store(core,sid,&previous,state)?;
    }
    Ok(())
}

fn checked_state(core: &Core, sid: &str, refresh: bool) -> Result<Value> {
    let previous;
    {
        let _guard = core.operations.lock().unwrap();
        previous = load(core, sid);
        if previous.is_null() { return Ok(json!({"phase":"idle", "revision":0})); }
        if matches!(previous["phase"].as_str(), Some("stale" | "interrupted" | "error")) { return Ok(previous); }
        let timed_out = pending(&previous) && previous["startedAt"].as_str()
            .and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok())
            .is_some_and(|value| chrono::Utc::now().signed_duration_since(value).num_minutes()
                >= if previous["phase"] == "preparing" { 10 } else { 60 });
        if timed_out || check_live(core,sid,&previous).is_err() {
            let mut state = previous.clone();
            interrupt(&mut state,"interrupted",if timed_out {"expired"} else {"bindingChanged"});
            return store(core,sid,&previous,state);
        }
    }
    if !refresh { return Ok(previous); }
    let digest = fingerprint(core,sid);
    let _guard = core.operations.lock().unwrap();
    let current = load(core,sid);
    if !same_request(&current,&previous) { bail!("Doğrulama değişti; kapsamı yeniden kontrol et."); }
    let mut state = current.clone();
    if check_live(core,sid,&state).is_err() { interrupt(&mut state,"interrupted","bindingChanged"); }
    else {
        match digest {
            Ok(digest) if state["digest"] == digest => {},
            Ok(_) => interrupt(&mut state,"stale","sourceChanged"),
            Err(_) => interrupt(&mut state,"error","scopeUnavailable"),
        }
    }
    if state != current { store(core,sid,&current,state) } else { Ok(state) }
}

fn text(value: &Value, limit: usize) -> Result<String> {
    let value = value.as_str().context("Doğrulama metni gerekli.")?.trim();
    if value.is_empty() || value.chars().count() > limit
        || value.chars().any(|c| c.is_control() && !matches!(c, '\n' | '\t'))
    { bail!("Doğrulama metni boş veya çok uzun."); }
    Ok(value.to_owned())
}

fn criteria(value: &Value) -> Result<Value> {
    let items = value.as_array().context("Doğrulama adımları liste olmalı.")?;
    if items.is_empty() || items.len() > MAX_ITEMS { bail!("1–20 doğrulama adımı gerekli."); }
    let items = items.iter().map(|item| Ok(json!({"id":id(),
        "title":text(&item["title"], 180)?, "check":text(&item["check"], 2000)?})))
        .collect::<Result<Vec<_>>>()?;
    Ok(json!(items))
}

fn results(items: &Value, value: &Value) -> Result<Value> {
    let reports = value.as_array().context("Sonuçlar liste olmalı.")?;
    let items = items.as_array().context("Onaylı liste bulunamadı.")?;
    if reports.len() != items.len() { bail!("Her adım için bir sonuç gerekli; yapılamayanı blocked olarak bildir."); }
    let mut seen = std::collections::HashSet::new();
    let mut output = vec![];
    for report in reports {
        let item = string(report, "id")?;
        if !seen.insert(item) || !items.iter().any(|entry| entry["id"] == item)
            || !matches!(report["status"].as_str(), Some("passed" | "failed" | "blocked"))
        { bail!("Sonuç güncel listeyle eşleşmiyor."); }
        output.push(json!({"id":item, "status":report["status"],
            "evidence":text(&report["evidence"], 4000)?, "source":"model-report"}));
    }
    Ok(json!(output))
}

// Called under the terminal's input/operation guard, before touching the PTY.
pub(crate) fn authorize_delivery(core: &Core, sid: &str, run: &str, operation: &str) -> Result<()> {
    let state = load(core, sid);
    if !pending(&state) || state["operation"] != operation || state["runId"] != run
        || state["binding"] != binding(core, sid)?
    { bail!("Doğrulama isteği artık güncel değil."); }
    Ok(())
}

// Call only for a confirmed ordinary submission or an explicit terminal stop.
// Answering a permission card does not cancel a running verification.
pub(crate) fn invalidate(core: &Core, sid: &str, reason: &str) -> Result<()> {
    let previous = load(core, sid);
    if previous.is_null() || matches!(previous["phase"].as_str(), Some("idle" | "stale" | "interrupted" | "error"))
        { return Ok(()); }
    let mut state = previous.clone();
    interrupt(&mut state, "stale", reason);
    store(core, sid, &previous, state)?;
    Ok(())
}

pub fn dispatch(core: &Core, action: &str, args: &Value) -> Result<Value> {
    let sid = string(args, "id")?;
    binding(core, sid)?;
    match action {
        "state" => checked_state(core, sid, args["refresh"] == true),
        "prepare" | "start" => {
            let session = core.session(sid)?;
            if session["tools"] == false { bail!("Doğrulama için oturumun Workflow araç erişimini aç."); }
            let (terminal, initial_scope) = {
                let _guard = core.operations.lock().unwrap();
                (crate::terminal::chat_state(core, &core.session(sid)?, false), binding(core,sid)?)
            };
            if terminal.ticket.is_null() { bail!("Sağlayıcı hazır değil. Terminalde onay veya taslak olup olmadığına bak."); }
            let previous = load(core, sid);
            if pending(&previous) { bail!("Önce mevcut doğrulama isteğini iptal et."); }
            if args["revision"].as_u64().unwrap_or(0) != previous["revision"].as_u64().unwrap_or(0) {
                bail!("Doğrulama değişti; güncel listeyi yeniden aç.");
            }
            let digest = fingerprint(core, sid)?;
            let guard = core.operations.lock().unwrap();
            let current_session = core.session(sid)?;
            let current_terminal = crate::terminal::chat_state(core, &current_session, false);
            let scope = binding(core, sid)?;
            let mut first_scope = initial_scope.clone();
            if initial_scope["conversation"].is_null() && current_session["workspaceKind"] != "direct" {
                first_scope["conversation"] = scope["conversation"].clone();
            }
            if current_terminal.ticket.is_null() || current_terminal.ticket != terminal.ticket
                || first_scope != scope || current_session["tools"] == false
            { bail!("Doğrulama sırasında oturum veya girdi değişti; yeniden hazırla."); }
            let terminal = current_terminal;
            if action == "start" && (previous["phase"] != "ready"
                || previous["digest"] != digest || previous["binding"] != scope
                || previous["runId"] != terminal.run_id.as_deref().unwrap_or(""))
            {
                let mut stale = previous.clone();
                interrupt(&mut stale, "stale", "sourceChanged");
                store(core, sid, &previous, stale)?;
                bail!("Kaynak veya oturum değişti; doğrulamayı yeniden hazırla.");
            }
            let operation = id();
            let phase = if action == "prepare" { "preparing" } else { "running" };
            let mut state = json!({"phase":phase, "operation":operation, "delivery":"sending", "runId":terminal.run_id,
                "binding":scope, "digest":digest, "startedAt":now(), "items":[], "results":[]});
            if action == "start" { state["items"] = previous["items"].clone(); }
            let state = store(core, sid, &previous, state)?;
            drop(guard);
            // Keep this one line short enough for native CLI composer echo checks.
            // The authenticated getter supplies the actual task instructions/list.
            let prompt = format!("Workflow verification: call verification_request for operation {operation} and follow its instructions.");
            let mut request = terminal.ticket;
            request["id"] = json!(sid);
            request["text"] = json!(prompt);
            let delivery=crate::terminal::send_verification(core, &request, &operation);
            if let Err(error) = delivery {
                let _guard = core.operations.lock().unwrap();
                let current = load(core,sid);
                // First binding can legitimately change the snapshot while
                // keeping this operation. Cancel/new requests own their state.
                if pending(&current) && current["operation"] == operation && current["runId"] == state["runId"] {
                    let mut failed = current.clone();
                    interrupt(&mut failed, "error", "deliveryFailed");
                    let _ = store(core, sid, &current, failed);
                }
                return Err(error);
            }
            {
                let _guard=core.operations.lock().unwrap();
                let current=load(core,sid);
                if pending(&current) && current["operation"]==operation && current["runId"]==state["runId"]
                    && current["delivery"]!="received" {
                    let mut delivered=current.clone();
                    delivered["delivery"]=json!(match delivery.unwrap() {
                        crate::terminal::VerificationDelivery::Submitted=>"submitted",
                        crate::terminal::VerificationDelivery::Unconfirmed=>"unconfirmed",
                    });
                    persist(core,sid,&current,delivered)?;
                }
            }
            checked_state(core, sid, false)
        }
        "edit" => {
            let previous = load(core, sid);
            if args["revision"] != previous["revision"] || pending(&previous) || previous.is_null() {
                bail!("Doğrulama değişti; güncel listeyi yeniden aç.");
            }
            let mut state = previous.clone();
            state["items"] = criteria(&args["items"])?;
            state["results"] = json!([]);
            state["operation"] = Value::Null;
            // Editing stale evidence must not grant approval for changed code.
            state["phase"] = json!(if check_live(core, sid, &previous).is_ok()
                && previous["digest"] == fingerprint(core, sid)?
                && matches!(previous["phase"].as_str(), Some("ready" | "reported")) { "ready" } else { "stale" });
            store(core, sid, &previous, state)
        }
        "cancel" => {
            let _guard = core.operations.lock().unwrap();
            let previous = load(core, sid);
            if args["revision"] != previous["revision"] || !pending(&previous) {
                bail!("Doğrulama değişti; güncel listeyi yeniden aç.");
            }
            let mut state = previous.clone();
            interrupt(&mut state, "interrupted", "cancelled");
            store(core, sid, &previous, state)
        }
        _ => bail!("Geçersiz doğrulama işlemi"),
    }
}

pub(crate) fn tool(core: &Core, sid: &str, run: &str, method: &str, args: &Value) -> Result<Value> {
    {
        // The authenticated caller already consumed the delivery. Stop the
        // terminal waiter before the potentially slow full source scan.
        let _guard=core.operations.lock().unwrap();
        let initial=load(core,sid);
        let operation=string(args,"operation")?;
        if !pending(&initial) || initial["operation"]!=operation || initial["runId"]!=run {
            return Err(unavailable(&initial,run,operation));
        }
        check_live(core,sid,&initial)?;
        crate::terminal::acknowledge_verification(core,sid,run,operation);
        if initial["delivery"]!="received" {
            let mut received=initial.clone();
            received["delivery"]=json!("received");received["receivedOperation"]=json!(operation);
            persist(core,sid,&initial,received)?;
        }
    }
    let previous = checked_state(core, sid, true)?;
    if !pending(&previous) || previous["operation"] != string(args, "operation")?
        || previous["runId"] != run { return Err(unavailable(&previous,run,string(args,"operation")?)); }
    check_live(core, sid, &previous)?;
    let mut state = previous.clone();
    match method {
        "verification_request" => {
            let _guard = core.operations.lock().unwrap();
            let current=load(core,sid);
            if !same_request(&current,&previous) { bail!("Doğrulama isteği artık güncel değil."); }
            check_live(core, sid, &current)?;
            let previous=current;
            Ok(json!({"operation":previous["operation"], "phase":previous["phase"],
            "items":previous["items"], "instructions":if previous["phase"] == "preparing" {
                "Use the current conversation's requested development and project to propose 1–20 concrete, relevant acceptance checks via verification_propose, in the conversation's language. Each item needs title and check (procedure and expected result). Include applicable regression, failure and provider coverage. Do NOT run checks, modify files or start verification. Wait for the user's separate Start approval after proposing. Do not include credentials, private account data or conversation transcripts."
            } else {
                "Execute ONLY the approved items, using current project tools. Do not change implementation or the checklist. Existing provider/tool permission prompts still apply. Report every item's id, status (passed/failed/blocked), and evidence (command or action, actual outcome and limitations) via verification_report. Never claim unperformed checks passed. Reports are labelled model-reported, not independent application-observed proof. Stop if the source changes; request a fresh preparation. Do not include secrets or private account data in reports."
            }}))
        },
        "verification_propose" if previous["phase"] == "preparing" => {
            state["items"] = criteria(&args["items"])?;
            state["phase"] = json!("ready");
            state["operation"] = Value::Null;
            // Hold the same operation guard as terminal start/stop/rebind for the
            // final binding check and mutation; scans above never hold this lock.
            let _guard = core.operations.lock().unwrap();
            check_live(core, sid, &previous)?;
            let current=load(core,sid);
            if !same_request(&current,&previous) {bail!("Doğrulama isteği artık güncel değil.");}
            let state = store(core, sid, &current, state)?;
            Ok(json!({"phase":"ready", "count":state["items"].as_array().map_or(0,Vec::len)}))
        }
        "verification_report" if previous["phase"] == "running" => {
            state["results"] = results(&previous["items"], &args["results"])?;
            state["phase"] = json!("reported");
            state["operation"] = Value::Null;
            let _guard = core.operations.lock().unwrap();
            check_live(core, sid, &previous)?;
            let current=load(core,sid);
            if !same_request(&current,&previous) {bail!("Doğrulama isteği artık güncel değil.");}
            let state = store(core, sid, &current, state)?;
            Ok(json!({"phase":"reported", "count":state["results"].as_array().map_or(0,Vec::len)}))
        }
        _ => bail!("Bu doğrulama aşamasında işlem kullanılamıyor."),
    }
}

fn unavailable(state: &Value, run: &str, operation: &str) -> anyhow::Error {
    let owned = state["runId"] == run
        && (state["operation"] == operation || state["endedOperation"] == operation);
    let message = if owned { match state["reason"].as_str() {
        Some("newMessage") => "Yeni bir geliştirme mesajı gönderildiği için doğrulama isteği geçersizleşti. Doğrulamayı yeniden hazırla.",
        Some("sourceChanged") => "Kaynak dosyalar değiştiği için doğrulama isteği geçersizleşti. Doğrulamayı yeniden hazırla.",
        Some("bindingChanged" | "stopped" | "restarted") => "Doğrulamanın terminali veya konuşması değişti. Doğrulamayı yeniden hazırla.",
        Some("expired") => "Doğrulama isteğinin süresi doldu. Doğrulamayı yeniden hazırla.",
        Some("cancelled") => "Doğrulama isteği kullanıcı tarafından iptal edildi.",
        Some("scopeUnavailable") => "Doğrulama kapsamı okunamadı. Kaynaklara erişimi kontrol edip yeniden hazırla.",
        Some("deliveryFailed") => "Doğrulama mesajı gönderilemedi. Terminaldeki taslağı kontrol edip yeniden hazırla.",
        _ => "Doğrulama isteği artık güncel değil.",
    }} else { "Doğrulama isteği artık güncel değil." };
    anyhow::anyhow!(message)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn delivery_metadata_does_not_replace_request_scope_or_approval() {
        let current=json!({"phase":"preparing","operation":id(),"runId":id(),
            "binding":{"root":"synthetic","conversation":id()},"digest":id(),"items":[],"revision":3,"delivery":"sending"});
        let mut received=current.clone();
        received["delivery"]=json!("received");received["receivedOperation"]=current["operation"].clone();
        received["updatedAt"]=json!(now());received["revision"]=json!(4);
        assert!(same_request(&current,&received));
        for field in ["phase","operation","runId","binding","digest","items"] {
            let mut changed=received.clone();changed[field]=Value::Null;
            assert!(!same_request(&current,&changed),"request guard field {field}");
        }
    }
    #[test]
    fn invalidation_explains_its_cause_only_to_the_owning_operation() {
        for reason in ["newMessage","sourceChanged","expired","cancelled","scopeUnavailable","stopped"] {
            let mut state=json!({"phase":"preparing","runId":"current-run","operation":"current-operation"});
            interrupt(&mut state,"stale",reason);
            assert_eq!(state["reason"],reason);
            assert!(state["operation"].is_null());
            assert_ne!(unavailable(&state,"current-run","current-operation").to_string(),"Doğrulama isteği artık güncel değil.");
            for (run,operation) in [("different-run","current-operation"),("current-run","different-operation")] {
                assert_eq!(unavailable(&state,run,operation).to_string(),"Doğrulama isteği artık güncel değil.");
            }
        }
    }
    #[test]
    fn criteria_are_bounded_and_require_real_check_instructions() {
        for invalid in [json!([]), json!([{"title":"x","check":" "}]), json!([{"title":"\u{1b}","check":"x"}]),
            json!(vec![json!({"title":"x","check":"y"});21])] { assert!(criteria(&invalid).is_err()); }
        let a = criteria(&json!([{"title":"Build","check":"Run the project build; expect exit 0."}])).unwrap();
        assert!(a[0]["id"].as_str().is_some());
    }
    #[test]
    fn report_cannot_skip_duplicate_or_replace_approved_items() {
        let items = json!([{"id":"a"},{"id":"b"}]);
        let report = |id, status| json!({"id":id,"status":status,"evidence":"Observed output"});
        assert!(results(&items, &json!([report("a","passed")])).is_err());
        assert!(results(&items, &json!([report("a","passed"),report("a","passed")])).is_err());
        assert!(results(&items, &json!([report("a","passed"),report("c","passed")])).is_err());
        assert!(results(&items, &json!([report("a","unknown"),report("b","passed")])).is_err());
        let value = results(&items,&json!([report("a","passed"),report("b","blocked")])).unwrap();
        assert!(value.as_array().unwrap().iter().all(|r| r["source"] == "model-report"));
    }
    #[test]
    fn reopening_app_interrupts_work_without_losing_the_list() {
        for phase in ["preparing","running","ready","reported"] {
            let mut db = json!({"verifications":{"sample":{"phase":phase,"operation":"old","items":[{"title":"Build"}]}}});
            recover(&mut db);
            assert_eq!(db["verifications"]["sample"]["items"][0]["title"],"Build");
            if matches!(phase,"preparing"|"running") {
                assert_eq!(db["verifications"]["sample"]["phase"],"interrupted");
                assert!(db["verifications"]["sample"]["operation"].is_null());
            } else { assert_eq!(db["verifications"]["sample"]["phase"],phase); }
        }
    }
    #[test]
    fn digest_detects_same_size_edits_additions_deletions_and_source_bin_files() {
        let root = std::env::temp_dir().join(format!("workflow-verification-{}",id()));
        std::fs::create_dir_all(root.join("src/bin")).unwrap();
        let source = root.join("src/bin/sample.rs");
        std::fs::write(&source,"before").unwrap();
        let before = source_digest(&root).unwrap();
        assert_eq!(before,source_digest(&root).unwrap(),"hash ordering is stable");
        std::fs::write(&source,"after!").unwrap();
        let edited = source_digest(&root).unwrap();
        assert_ne!(before,edited);
        std::fs::write(root.join("new.rs"),"synthetic source").unwrap();
        assert_ne!(edited,source_digest(&root).unwrap());
        std::fs::remove_file(root.join("new.rs")).unwrap();
        assert_eq!(edited,source_digest(&root).unwrap());
        std::fs::remove_file(&source).unwrap();
        assert_ne!(edited,source_digest(&root).unwrap());
        std::fs::remove_dir_all(root).unwrap();
    }
}
