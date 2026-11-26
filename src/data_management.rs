//! Explicit local maintenance. Physical deletion is restricted to verified
//! app-owned workspace copies; source projects and account data are protected.
use crate::core::{atomic_json, id, is_link, now, safe_path, string, Core};
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use serde::Serialize;
use sha2::{Digest,Sha256};
use std::{fs, io::{Read,Write}, sync::Arc};

const RECORD_KEYS: &[&str] = &["projects", "sessions", "links", "jobs", "verifications"];
const MAX_BACKUP_BYTES: u64 = 256 * 1024 * 1024;

#[derive(Serialize)]
struct RecordView<'a> { projects:&'a Value,sessions:&'a Value,links:&'a Value,jobs:&'a Value,verifications:&'a Value }
fn record_view(db:&Value) -> RecordView<'_> {
    RecordView { projects:&db["projects"],sessions:&db["sessions"],links:&db["links"],jobs:&db["jobs"],verifications:&db["verifications"] }
}
#[derive(Default)]
struct DigestWriter { digest:Sha256, bytes:u64 }
impl Write for DigestWriter {
    fn write(&mut self,bytes:&[u8])->std::io::Result<usize> {
        self.digest.update(bytes); self.bytes=self.bytes.saturating_add(bytes.len() as u64); Ok(bytes.len())
    }
    fn flush(&mut self)->std::io::Result<()> { Ok(()) }
}
#[derive(Serialize)]
struct RecordBackup<'a> { version:u32, #[serde(rename="createdAt")] created_at:String,records:RecordView<'a> }

fn backup_records(directory:&std::path::Path,db:&Value)->Result<(String,String)> {
    let original=db["dataManagement"]["pendingCleanup"]["backup"].as_str().map(|original|->Result<String>{
        let original=uuid::Uuid::parse_str(original)?.to_string();
        let target=safe_path(directory,&format!("data-backups/{original}/records.json"))?;
        if !target.is_file() { bail!("Yarım kalan temizliğin kayıt yedeği bulunamadı. Kayıtlar korundu."); }
        Ok(original)
    }).transpose()?;
    // A retry may contain newly added records. Preserve both the original
    // pre-cleanup snapshot and a fresh complementary snapshot before deletion.
    let payload=RecordBackup{version:1,created_at:now(),records:record_view(db)};
    let mut counter=DigestWriter::default();
    serde_json::to_writer_pretty(&mut counter,&payload)?;
    if counter.bytes>MAX_BACKUP_BYTES { bail!("Veri yedeği güvenli boyut sınırını aşıyor. Kayıtlar korundu."); }
    let latest=id();
    let folder=safe_path(directory,&format!("data-backups/{latest}"))?;
    fs::create_dir_all(&folder)?;
    atomic_json(&safe_path(directory,&format!("data-backups/{latest}/records.json"))?,&payload)?;
    Ok((latest.clone(),original.unwrap_or(latest)))
}

fn revision(db: &Value) -> Result<String> {
    let mut writer=DigestWriter::default();
    serde_json::to_writer(&mut writer,&record_view(db))?;
    Ok(format!("{:x}",writer.digest.finalize()))
}

fn reset_records(db: &Value) -> Value {
    let mut next=Value::Object(db.as_object().unwrap().iter().filter(|(key,_)|!RECORD_KEYS.contains(&key.as_str()))
        .map(|(key,value)|(key.clone(),value.clone())).collect());
    for key in ["projects", "sessions", "links", "jobs"] { next[key] = json!([]); }
    next["verifications"] = json!({});
    next
}

fn validate_records(saved:RecordView<'_>) -> Result<()> {
    for value in [saved.projects,saved.sessions,saved.links,saved.jobs] {
        if !value.is_array() { bail!("Veri yedeği geçerli değil."); }
    }
    if !saved.verifications.is_null() && !saved.verifications.is_object() {
        bail!("Veri yedeği geçerli değil.");
    }
    for values in [saved.projects,saved.sessions] {
        let mut ids = std::collections::HashSet::new();
        for entry in values.as_array().unwrap() {
            let key = uuid::Uuid::parse_str(string(entry, "id")?)?;
            if !ids.insert(key) || !entry["directory"].is_string() {
                bail!("Veri yedeği geçerli değil.");
            }
        }
    }
    Ok(())
}

fn restore_records(db: &Value, mut saved: Value) -> Result<Value> {
    validate_records(record_view(&saved))?;
    let mut next = reset_records(db);
    for key in RECORD_KEYS { next[*key] = saved[*key].take(); }
    if next["verifications"].is_null() { next["verifications"] = json!({}); }
    for session in next["sessions"].as_array_mut().unwrap() {
        session["runId"] = Value::Null;
        session["state"] = json!("stopped");
    }
    crate::verification::recover(&mut next);
    Ok(next)
}

