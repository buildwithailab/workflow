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

pub(crate) fn preview_content(path: &Path, bytes: &[u8]) -> Result<Value> {
    let mime = match path.extension().and_then(|e| e.to_str()) {
        Some("png") => Some("image/png"),
        Some("jpg" | "jpeg") => Some("image/jpeg"),
        Some("webp") => Some("image/webp"),
        Some("gif") => Some("image/gif"),
        _ => None,
    };
    if let Some(mime) = mime {
        Ok(json!({"image":format!("data:{mime};base64,{}",B64.encode(bytes))}))
    } else {
        Ok(
            json!({"text":std::str::from_utf8(bytes).context("İkili dosya önizlenemiyor")?,"hash":hash(bytes)}),
        )
    }
}

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
        crate::workspace_archive::slot(&core.directory, &core.session(id)?)?.join("baseline.json"),
    )?)?)
}
fn baseline_modes(core: &Core, sid: &str) -> Result<HashMap<String, u32>> {
    let file = crate::workspace_archive::slot(&core.directory, &core.session(sid)?)?
        .join("baseline-modes.json");
    let Some(bytes) = read_optional(&file)? else { return Ok(HashMap::new()); };
    let modes: HashMap<String, u32> = serde_json::from_slice(&bytes)?;
    if modes.values().any(|mode| *mode > 0o777) { bail!("Geçersiz dosya izinleri") }
    Ok(modes)
}

fn recorded_mode(record: &Value, side: &str) -> Result<Option<u32>> {
    let value = &record[format!("{side}Mode")];
    if value.is_null() { return Ok(None); }
    value.as_u64().filter(|mode| *mode <= 0o777).map(|mode| Some(mode as u32))
        .context("Geçersiz dosya izinleri")
}

fn matches_record(path: &Path, record: &Value, side: &str) -> Result<bool> {
    if json!(file_hash(path)?) != record[side] { return Ok(false); }
    // Old journals do not contain permission metadata and retain hash-only checks.
    #[cfg(unix)]
    if record.get(format!("{side}Mode")).is_some() {
        return Ok(file_mode(path)? == recorded_mode(record, side)?);
    }
    Ok(true)
}

fn file_stamp(path: &Path) -> Result<Value> {
    stamp(file_hash(path)?.as_deref(), file_mode(path)?)
}
fn stamp(digest: Option<&str>, mode: Option<u32>) -> Result<Value> {
    #[cfg(unix)]
    if let (Some(digest), Some(mode)) = (digest, mode) {
        return Ok(json!(format!("{digest}:{mode:o}")));
    }
    #[cfg(not(unix))]
    let _ = mode;
    Ok(json!(digest))
}
fn checkout(root: &Path) -> Option<String> {
    git(root, &["symbolic-ref", "--quiet", "HEAD"])
        .or_else(|_| git(root, &["rev-parse", "--verify", "HEAD"]))
        .ok()
}

fn capture_snapshot(
    source: &Path,
    mut save: impl FnMut(&[u8]) -> Result<String>,
) -> Result<(HashMap<String, String>, HashMap<String, u32>)> {
    let mut modes = HashMap::new();
    let files = scan_with(source, |relative, _| {
        let path = safe_path(source, relative)?;
        if let Some(mode) = file_mode(&path)? { modes.insert(relative.to_owned(), mode); }
        let bytes = fs::read(path)?;
        save(&bytes)
    })?;
    Ok((files, modes))
}

fn verify_snapshot(
    source: &Path,
    before: &HashMap<String, String>,
    direct: bool,
    baseline_ref: &Option<String>,
    modes: &HashMap<String, u32>,
) -> Result<()> {
    // Always re-read all included source files, including additions and deletions.
    let mut current_modes = HashMap::new();
    let current = scan_with(source, |relative, path| {
        if let Some(mode) = file_mode(path)? { current_modes.insert(relative.to_owned(), mode); }
        Ok(hash(&fs::read(path)?))
    })?;
    if current != *before || (!modes.is_empty() && current_modes != *modes)
        || (direct && checkout(source) != *baseline_ref) {
        bail!("Kaynak proje işlem sırasında değişti; yeni oturum başlatılamadı")
    }
    Ok(())
}

