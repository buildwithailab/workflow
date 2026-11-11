use crate::core::*;
use anyhow::{bail, Context, Result};
use base64::{engine::general_purpose::STANDARD as B64, Engine};
use serde_json::{json, Value};
use similar::{ChangeTag, TextDiff};
use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    sync::Arc,
};
mod editor;

pub fn store(core: &Core, bytes: &[u8]) -> Result<String> {
    let digest = hash(bytes);
    let p = core.directory.join("objects").join(&digest);
    if !p.exists() {
        atomic(&p, bytes)?
    };
    Ok(digest)
}
pub fn blob(core: &Core, key: &Value) -> Result<Option<Vec<u8>>> {
    let Some(key) = key.as_str() else {
        return Ok(None);
    };
    if key.len() != 64 || !key.bytes().all(|b| b.is_ascii_hexdigit()) {
        bail!("Yedek kimliği geçersiz")
    };
    let bytes = fs::read(core.directory.join("objects").join(key))?;
    if hash(&bytes) != key {
        bail!("Yedek bütünlüğü doğrulanamadı")
    };
    Ok(Some(bytes))
}
pub fn baseline(core: &Core, id: &str) -> Result<Value> {
    Ok(serde_json::from_slice(&fs::read(
        core.directory
            .join("workspaces")
            .join(id)
            .join("baseline.json"),
    )?)?)
}
fn checkout(root: &Path) -> Option<String> {
    git(root, &["symbolic-ref", "--quiet", "HEAD"])
        .or_else(|_| git(root, &["rev-parse", "--verify", "HEAD"]))
        .ok()
}
pub fn create_workspace(core: &Core, project: &Value, sid: &str, direct: bool) -> Result<Value> {
    let source = dunce::canonicalize(string(project, "directory")?)?;
    core.check_project_directory(&source)?;
    let baseline_ref = direct.then(|| checkout(&source)).flatten();
    let before = scan(&source)?;
    let slot = core.directory.join("workspaces").join(sid);
    fs::create_dir_all(&slot)?;
    let worktree = slot.join("files");
    let repository = (!direct && git(&source, &["rev-parse", "--verify", "HEAD"]).is_ok())
        .then(|| crate::git::worktree_root(&source).ok())
        .flatten();
    let (root, kind, branch) = if direct {
        let branch = baseline_ref
            .as_deref()
            .and_then(|reference| reference.strip_prefix("refs/heads/"))
            .map(str::to_owned);
        (external_path(&source).to_path_buf(), "direct", branch)
    } else if let Some(repository) = repository {
        let subdir = source.strip_prefix(&repository)?;
        let branch = format!("workflow/session-{}", &sid[..8]);
        git(
            &repository,
            &[
                "worktree",
                "add",
                "-b",
                &branch,
                "--",
                &worktree.to_string_lossy(),
                "HEAD",
            ],
        )?;
        (worktree.join(subdir), "worktree", Some(branch))
    } else {
        (worktree.clone(), "copy", None)
    };
    let prepared = (|| -> Result<()> {
        if !direct {
            let subdir = root.strip_prefix(&worktree)?;
            fs::create_dir_all(&worktree)?;
            fs::create_dir_all(safe_path(&worktree, &subdir.to_string_lossy())?)?;
        }
        let mut saved = serde_json::Map::new();
        for (rel, digest) in &before {
            let bytes = fs::read(safe_path(&source, rel)?)?;
            if hash(&bytes) != *digest {
                bail!("Çalışma alanı hazırlanırken kaynak değişti; yeniden dene")
            }
            store(core, &bytes)?;
            saved.insert(rel.clone(), json!(digest));
            if !direct {
                atomic(&safe_path(&root, rel)?, &bytes)?;
            }
        }
        if !direct {
            for rel in scan(&root)?.keys() {
                if !before.contains_key(rel) {
                    fs::remove_file(safe_path(&root, rel)?)?;
                }
            }
        }
        if scan(&source)? != before || (direct && checkout(&source) != baseline_ref) {
            bail!("Kaynak proje işlem sırasında değişti; yeni oturum başlatılamadı")
        }
        atomic(&slot.join("baseline.json"), &serde_json::to_vec(&saved)?)
    })();
    if prepared.is_err() {
        if let Some(branch) = branch.as_deref().filter(|_| kind == "worktree") {
            git(
                &source,
                &[
                    "worktree",
                    "remove",
                    "--force",
                    "--",
                    &worktree.to_string_lossy(),
                ],
            )
            .and_then(|_| git(&source, &["branch", "-D", "--", branch]))
            .context("Tamamlanamayan oturumun Git çalışma alanı temizlenemedi")?;
        }
    }
    prepared?;
    Ok(json!({"directory":root,"kind":kind,"branch":branch,"baselineRef":baseline_ref}))
}
fn require_restore_checkout(core: &Core, args: &Value, root: &Path) -> Result<()> {
    let session = core.session(string(args, "sessionId")?)?;
    if session["workspaceKind"] == "direct"
        && checkout(root).as_deref() != session["baselineRef"].as_str()
    {
        bail!("Başlangıç kaydına göre geri almak için oturumun açıldığı Git dalına dön.")
    }
    Ok(())
}
fn diff(before: Option<&[u8]>, after: Option<&[u8]>) -> Value {
    let old = before.and_then(|b| std::str::from_utf8(b).ok());
    let new = after.and_then(|b| std::str::from_utf8(b).ok());
    if before.is_some() && old.is_none()
        || after.is_some() && new.is_none()
        || before.is_some_and(|b| b.contains(&0))
        || after.is_some_and(|b| b.contains(&0))
    {
        return json!({"binary":true});
    }
    let old = old.unwrap_or("");
    let new = new.unwrap_or("");
    let d = TextDiff::from_lines(old, new);
    let lines=d.iter_all_changes().enumerate().map(|(index,c)|json!({"index":index,"tag":match c.tag(){ChangeTag::Equal=>"equal",ChangeTag::Delete=>"delete",ChangeTag::Insert=>"insert"},"old":c.old_index().map(|n|n+1),"new":c.new_index().map(|n|n+1),"text":c.value()})).collect::<Vec<_>>();
    json!({"lines":lines,"patch":d.unified_diff().context_radius(3).to_string(),"old":old,"new":new})
}
fn comparison(core: &Core, args: &Value) -> Result<(PathBuf, Value, HashMap<String, String>)> {
    let sid = string(args, "sessionId")?;
    let root = core.root(args)?;
    let b = baseline(core, sid)?;
    let current = scan(&root)?;
    Ok((root, b, current))
}
pub fn list_changes(core: &Core, args: &Value) -> Result<Value> {
    let (_root, b, current) = comparison(core, args)?;
    let mut names = b
        .as_object()
        .unwrap()
        .keys()
        .chain(current.keys())
        .cloned()
        .collect::<Vec<_>>();
    names.sort();
    names.dedup();
    Ok(json!(names.into_iter().filter_map(|name|{let old=b[&name].as_str();let new=current.get(&name).map(|s|s.as_str());(old!=new).then(||json!({"path":name,"status":if old.is_none(){"A"}else if new.is_none(){"D"}else{"M"},"before":old,"after":new}))}).collect::<Vec<_>>()))
}
pub fn dispatch(core: &Arc<Core>, action: &str, args: &Value) -> Result<Value> {
    if matches!(
        action,
        "transfer_list" | "transfer_preview" | "transfer" | "undo_transfer"
    ) && core.session(string(args, "sessionId")?)?["workspaceKind"] == "direct"
    {
        bail!("Bu oturumun dosyaları zaten proje klasöründe.")
    }
    match action {
        "transfer_history" => {
            let sid = string(args, "sessionId")?;
            let directory = core.directory.join("transfers");
            let mut out = vec![];
            if directory.is_dir() {
                for e in fs::read_dir(directory)?.flatten() {
                    if let Ok(b) = fs::read(e.path()) {
                        if let Ok(record) = serde_json::from_slice::<Value>(&b) {
                            if record["sessionId"] == sid {
                                out.push(record)
                            }
                        }
                    }
                }
            }
            out.sort_by(|a, b| b["createdAt"].as_str().cmp(&a["createdAt"].as_str()));
            Ok(json!(out))
        }
        "list" => {
            let root = core.root(args)?;
            let rel = args["path"].as_str().unwrap_or("");
            let folder = if rel.is_empty() {
                root
            } else {
                safe_path(&root, rel)?
            };
            let mut entries = vec![];
            for l in fs::read_dir(folder)? {
                let l = l?;
                let ft = l.file_type()?;
                let name = l.file_name().to_string_lossy().to_string();
                if name == ".git" || ft.is_symlink() {
                    continue;
                }
                entries.push(json!({"name":name,"path":if rel.is_empty(){name}else{format!("{rel}/{name}")},"directory":ft.is_dir()}));
            }
            entries.sort_by(|a, b| {
                b["directory"]
                    .as_bool()
                    .cmp(&a["directory"].as_bool())
                    .then(a["name"].as_str().cmp(&b["name"].as_str()))
            });
            Ok(json!(entries))
        }
        "read" => {
            let p = safe_path(&core.root(args)?, string(args, "path")?)?;
            if fs::metadata(&p)?.len() > 5 * 1024 * 1024 {
                bail!("Önizleme sınırı 5 MB")
            };
            let bytes = fs::read(&p)?;
            let ext = p.extension().unwrap_or_default().to_string_lossy();
            let mime = match ext.as_ref() {
                "png" => Some("image/png"),
                "jpg" | "jpeg" => Some("image/jpeg"),
                "webp" => Some("image/webp"),
                "gif" => Some("image/gif"),
                _ => None,
            };
            if let Some(mime) = mime {
                Ok(json!({"image":format!("data:{mime};base64,{}",B64.encode(&bytes))}))
            } else {
                Ok(
                    json!({"text":std::str::from_utf8(&bytes).context("İkili dosya önizlenemiyor")?,"hash":hash(&bytes)}),
                )
            }
        }
        "edit_read" => editor::read(core, args),
        "edit_save" => editor::save(core, args),
        "changes" => list_changes(core, args),
        "diff" => {
            let (root, b, _) = comparison(core, args)?;
            let path = string(args, "path")?;
            let before = blob(core, &b[path])?;
            let after = read_optional(&safe_path(&root, path)?)?;
            let mut d = diff(before.as_deref(), after.as_deref());
            d["stamp"] = json!(after.as_ref().map(|b| hash(b)));
            Ok(d)
        }
        "restore" => {
            let _guard = core.operations.lock().unwrap();
            let (root, b, _) = comparison(core, args)?;
            require_restore_checkout(core, args, &root)?;
            let name = string(args, "path")?;
            let p = safe_path(&root, name)?;
            let current = read_optional(&p)?;
            let actual = current.as_ref().map(|b| hash(b));
            if json!(actual) != args["stamp"] {
                bail!("Dosya önizlemeden sonra değişti; geri alınmadı")
            };
            let original = blob(core, &b[name])?;
            let desired = if let Some(indices) = args["indices"].as_array() {
                let before = String::from_utf8(original.clone().unwrap_or_default())?;
                let after = String::from_utf8(current.clone().unwrap_or_default())?;
                let d = TextDiff::from_lines(&before, &after);
                let selected = indices.iter().filter_map(Value::as_u64).collect::<Vec<_>>();
                let mut out = String::new();
                for (i, c) in d.iter_all_changes().enumerate() {
                    let chosen = selected.contains(&(i as u64));
                    match c.tag() {
                        ChangeTag::Equal => out.push_str(c.value()),
                        ChangeTag::Insert if !chosen => out.push_str(c.value()),
                        ChangeTag::Delete if chosen => out.push_str(c.value()),
                        _ => {}
                    }
                }
                Some(out.into_bytes())
            } else {
                original
            };
            let backup = current
                .as_deref()
                .map(|bytes| store(core, bytes))
                .transpose()?;
            let token = id();
            let record = json!({"id":token,"sessionId":args["sessionId"],"root":root,"path":name,"before":backup,"after":desired.as_deref().map(hash)});
            let journal = core.directory.join("undo").join(format!("{token}.json"));
            // The before/after hashes also let undo recognize an interrupted restore.
            atomic(&journal, &serde_json::to_vec(&record)?)?;
            let restored = (|| {
                require_restore_checkout(core, args, &root)?;
                if file_hash(&p)? != actual {
                    bail!("Dosya önizlemeden sonra değişti; geri alınmadı")
                }
                apply_bytes(&p, desired.as_deref())
            })();
            if let Err(error) = restored {
                let _ = fs::remove_file(journal);
                return Err(error);
            }
            Ok(record)
        }
        "undo_restore" => {
            let _guard = core.operations.lock().unwrap();
            let token = string(args, "token")?;
            uuid::Uuid::parse_str(token)?;
            let journal = core.directory.join("undo").join(format!("{token}.json"));
            let r: Value = serde_json::from_slice(&fs::read(&journal)?)?;
            let root = core.root(args)?;
            if Path::new(string(&r, "root")?) != root
                || (r["sessionId"].is_string() && r["sessionId"] != args["sessionId"])
            {
                bail!("Geri alma çalışma alanı değişti")
            };
            require_restore_checkout(core, args, &root)?;
            let p = safe_path(&root, string(&r, "path")?)?;
            let current = json!(file_hash(&p)?);
            if current == r["after"] && current != r["before"] {
                apply_bytes(&p, blob(core, &r["before"])?.as_deref())?;
            } else if current != r["before"] {
                bail!("Dosya geri alma sonrasında değişti; korundu")
            }
            fs::remove_file(journal)?;
            Ok(json!(true))
        }
        "transfer_list" => {
            let (root, b, current) = comparison(core, args)?;
            let session = core.session(string(args, "sessionId")?)?;
            let project = core.project(string(&session, "projectId")?)?;
            let main = PathBuf::from(string(&project, "directory")?);
            let changes = list_changes(core, args)?;
            let mut out = vec![];
            for mut item in changes.as_array().unwrap().clone() {
                let name = item["path"].as_str().unwrap();
                let actual = file_hash(&safe_path(&main, name)?)?;
                let desired = current.get(name);
                item["conflict"] =
                    json!(actual.as_deref() != b[name].as_str() && actual.as_ref() != desired);
                item["main"] = json!(actual);
                out.push(item);
            }
            let _ = root;
            Ok(json!(out))
        }
        "transfer_preview" => {
            let s = core.session(string(args, "sessionId")?)?;
            let main = core.project(string(&s, "projectId")?)?;
            let p = string(args, "path")?;
            let before = read_optional(&safe_path(Path::new(string(&main, "directory")?), p)?)?;
            let after = read_optional(&safe_path(&core.root(args)?, p)?)?;
            Ok(diff(before.as_deref(), after.as_deref()))
        }
        "transfer" => transfer(core, args, false),
        "undo_transfer" => transfer(core, args, true),
        _ => bail!("Geçersiz dosya işlemi"),
    }
}
pub fn apply_bytes(p: &Path, bytes: Option<&[u8]>) -> Result<()> {
    if let Some(bytes) = bytes {
        atomic(p, bytes)
    } else {
        match fs::remove_file(p) {
            Ok(_) => Ok(()),
            Err(l) if l.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(l) => Err(l.into()),
        }
    }
}
fn transfer(core: &Core, args: &Value, undo: bool) -> Result<Value> {
    let _guard = core.operations.lock().unwrap();
    let sid = string(args, "sessionId")?;
    let session = core.session(sid)?;
    let project = core.project(string(&session, "projectId")?)?;
    let main = PathBuf::from(string(&project, "directory")?);
    let root = core.root(args)?;
    let mut record = if undo {
        let token = string(args, "token")?;
        uuid::Uuid::parse_str(token)?;
        serde_json::from_slice::<Value>(&fs::read(
            core.directory
                .join("transfers")
                .join(format!("{token}.json")),
        )?)?
    } else {
        let b = baseline(core, sid)?;
        let mut steps = vec![];
        let mut destinations = std::collections::HashSet::new();
        for file in args["files"]
            .as_array()
            .context("Aktarılacak dosyaları seç")?
        {
            let name = string(file, "path")?;
            let src = safe_path(&root, name)?;
            let dst = safe_path(&main, name)?;
            let from = read_optional(&dst)?;
            let to = read_optional(&src)?;
            let from_hash = from.as_ref().map(|b| hash(b));
            let to_hash = to.as_ref().map(|b| hash(b));
            if json!(from_hash) != file["main"] || json!(to_hash) != file["after"] {
                bail!("Dosya aktarım önizlemesinden sonra değişti: {name}")
            }
            let conflict = from_hash.as_deref() != b[name].as_str() && from_hash != to_hash;
            if file["resolution"] == "main" {
                continue;
            }
            if conflict && file["resolution"] != "session" && file["resolution"] != "manual" {
                bail!("Önce çakışmayı çöz: {name}")
            }
            if !destinations.insert(dst) {
                bail!("Dosya aktarımda birden fazla kez seçildi: {name}")
            }
            let to = if file["resolution"] == "manual" {
                Some(string(file, "text")?.as_bytes().to_vec())
            } else {
                to
            };
            let before = from.map(|b| store(core, &b)).transpose()?;
            let after = to.map(|b| store(core, &b)).transpose()?;
            steps.push(json!({"path":name,"before":before,"after":after}));
        }
        json!({"id":id(),"sessionId":sid,"root":main,"createdAt":now(),"state":"prepared","steps":steps})
    };
    if record["sessionId"] != sid || Path::new(string(&record, "root")?) != main {
        bail!("Aktarım kimliği uyuşmuyor")
    }
    let previous_state = record["state"].clone();
    let recovering = undo && matches!(previous_state.as_str(), Some("prepared" | "reverting"));
    if undo && previous_state != "applied" && !recovering {
        bail!("Bu aktarım geri alınamaz")
    }
    let steps = record["steps"].as_array().unwrap().clone();
    let (from, to) = if undo {
        ("after", "before")
    } else {
        ("before", "after")
    };
    for step in &steps {
        let current = json!(file_hash(&safe_path(&main, string(step, "path")?)?)?);
        if current == step[to] && recovering {
            continue;
        }
        if current != step[from] {
            bail!(
                "Ana projede dosya değişti; aktarım durduruldu: {}",
                step["path"]
            )
        }
        blob(core, &step[to])?;
    }
    let journal = core
        .directory
        .join("transfers")
        .join(format!("{}.json", string(&record, "id")?));
    if undo {
        record["state"] = json!("reverting");
    }
    atomic(&journal, &serde_json::to_vec_pretty(&record)?)?;
    let mut done = vec![];
    let result = (|| -> Result<()> {
        for step in &steps {
            let p = safe_path(&main, string(step, "path")?)?;
            let current = json!(file_hash(&p)?);
            if current == step[to] && recovering {
                continue;
            }
            if current != step[from] {
                bail!("Ana proje aktarım sırasında değişti: {}", step["path"])
            }
            apply_bytes(&p, blob(core, &step[to])?.as_deref())?;
            done.push(step);
        }
        record["state"] = json!(if undo { "undone" } else { "applied" });
        atomic(&journal, &serde_json::to_vec_pretty(&record)?)
    })();
    if let Err(error) = result {
        let mut restored = true;
        for old in done.into_iter().rev() {
            let rollback = (|| -> Result<()> {
                let p = safe_path(&main, string(old, "path")?)?;
                if json!(file_hash(&p)?) != old[to] {
                    bail!("Ana proje geri alma sırasında değişti")
                }
                apply_bytes(&p, blob(core, &old[from])?.as_deref())
            })();
            restored &= rollback.is_ok();
        }
        if restored {
            record["state"] = if undo {
                previous_state
            } else {
                json!("rolled_back")
            };
            if atomic(&journal, &serde_json::to_vec_pretty(&record)?).is_ok() {
                return Err(error);
            }
        }
        bail!("Aktarım tamamlanamadı: {error}. Değişiklikler bölümünden aktarımı geri almayı yeniden dene.")
    }
    Ok(record)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn line_diff_keeps_indices() {
        let v = diff(Some(b"a\nb\n"), Some(b"a\nc\n"));
        assert!(v["patch"].as_str().unwrap().contains("-b"));
        assert_eq!(v["lines"][1]["tag"], "delete");
        assert_eq!(v["lines"][2]["tag"], "insert");
    }
}