fn ensure_idle(core: &Core) -> Result<()> {
    if core.browser_opening.load(std::sync::atomic::Ordering::SeqCst)!=0 {
        bail!("Dahili tarayıcı açılıyor. Tamamlanmasını bekle.");
    }
    // Exited terminals remain here only while their final output is draining.
    if !core.terms.lock().unwrap().is_empty()
        || core.login_terminal.lock().unwrap().as_ref().is_some_and(|login| login.running())
    { bail!("Önce açık terminalleri ve hesap girişini durdur."); }
    #[cfg(any(windows, target_os = "macos"))]
    if core.previews.any_running() { bail!("Önce çalışan önizlemeleri durdur."); }
    if !core.browsers.lock().unwrap().pages.is_empty() {
        bail!("Önce açık dahili tarayıcı sekmelerini kapat.");
    }
    Ok(())
}

fn snapshot(core: &Core) -> Result<Value> {
    let db = core.db.lock().unwrap();
    let sessions = db["sessions"].as_array().context("Oturum kayıtları geçersiz")?;
    let mut output = json!({
        "projects":db["projects"].as_array().map_or(0,Vec::len),
        "sessions":sessions.len(),"archived":sessions.iter().filter(|s|s["archived"]==true).count(),
        "retainedTerminals":core.terms.lock().unwrap().len(),"revision":revision(&db)?,
        "workspaceBytes":fs::metadata(core.directory.join("workspace.json")).map_or(0,|m|m.len()),
        "lastReset":db["dataManagement"]["lastReset"],
        "pendingCleanup":db["dataManagement"]["pendingCleanup"].is_object(),
        "hasRecords":!sessions.is_empty() || db["projects"].as_array().is_some_and(|p|!p.is_empty())
    });
    drop(db);
    let folder = safe_path(&core.directory,"transcripts")?;
    let mut count = 0usize;
    let mut bytes = 0u64;
    let mut limited = false;
    match fs::read_dir(folder) {
        Ok(entries) => for (visited,entry) in entries.enumerate() {
            if visited >= 100_000 { limited = true; break; }
            let entry = entry?;
            let metadata = fs::symlink_metadata(entry.path())?;
            if metadata.is_file() && !is_link(&metadata) && entry.path().extension().is_some_and(|s|s=="ansi") {
                count += 1; bytes = bytes.saturating_add(metadata.len());
            }
        },
        Err(error) if error.kind()==std::io::ErrorKind::NotFound => (),
        Err(error) => return Err(error.into()),
    }
    output["terminalFiles"] = json!(count);
    output["terminalBytes"] = json!(bytes);
    output["limited"] = json!(limited);
    let mut managed=0usize;
    for bucket in ["workspaces","archive"] {
        let path=safe_path(&core.directory,bucket)?;
        match fs::read_dir(path) {
            Ok(entries)=>for entry in entries.take(100_001) { let entry=entry?; if entry.file_type()?.is_dir() { managed+=1; } },
            Err(error) if error.kind()==std::io::ErrorKind::NotFound=>(),
            Err(error)=>return Err(error.into()),
        }
    }
    output["managedFolders"]=json!(managed);
    Ok(output)
}