pub fn create_workspace(core: &Core, project: &Value, sid: &str, direct: bool) -> Result<Value> {
    let source = dunce::canonicalize(string(project, "directory")?)?;
    core.check_project_directory(&source)?;
    let baseline_ref = direct.then(|| checkout(&source)).flatten();
    let (before, modes) = capture_snapshot(&source, |bytes| store(core, bytes))?;
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
        if !direct {
            for (rel, digest) in &before {
                // Copy the captured bytes, not a later revision of the source.
                let bytes = blob(core, &json!(digest))?.context("Başlangıç yedeği bulunamadı")?;
                atomic_with_mode(&safe_path(&root, rel)?, &bytes, modes.get(rel).copied())?;
            }
            for rel in scan(&root)?.keys() {
                if !before.contains_key(rel) {
                    fs::remove_file(safe_path(&root, rel)?)?;
                }
            }
        }
        verify_snapshot(&source, &before, direct, &baseline_ref, &modes)?;
        if !modes.is_empty() {
            atomic(&slot.join("baseline-modes.json"), &serde_json::to_vec(&modes)?)?;
        }
        atomic(&slot.join("baseline.json"), &serde_json::to_vec(&before)?)
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
pub(crate) fn discard_new_workspace(core: &Core, session: &Value) -> Result<()> {
    let sid = string(session, "id")?;
    uuid::Uuid::parse_str(sid)?;
    let workspaces = core.directory.join("workspaces");
    let slot = safe_path(&workspaces, sid)?;
    if session["workspaceKind"] == "worktree" {
        let directory = slot.join("files");
        let root = external_path(&directory).to_string_lossy();
        let branch = string(session, "branch")?;
        let project = core.project(string(session, "projectId")?)?;
        let source = Path::new(string(&project, "directory")?);
        git(source, &["worktree", "remove", "--force", "--", &root])?;
        git(source, &["branch", "-D", "--", branch])?;
    }
    fs::remove_dir_all(slot)?;
    Ok(())
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
pub(crate) fn diff(before: Option<&[u8]>, after: Option<&[u8]>) -> Value {
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
    let lines = d.iter_all_changes().enumerate().map(|(index, change)| {
        json!({
            "index": index,
            "tag": match change.tag() {
                ChangeTag::Equal => "equal",
                ChangeTag::Delete => "delete",
                ChangeTag::Insert => "insert",
            },
            "old": change.old_index().map(|line| line + 1),
            "new": change.new_index().map(|line| line + 1),
            "text": change.value(),
            "noNewline": !change.value().ends_with('\n'),
        })
    }).collect::<Vec<_>>();
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
    let (root, b, current) = comparison(core, args)?;
    let modes = baseline_modes(core, string(args, "sessionId")?)?;
    let mut names = b
        .as_object()
        .unwrap()
        .keys()
        .chain(current.keys())
        .cloned()
        .collect::<Vec<_>>();
    names.sort();
    names.dedup();
    let mut changes = Vec::new();
    for name in names {
        let old = b[&name].as_str();
        let new = current.get(&name).map(String::as_str);
        let before_mode = modes.get(&name).copied();
        let after_mode = file_mode(&safe_path(&root, &name)?)?;
        if old == new && !(cfg!(unix) && before_mode.is_some() && before_mode != after_mode) {
            continue;
        }
        changes.push(json!({
            "path":name, "status":if old.is_none(){"A"}else if new.is_none(){"D"}else{"M"},
            "before":old, "after":new, "beforeMode":before_mode, "afterMode":after_mode,
        }));
    }
    if args["reviewStats"] == true {
        // Keep totals optional and bound the extra work on large changesets.
        let mut remaining_bytes = 2 * 1024 * 1024;
        for item in changes.iter_mut().take(100) {
            let name = item["path"].as_str().unwrap();
            let sizes = [
                item["before"].as_str().and_then(|key| {
                    fs::metadata(core.directory.join("objects").join(key)).ok()
                }),
                safe_path(&root, name).ok().and_then(|path| fs::metadata(path).ok()),
            ];
            let size: u64 = sizes.iter().flatten().map(fs::Metadata::len).sum();
            if size > 256 * 1024 || size > remaining_bytes {
                continue;
            }
            remaining_bytes -= size;
            let before = blob(core, &item["before"]).ok().flatten();
            let after = safe_path(&root, name).ok()
                .and_then(|path| read_optional(&path).ok()).flatten();
            if before.as_deref().map(hash).as_deref() != item["before"].as_str()
                || after.as_deref().map(hash).as_deref() != item["after"].as_str()
            {
                continue;
            }
            if let (Ok(before), Ok(after)) = (
                std::str::from_utf8(before.as_deref().unwrap_or_default()),
                std::str::from_utf8(after.as_deref().unwrap_or_default()),
            ) {
                if before.contains('\0') || after.contains('\0') {
                    continue;
                }
                let changes = TextDiff::from_lines(before, after);
                let mut added = 0;
                let mut removed = 0;
                for line in changes.iter_all_changes() {
                    match line.tag() {
                        ChangeTag::Insert => added += 1,
                        ChangeTag::Delete => removed += 1,
                        ChangeTag::Equal => {}
                    }
                }
                item["stats"] = json!({"added":added,"removed":removed});
            }
        }
    }
    Ok(json!(changes))
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
                let name = l.file_name().to_str().context("Dosya adı UTF-8 olarak işlenemiyor")?.to_owned();
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
            preview_content(&p, &fs::read(&p)?)
        }
        "edit_read" => editor::read(core, args),
        "edit_save" => editor::save(core, args),
        "changes" => list_changes(core, args),
        "diff" => {
            let (root, b, _) = comparison(core, args)?;
            let path = string(args, "path")?;
            let before = blob(core, &b[path])?;
            let p = safe_path(&root, path)?;
            let mode = file_mode(&p)?;
            let after = read_optional(&p)?;
            let mut d = diff(before.as_deref(), after.as_deref());
            d["stamp"] = stamp(after.as_deref().map(hash).as_deref(), mode)?;
            d["beforeMode"] = json!(baseline_modes(core, string(args, "sessionId")?)?.get(path));
            d["afterMode"] = json!(mode);
            Ok(d)
        }
        "restore" => {
            let initial = core.root(args)?;
            let repository = core.repository_operations.lock(&[initial.clone()])?;
            repository.validate(&[initial.clone()])?;
            let _guard = core.operations.lock().unwrap();
            if core.root(args)? != initial { bail!(crate::repository_operations::CHANGED); }
            let (root, b, _) = comparison(core, args)?;
            require_restore_checkout(core, args, &root)?;
            let name = string(args, "path")?;
            let p = safe_path(&root, name)?;
            let current = read_optional(&p)?;
            let actual = current.as_ref().map(|b| hash(b));
            let before_mode = file_mode(&p)?;
            if file_stamp(&p)? != args["stamp"] {
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
            let after_mode = if desired.is_none() { None }
                else if args["indices"].is_array() { before_mode }
                else { baseline_modes(core, string(args, "sessionId")?)?.get(name).copied().or(before_mode) };
            let backup = current
                .as_deref()
                .map(|bytes| store(core, bytes))
                .transpose()?;
            let token = id();
            let record = json!({"id":token,"sessionId":args["sessionId"],"root":root,"path":name,"before":backup,"after":desired.as_deref().map(hash),"beforeMode":before_mode,"afterMode":after_mode});
            let journal = core.directory.join("undo").join(format!("{token}.json"));
            // The before/after hashes also let undo recognize an interrupted restore.
            atomic(&journal, &serde_json::to_vec(&record)?)?;
            let restored = (|| {
                require_restore_checkout(core, args, &root)?;
                if file_hash(&p)? != actual || file_mode(&p)? != before_mode {
                    bail!("Dosya önizlemeden sonra değişti; geri alınmadı")
                }
                apply_bytes_mode(&p, desired.as_deref(), after_mode)
            })();
            if let Err(error) = restored {
                let _ = fs::remove_file(journal);
                return Err(error);
            }
            Ok(record)
        }
        "undo_restore" => {
            let initial = core.root(args)?;
            let repository = core.repository_operations.lock(&[initial.clone()])?;
            repository.validate(&[initial.clone()])?;
            let _guard = core.operations.lock().unwrap();
            if core.root(args)? != initial { bail!(crate::repository_operations::CHANGED); }
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
            let before = matches_record(&p, &r, "before")?;
            if matches_record(&p, &r, "after")? && !before {
                apply_bytes_mode(&p, blob(core, &r["before"])?.as_deref(), recorded_mode(&r, "before")?)?;
            } else if !before {
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
            let modes = baseline_modes(core, string(args, "sessionId")?)?;
            let mut out = vec![];
            for mut item in changes.as_array().unwrap().clone() {
                let name = item["path"].as_str().unwrap();
                let actual = file_hash(&safe_path(&main, name)?)?;
                let actual_mode = file_mode(&safe_path(&main, name)?)?;
                let desired = current.get(name);
                let same_baseline = actual.as_deref() == b[name].as_str()
                    && (!cfg!(unix) || !modes.contains_key(name) || actual_mode == modes.get(name).copied());
                let same_desired = actual.as_ref() == desired
                    && (!cfg!(unix) || json!(actual_mode) == item["afterMode"]);
                item["conflict"] = json!(!same_baseline && !same_desired);
                item["main"] = json!(actual);
                item["mainMode"] = json!(actual_mode);
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
    apply_bytes_mode(p, bytes, None)
}
pub(crate) fn apply_bytes_mode(p: &Path, bytes: Option<&[u8]>, mode: Option<u32>) -> Result<()> {
    if let Some(bytes) = bytes {
        atomic_with_mode(p, bytes, mode)
    } else {
        match fs::remove_file(p) {
            Ok(_) => Ok(()),
            Err(l) if l.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(l) => Err(l.into()),
        }
    }
}
fn transfer(core: &Core, args: &Value, undo: bool) -> Result<Value> {
    let initial_root = core.root(args)?;
    let initial_session = core.session(string(args, "sessionId")?)?;
    let initial_project = core.project(string(&initial_session, "projectId")?)?;
    let initial_main = PathBuf::from(string(&initial_project, "directory")?);
    let paths = [initial_root.clone(), initial_main.clone()];
    let repository = core.repository_operations.lock(&paths)?;
    repository.validate(&paths)?;
    let _guard = core.operations.lock().unwrap();
    let sid = string(args, "sessionId")?;
    let session = core.session(sid)?;
    let project = core.project(string(&session, "projectId")?)?;
    let main = PathBuf::from(string(&project, "directory")?);
    let root = core.root(args)?;
    if root != initial_root || main != initial_main { bail!(crate::repository_operations::CHANGED); }
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
        let modes = baseline_modes(core, sid)?;
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
            let from_mode = file_mode(&dst)?;
            let to_mode = file_mode(&src)?;
            if json!(from_hash) != file["main"] || json!(to_hash) != file["after"] {
                bail!("Dosya aktarım önizlemesinden sonra değişti: {name}")
            }
            #[cfg(unix)]
            if (file.get("mainMode").is_some() && json!(from_mode) != file["mainMode"])
                || (file.get("afterMode").is_some() && json!(to_mode) != file["afterMode"]) {
                bail!("Dosya izinleri aktarım önizlemesinden sonra değişti: {name}")
            }
            let same_baseline = from_hash.as_deref() == b[name].as_str()
                && (!cfg!(unix) || !modes.contains_key(name) || from_mode == modes.get(name).copied());
            let same_desired = from_hash == to_hash && from_mode == to_mode;
            let conflict = !same_baseline && !same_desired;
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
            let after_mode = if to.is_none() { None }
                else if file["resolution"] == "manual" { from_mode.or(to_mode) }
                else { to_mode };
            let before = from.map(|b| store(core, &b)).transpose()?;
            let after = to.map(|b| store(core, &b)).transpose()?;
            steps.push(json!({"path":name,"before":before,"after":after,"beforeMode":from_mode,"afterMode":after_mode}));
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
        let path = safe_path(&main, string(step, "path")?)?;
        if matches_record(&path, step, to)? && recovering {
            continue;
        }
        if !matches_record(&path, step, from)? {
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
            if matches_record(&p, step, to)? && recovering {
                continue;
            }
            if !matches_record(&p, step, from)? {
                bail!("Ana proje aktarım sırasında değişti: {}", step["path"])
            }
            apply_bytes_mode(&p, blob(core, &step[to])?.as_deref(), recorded_mode(step, to)?)?;
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
                if !matches_record(&p, old, to)? {
                    bail!("Ana proje geri alma sırasında değişti")
                }
                apply_bytes_mode(&p, blob(core, &old[from])?.as_deref(), recorded_mode(old, from)?)
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

    fn capture_baseline(source: &Path, save: impl FnMut(&[u8]) -> Result<String>)
        -> Result<HashMap<String, String>> {
        Ok(capture_snapshot(source, save)?.0)
    }
    fn verify_baseline(source: &Path, files: &HashMap<String, String>, direct: bool,
        reference: &Option<String>) -> Result<()> {
        verify_snapshot(source, files, direct, reference, &HashMap::new())
    }

    #[test]
    fn invalid_permission_metadata_fails_closed_and_legacy_records_remain_readable() {
        assert_eq!(recorded_mode(&json!({"before":null}), "before").unwrap(), None);
        assert_eq!(recorded_mode(&json!({"beforeMode":493}), "before").unwrap(), Some(0o755));
        for invalid in [json!(4095), json!(-1), json!("755"), json!({})] {
            assert!(recorded_mode(&json!({"beforeMode":invalid}), "before").is_err());
        }
    }

    #[cfg(unix)]
    #[test]
    fn executable_snapshot_copies_restore_and_undo_preserve_modes() {
        use std::os::unix::fs::PermissionsExt;
        let directory = SnapshotDirectory::new();
        let root = &directory.0;
        let source = root.join("run.sh");
        fs::write(&source, b"synthetic executable\n").unwrap();
        fs::set_permissions(&source, fs::Permissions::from_mode(0o750)).unwrap();
        let mut objects = HashMap::new();
        let (files, modes) = capture_snapshot(root, |bytes| {
            let digest = hash(bytes);
            objects.insert(digest.clone(), bytes.to_vec());
            Ok(digest)
        }).unwrap();
        verify_snapshot(root, &files, false, &None, &modes).unwrap();
        let target = root.join("copied.sh");
        let original = &objects[&files["run.sh"]];
        apply_bytes_mode(&target, Some(original), modes.get("run.sh").copied()).unwrap();
        assert_eq!(file_mode(&target).unwrap(), Some(0o750));
        atomic(&target, b"edited executable\n").unwrap();
        assert_eq!(file_mode(&target).unwrap(), Some(0o750));
        let record = json!({"before":hash(original),"beforeMode":0o750,
            "after":hash(b"edited executable\n"),"afterMode":0o640});
        fs::set_permissions(&target, fs::Permissions::from_mode(0o640)).unwrap();
        assert!(matches_record(&target, &record, "after").unwrap());
        apply_bytes_mode(&target, Some(original), recorded_mode(&record, "before").unwrap()).unwrap();
        assert!(matches_record(&target, &record, "before").unwrap());
        apply_bytes_mode(&target, None, None).unwrap();
        apply_bytes_mode(&target, Some(original), recorded_mode(&record, "before").unwrap()).unwrap();
        assert_eq!(file_mode(&target).unwrap(), Some(0o750));
    }

    #[cfg(unix)]
    #[test]
    fn chmod_after_capture_or_preview_is_rejected_without_changing_content() {
        use std::os::unix::fs::PermissionsExt;
        let directory = SnapshotDirectory::new();
        let file = directory.0.join("run.sh");
        fs::write(&file, b"unchanged\n").unwrap();
        fs::set_permissions(&file, fs::Permissions::from_mode(0o755)).unwrap();
        let (files, modes) = capture_snapshot(&directory.0, |bytes| Ok(hash(bytes))).unwrap();
        let stamp = file_stamp(&file).unwrap();
        let record = json!({"before":files["run.sh"], "beforeMode":0o755});
        fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(verify_snapshot(&directory.0, &files, false, &None, &modes).is_err());
        assert_ne!(stamp, file_stamp(&file).unwrap());
        assert!(!matches_record(&file, &record, "before").unwrap());
        assert!(matches_record(&file, &json!({"before":files["run.sh"]}), "before").unwrap());
        assert_eq!(fs::read(&file).unwrap(), b"unchanged\n");
    }

    struct SnapshotDirectory(PathBuf);
    impl SnapshotDirectory {
        fn new() -> Self {
            let root = std::env::temp_dir().join(id());
            fs::create_dir(&root).unwrap();
            Self(dunce::canonicalize(root).unwrap())
        }
    }
    impl Drop for SnapshotDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn snapshot_hashes_and_backups_share_one_capture_per_included_file() {
        let directory = SnapshotDirectory::new();
        let root = &directory.0;
        fs::create_dir(root.join("nested")).unwrap();
        fs::create_dir(root.join("node_modules")).unwrap();
        fs::write(root.join(".workflowignore"), "ignored.txt\n").unwrap();
        fs::write(root.join("ignored.txt"), "ignored content").unwrap();
        fs::write(root.join("node_modules/generated.txt"), "generated content").unwrap();
        fs::write(root.join("nested/data.bin"), [0, 255, 42]).unwrap();
        fs::write(root.join("empty.txt"), []).unwrap();
        let mut saves = 0;
        let mut objects = HashMap::new();
        let baseline = capture_baseline(root, |bytes| {
            saves += 1;
            let digest = hash(bytes);
            objects.insert(digest.clone(), bytes.to_vec());
            Ok(digest)
        }).unwrap();
        assert_eq!(saves, 3);
        assert_eq!(baseline, scan(root).unwrap());
        for (relative, digest) in &baseline {
            assert_eq!(objects[digest], fs::read(root.join(relative)).unwrap());
        }
        assert!(!baseline.contains_key("ignored.txt"));
        assert!(!baseline.contains_key("node_modules/generated.txt"));
        verify_baseline(root, &baseline, false, &None).unwrap();
    }

    #[test]
    fn final_verification_rejects_edits_additions_and_deletions_after_capture() {
        for change in ["edit", "add", "delete"] {
            let directory = SnapshotDirectory::new();
            let root = &directory.0;
            let file = root.join("source.txt");
            fs::write(&file, "first bytes").unwrap();
            let mut backup = Vec::new();
            let baseline = capture_baseline(root, |bytes| {
                backup = bytes.to_vec();
                if change == "edit" {
                    // Same-length edits during persistence must not be hidden by metadata.
                    fs::write(&file, "other bytes")?;
                }
                Ok(hash(bytes))
            }).unwrap();
            assert_eq!(backup, b"first bytes");
            assert_eq!(baseline["source.txt"], hash(&backup));
            match change {
                "add" => fs::write(root.join("new.txt"), "new bytes").unwrap(),
                "delete" => fs::remove_file(&file).unwrap(),
                _ => {}
            }
            assert!(verify_baseline(root, &baseline, false, &None).is_err());
        }
    }

    #[test]
    fn snapshot_failures_and_file_limits_never_return_a_valid_baseline() {
        let directory = SnapshotDirectory::new();
        let file = directory.0.join("source.txt");
        fs::write(&file, "synthetic bytes").unwrap();
        assert!(capture_baseline(&directory.0, |_| bail!("backup unavailable")).is_err());
        fs::OpenOptions::new().write(true).open(file).unwrap()
            .set_len(128 * 1024 * 1024 + 1).unwrap();
        let mut calls = 0;
        assert!(capture_baseline(&directory.0, |bytes| {
            calls += 1;
            Ok(hash(bytes))
        }).is_err());
        assert_eq!(calls, 0);
    }

    #[test]
    fn final_verification_keeps_the_direct_workspace_branch_guard() {
        let directory = SnapshotDirectory::new();
        git(&directory.0, &["init", "-b", "snapshot-start"]).unwrap();
        fs::write(directory.0.join("source.txt"), "synthetic bytes").unwrap();
        let reference = checkout(&directory.0);
        assert!(reference.is_some());
        let baseline = capture_baseline(&directory.0, |bytes| Ok(hash(bytes))).unwrap();
        verify_baseline(&directory.0, &baseline, true, &reference).unwrap();
        git(&directory.0, &["symbolic-ref", "HEAD", "refs/heads/snapshot-other"]).unwrap();
        assert!(verify_baseline(&directory.0, &baseline, true, &reference).is_err());
    }

    #[test]
    fn line_diff_keeps_indices() {
        let v = diff(Some(b"a\nb\n"), Some(b"a\nc\n"));
        assert!(v["patch"].as_str().unwrap().contains("-b"));
        assert_eq!(v["lines"][1]["tag"], "delete");
        assert_eq!(v["lines"][2]["tag"], "insert");
    }

    #[test]
    fn line_diff_marks_a_missing_final_newline() {
        let v = diff(Some(b"same\r\n"), Some(b"same"));
        assert_eq!(v["lines"][0]["noNewline"], false);
        assert_eq!(v["lines"][1]["noNewline"], true);
    }
}