pub(crate) fn dispatch(core: &Arc<Core>, action: &str, args: &Value) -> Result<Value> {
    match action {
        "status" => snapshot(core),
        "cleanup_preview" => Ok(crate::workspace_cleanup::plan(core)?.public()),
        "clear_memory" => {
            *core.chat_log.lock().unwrap() = crate::chat::LogCache::default();
            core.model_catalog.clear_idle();
            core.browsers.lock().unwrap().clear_trace_cache();
            Ok(json!(true))
        }
        "reset_layout" => {
            let _operation = core.operations.lock().unwrap();
            let mut db = core.db.lock().unwrap();
            let mut previous=Vec::new();
            for (index,session) in db["sessions"].as_array_mut().context("Oturum kayıtları geçersiz")?.iter_mut().enumerate() {
                if let Some(object)=session.as_object_mut() {
                    previous.push((index,object.remove("position"),object.remove("size")));
                }
            }
            if let Err(error)=core.save_workspace(&db) {
                for (index,position,size) in previous {
                    if let Some(position)=position { db["sessions"][index]["position"]=position; }
                    if let Some(size)=size { db["sessions"][index]["size"]=size; }
                }
                return Err(error);
            }
            drop(db);
            core.emit("workspace",core.snapshot());
            Ok(json!(true))
        }
        "reset_records" => {
            let delete_files=args["deleteWorkspaces"]==true;
            let plan=if delete_files {Some(crate::workspace_cleanup::plan(core)?)}else{None};
            let roots=plan.as_ref().map(|plan|plan.roots()).unwrap_or_default();
            let repository=if delete_files {Some(core.repository_operations.lock(&roots)?)}else{None};
            let _operation = core.operations.lock().unwrap();
            if let Some(repository)=&repository { repository.validate(&roots)?; }
            ensure_idle(core)?;
            if args["confirmation"] != "RESET" { bail!("Sıfırlama onayı gerekli."); }
            if let Some(plan)=&plan {
                if args["fileConfirmation"]!="DELETE" { bail!("Çalışma alanı dosyalarını silmek için ayrı onay gerekli."); }
                if !plan.public()["blocked"].as_array().is_some_and(Vec::is_empty) {
                    bail!("Temizlemeyi engelleyen klasörler var. Önizlemeyi incele.");
                }
                if args["cleanupFingerprint"].as_str()!=Some(plan.fingerprint().as_str()) {
                    bail!("Dosyalar veya klasör kayıtları değişti. Önizlemeyi yenileyin.");
                }
            }
            let mut db = core.db.lock().unwrap();
            if db["dataManagement"]["pendingCleanup"].is_object() && !delete_files {
                bail!("Yarım kalan dosya temizliğini önce Veri yönetiminden tamamla.");
            }
            if args["revision"].as_str() != Some(revision(&db)?.as_str()) {
                bail!("Kayıtlar değişti. Veri yönetimini yenileyip tekrar onayla.");
            }
            if db["projects"].as_array().is_some_and(Vec::is_empty)
                && db["sessions"].as_array().is_some_and(Vec::is_empty) && !delete_files {
                bail!("Sıfırlanacak kayıt yok.");
            }
            validate_records(record_view(&db))?;
            let (backup,original_backup)=backup_records(&core.directory,&db)?;
            if let Some(plan)=&plan {
                let identifiers:std::collections::HashSet<String>=plan.public()["folders"].as_array().unwrap().iter()
                    .filter_map(|folder|folder["id"].as_str().map(str::to_owned)).collect();
                let previous_management=db.get("dataManagement").cloned();
                let mut previous_flags=Vec::new();
                for (index,session) in db["sessions"].as_array_mut().unwrap().iter_mut().enumerate() {
                    if session["id"].as_str().is_some_and(|id|identifiers.contains(id)) {
                        previous_flags.push((index,session.get("workspaceCleanupPending").cloned()));
                        session["workspaceCleanupPending"]=json!(true);
                    }
                }
                db["dataManagement"]["pendingCleanup"]=json!({"backup":original_backup,"latestBackup":backup,"createdAt":now()});
                if let Err(error)=core.save_workspace(&db) {
                    if let Some(previous)=previous_management { db["dataManagement"]=previous; }
                    else { db.as_object_mut().unwrap().remove("dataManagement"); }
                    for (index,value) in previous_flags {
                        if let Some(value)=value { db["sessions"][index]["workspaceCleanupPending"]=value; }
                        else { db["sessions"][index].as_object_mut().unwrap().remove("workspaceCleanupPending"); }
                    }
                    return Err(error);
                }
                drop(db);
                let removed=crate::workspace_cleanup::remove(core,plan);
                let mut current=core.db.lock().unwrap();
                if let Err(failure)=removed {
                    for session in current["sessions"].as_array_mut().unwrap() {
                        if session["id"].as_str().is_some_and(|id|failure.completed.iter().any(|done|done==id)) {
                            session["workspaceFilesRemoved"]=json!(true);
                        }
                    }
                    core.save_workspace(&current)?;
                    drop(current);
                    core.emit("workspace",core.snapshot());
                    bail!("Dosya temizliği tamamlanamadı; bazı kopyalar silinmiş olabilir. Ana proje klasörü korundu. Veri yönetiminden önizlemeyi yenile.");
                }
                db=current;
            }
            let mut next = reset_records(&db);
            next["dataManagement"]["lastReset"] = json!({"id":backup,"originalBackup":original_backup,"createdAt":now(),"destructive":delete_files});
            next["dataManagement"]["pendingCleanup"]=Value::Null;
            core.save_workspace(&next)?;
            *db = next;
            drop(db);
            *core.chat_log.lock().unwrap() = crate::chat::LogCache::default();
            core.model_catalog.clear_idle();
            core.browsers.lock().unwrap().clear_trace_cache();
            core.pending.lock().unwrap().clear();
            crate::notifications::reset_history(core);
            core.emit("workspace",core.snapshot());
            Ok(json!({"backedUp":true,"filesRemoved":delete_files}))
        }
        "restore_records" => {
            let _operation = core.operations.lock().unwrap();
            ensure_idle(core)?;
            if args["confirmation"] != "RESTORE" { bail!("Geri yükleme onayı gerekli."); }
            let mut db = core.db.lock().unwrap();
            if db["dataManagement"]["pendingCleanup"].is_object() {
                bail!("Yarım kalan dosya temizliğini önce Veri yönetiminden tamamla.");
            }
            if db["dataManagement"]["lastReset"]["destructive"]==true {
                bail!("Çalışma alanı dosyaları silindi; bu sıfırlama geri alınamaz.");
            }
            if !db["projects"].as_array().is_some_and(Vec::is_empty)
                || !db["sessions"].as_array().is_some_and(Vec::is_empty) {
                bail!("Geri yüklemek için mevcut proje ve oturum listesi boş olmalı.");
            }
            let backup = uuid::Uuid::parse_str(string(&db["dataManagement"]["lastReset"],"id")?)?;
            let file = safe_path(&core.directory,&format!("data-backups/{backup}/records.json"))?;
            let file = fs::File::open(file)?;
            if file.metadata()?.len() > MAX_BACKUP_BYTES { bail!("Veri yedeği güvenli boyut sınırını aşıyor."); }
            let mut saved:Value = serde_json::from_reader(std::io::BufReader::with_capacity(64*1024,file.take(MAX_BACKUP_BYTES+1)))
                .context("Veri yedeği okunamadı; dosya korundu")?;
            if saved["version"] != 1 { bail!("Veri yedeği sürümü desteklenmiyor."); }
            let mut next = restore_records(&db,saved["records"].take())?;
            next["dataManagement"]["lastReset"] = Value::Null;
            core.save_workspace(&next)?;
            *db = next;
            drop(db);
            *core.chat_log.lock().unwrap() = crate::chat::LogCache::default();
            core.emit("workspace",core.snapshot());
            Ok(json!(true))
        }
        _ => bail!("Veri yönetimi işlemi bulunamadı"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cleanup_retry_preserves_original_and_new_records_in_distinct_backups() {
        let directory=std::env::temp_dir().join(format!("workflow-record-backup-{}",id()));
        fs::create_dir_all(&directory).unwrap();
        let mut db=json!({"projects":[],"sessions":[],"links":[],"jobs":[],"verifications":{},"providers":{"synthetic":true}});
        let (first,original)=backup_records(&directory,&db).unwrap();
        assert_eq!(first,original);
        let original_path=directory.join(format!("data-backups/{first}/records.json"));
        let original_bytes=fs::read(&original_path).unwrap();
        db["dataManagement"]=json!({"pendingCleanup":{"backup":first}});
        db["sessions"]=json!([{"id":id(),"directory":"synthetic-retry"}]);
        let (latest,preserved)=backup_records(&directory,&db).unwrap();
        assert_eq!(preserved,original);
        assert_ne!(latest,original);
        assert_eq!(fs::read(&original_path).unwrap(),original_bytes);
        let saved:Value=serde_json::from_reader(fs::File::open(directory.join(format!("data-backups/{latest}/records.json"))).unwrap()).unwrap();
        assert_eq!(saved["records"]["sessions"],db["sessions"]);
        assert!(saved["records"].get("providers").is_none());
        fs::remove_file(original_path).unwrap();
        assert!(backup_records(&directory,&db).is_err());
        fs::remove_dir_all(&directory).unwrap();
    }
    #[test]
    fn reset_and_restore_keep_accounts_preferences_and_templates_for_hundreds_of_sessions() {
        let project = id();
        let sessions:Vec<_>=(0..600).map(|i|json!({"id":id(),"projectId":project,"provider":(["codex","claude","copilot"][i%3]),
            "directory":"synthetic-workspace","archived":i%2==0,"runId":id(),"state":"running"})).collect();
        let db=json!({"projects":[{"id":project,"directory":"synthetic-project"}],"sessions":sessions,
            "links":[],"jobs":[],"verifications":{},"providers":{"synthetic":{}},"preferences":{"notificationsEnabled":false},"templates":{"keep":true}});
        let reset=reset_records(&db);
        assert!(reset["sessions"].as_array().unwrap().is_empty());
        for key in ["providers","preferences","templates"] { assert_eq!(reset[key],db[key]); }
        let restored=restore_records(&reset,serde_json::to_value(record_view(&db)).unwrap()).unwrap();
        assert_eq!(restored["sessions"].as_array().unwrap().len(),600);
        assert!(restored["sessions"].as_array().unwrap().iter().all(|s|s["runId"].is_null()&&s["state"]=="stopped"));
        for key in ["providers","preferences","templates"] { assert_eq!(restored[key],db[key]); }
        assert!(restore_records(&reset,json!({"projects":[],"sessions":{},"links":[],"jobs":[]})).is_err());
    }
}
