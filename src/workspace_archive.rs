use crate::core::{external_path, git, is_link, safe_path, string, Core};
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::{
    fs,
    path::{Component, Path, PathBuf},
};

pub(crate) fn slot(data: &Path, session: &Value) -> Result<PathBuf> {
    slot_path(
        data,
        string(session, "id")?,
        session["archivedFromDirectory"].is_string(),
    )
}

fn slot_path(data: &Path, id: &str, archived: bool) -> Result<PathBuf> {
    let id = uuid::Uuid::parse_str(id)?;
    safe_path(
        data,
        &format!("{}/{id}", if archived { "archive" } else { "workspaces" }),
    )
}

fn journal(data: &Path, id: &str) -> Result<PathBuf> {
    let id = uuid::Uuid::parse_str(id)?;
    safe_path(data, &format!("archive-transactions/{id}"))
}

fn isolated(session: &Value) -> bool {
    matches!(session["workspaceKind"].as_str(), Some("copy" | "worktree"))
}

pub(crate) fn directory_inside_slot(
    data: &Path,
    id: &str,
    archived: bool,
    expected: &Path,
    directory: &str,
) -> Result<PathBuf> {
    let data_slot = if archived { "archive" } else { "workspaces" };
    let original_spelling = data.join(data_slot).join(id).join("files");
    let worktree = expected.join("files");
    let directory = external_path(Path::new(directory));
    if !directory.is_absolute() || directory.components().any(|part| part == Component::ParentDir) {
        bail!("Oturum klasörü Workflow çalışma alanında değil.");
    }
    let relative = directory
        .strip_prefix(external_path(&worktree))
        .or_else(|_| directory.strip_prefix(external_path(&original_spelling)))
        .map(Path::to_path_buf)
        .or_else(|_| -> Result<PathBuf> {
            let canonical_data = dunce::canonicalize(data)?;
            for files in directory.ancestors() {
                let Some(slot) = files.parent() else { continue; };
                let Some(bucket) = slot.parent() else { continue; };
                let Some(profile) = bucket.parent() else { continue; };
                if files.file_name() == Some(std::ffi::OsStr::new("files"))
                    && slot.file_name() == Some(std::ffi::OsStr::new(id))
                    && bucket.file_name() == Some(std::ffi::OsStr::new(data_slot))
                    && dunce::canonicalize(profile)? == canonical_data
                {
                    // The target may not exist during unarchive or cleanup
                    // recovery. Prove its exact registered path from the real
                    // profile root, never from an arbitrary surviving ancestor.
                    return Ok(directory.strip_prefix(files)?.to_path_buf());
                }
            }
            // Existing journals may retain another spelling of the same real
            // directory across launches. This fallback requires existing files.
            let root = dunce::canonicalize(&worktree)?;
            Ok(dunce::canonicalize(&directory)?.strip_prefix(root)
                .context("Oturum klasörü Workflow çalışma alanında değil.")?.to_path_buf())
        })?;
    if !relative
        .components()
        .all(|part| matches!(part, Component::Normal(_)))
    {
        bail!("Oturum klasörü Workflow çalışma alanında değil.");
    }
    Ok(relative)
}

#[cfg(all(test, unix))]
mod path_tests {
    use super::*;
    #[test]
    fn aliases_require_the_exact_registered_profile_bucket_and_id_even_for_missing_targets() {
        let root = std::env::temp_dir().join(format!("workflow-archive-alias-{}", uuid::Uuid::new_v4()));
        let data = root.join("data");
        fs::create_dir_all(&data).unwrap();
        let alias = root.join("profile-alias");
        std::os::unix::fs::symlink(&data, &alias).unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        let expected = safe_path(&data, &format!("workspaces/{id}")).unwrap();
        for (profile, recorded) in [(&data, &alias), (&alias, &data)] {
            let recorded = recorded.join("workspaces").join(&id).join("files");
            assert_eq!(directory_inside_slot(profile, &id, false, &expected, recorded.to_str().unwrap()).unwrap(), PathBuf::new());
            assert_eq!(directory_inside_slot(profile, &id, false, &expected, recorded.join("nested").to_str().unwrap()).unwrap(), PathBuf::from("nested"));
            assert!(directory_inside_slot(profile, &id, false, &expected, recorded.join("../files").to_str().unwrap()).is_err());
        }
        for recorded in [alias.join("archive").join(&id).join("files"), alias.join("workspaces").join(uuid::Uuid::new_v4().to_string()).join("files"), root.join("other/workspaces").join(&id).join("files")] {
            assert!(directory_inside_slot(&data, &id, false, &expected, recorded.to_str().unwrap()).is_err());
        }
        assert!(directory_inside_slot(&data, &id, false, &expected, "relative/workspaces/files").is_err());
        fs::remove_dir_all(root).unwrap();
    }
}

struct GitWorktree<'a> {
    project: &'a Path,
    admin: PathBuf,
}

impl<'a> GitWorktree<'a> {
    fn new(project: &'a Path, root: &Path) -> Result<Self> {
        let gitfile = fs::symlink_metadata(root.join(".git"))?;
        if !gitfile.is_file() || is_link(&gitfile) {
            bail!("Oturumun Git worktree bağlantısı doğrulanamadı.");
        }
        let common = git(project, &["rev-parse", "--git-common-dir"])?;
        let registry = dunce::canonicalize(project.join(common).join("worktrees"))?;
        let admin = dunce::canonicalize(git(root, &["rev-parse", "--absolute-git-dir"])?)?;
        if admin.parent() != Some(registry.as_path()) {
            bail!("Oturumun Git worktree bağlantısı doğrulanamadı.");
        }
        Ok(Self { project, admin })
    }

    fn check_movable(&self, root: &Path) -> Result<()> {
        self.check_registered(root)?;
        if self.admin.join("locked").try_exists()? {
            bail!("Git worktree kilitli. Taşımadan önce kilidi kaldır.");
        }
        // Git does not support moving worktrees with populated submodules.
        let index = git(root, &["ls-files", "--stage", "-z"])?;
        for line in index
            .split('\0')
            .filter(|entry| entry.starts_with("160000 "))
        {
            if let Some((_, path)) = line.split_once('\t') {
                let module = safe_path(root, path)?;
                match fs::symlink_metadata(module.join(".git")) {
                    Ok(_) => bail!("Git worktree içinde başlatılmış alt modüller var. Klasör yerinde bırakıldı."),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
                    Err(error) => return Err(error.into()),
                }
            }
        }
        Ok(())
    }

    fn check_registered(&self, root: &Path) -> Result<()> {
        let gitfile = fs::read_to_string(self.admin.join("gitdir"))?;
        let registered =
            dunce::canonicalize(self.admin.join(gitfile.trim_end_matches(['\n', '\r'])))?;
        if registered != dunce::canonicalize(root.join(".git"))? {
            bail!("Oturumun Git worktree bağlantısı doğrulanamadı.");
        }
        Ok(())
    }

    fn repair(&self, root: &Path) -> Result<()> {
        if self.check_registered(root).is_err() {
            git(
                self.project,
                &["worktree", "repair", &external_path(root).to_string_lossy()],
            )?;
        }
        let worktree = dunce::canonicalize(root)?;
        if crate::git::worktree_root(root)? != worktree {
            bail!("Oturumun Git worktree bağlantısı doğrulanamadı.");
        }
        self.check_registered(root)
    }
}

// Cleanup separately scans the whole slot, rejecting populated submodules and
// nested Git repositories before asking Git to unregister this exact worktree.
pub(crate) fn cleanup_preflight(data: &Path, project: &Path, root: &Path) -> Result<PathBuf> {
    let tree = GitWorktree::new(project, root)?;
    let common = |path: &Path| -> Result<PathBuf> {
        Ok(dunce::canonicalize(
            path.join(git(path, &["rev-parse", "--git-common-dir"])?),
        )?)
    };
    let source_common = common(project)?;
    if source_common.starts_with(dunce::canonicalize(data)?)
        || common(root)? != source_common
        || tree.admin.parent() != Some(source_common.join("worktrees").as_path())
        || crate::git::worktree_root(root)? != dunce::canonicalize(root)?
    {
        bail!("Git çalışma alanının ana depo kaydı veya kilit durumu temizlemeye izin vermiyor.");
    }
    for marker in ["gitdir", "commondir"] {
        let metadata = fs::symlink_metadata(tree.admin.join(marker))?;
        if !metadata.is_file() || is_link(&metadata) || metadata.len() > 16 * 1024 {
            bail!("Git yönetim kaydı doğrulanamadı.");
        }
    }
    tree.check_registered(root)?;
    if cleanup_registration_count(project, root)? != 1 {
        bail!("Git çalışma alanı ana deponun çalışma alanları listesinde doğrulanamadı.");
    }
    match fs::symlink_metadata(tree.admin.join("locked")) {
        Ok(_) => bail!("Git çalışma alanı kilitli. Klasör korundu."),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
        Err(error) => return Err(error.into()),
    }
    Ok(tree.admin)
}

pub(crate) fn cleanup_registration_count(project: &Path, root: &Path) -> Result<usize> {
    let location = |path: &Path| -> Result<PathBuf> {
        match dunce::canonicalize(path) {
            Ok(value) => Ok(value),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(dunce::canonicalize(
                path.parent().context("Çalışma alanı üst klasörü yok.")?,
            )?
            .join(path.file_name().context("Çalışma alanı adı yok.")?)),
            Err(error) => Err(error.into()),
        }
    };
    let canonical = location(root)?;
    let entries = crate::core::git_bytes(project, &["worktree", "list", "--porcelain", "-z"])?;
    let mut matches = 0;
    for field in entries.split(|b| *b == 0) {
        if let Some(raw) = field.strip_prefix(b"worktree ") {
            #[cfg(unix)]
            let path = {
                use std::os::unix::ffi::OsStrExt;
                Path::new(std::ffi::OsStr::from_bytes(raw))
            };
            #[cfg(not(unix))]
            let path = Path::new(std::str::from_utf8(raw)?);
            // An approved slot always has the shape <UUID>/files. Do not
            // touch unrelated registered worktrees, including offline drives.
            if path.file_name() == root.file_name()
                && path.parent().and_then(Path::file_name)
                    == root.parent().and_then(Path::file_name)
                && location(path).is_ok_and(|registered| registered == canonical)
            {
                matches += 1;
            }
        }
    }
    Ok(matches)
}

pub(crate) fn cleanup_remove(data: &Path, project: &Path, root: &Path, admin: &Path) -> Result<()> {
    if cleanup_preflight(data, project, root)? != admin {
        bail!("Git çalışma alanının kaydı değişti. Önizlemeyi yenileyin.");
    }
    git(
        project,
        &[
            "worktree",
            "remove",
            "--force",
            "--",
            &external_path(root).to_string_lossy(),
        ],
    )?;
    Ok(())
}

fn worktree<'a>(
    session: &Value,
    project: &'a Value,
    current: &Path,
) -> Result<Option<GitWorktree<'a>>> {
    if session["workspaceKind"] != "worktree" {
        return Ok(None);
    }
    Ok(Some(GitWorktree::new(
        Path::new(string(project, "directory")?),
        &safe_path(current, "files")?,
    )?))
}

fn move_slot(from: &Path, to: &Path) -> Result<()> {
    if to.try_exists()? {
        bail!("Hedefte bir oturum klasörü zaten var. Klasörler korunuyor.");
    }
    fs::create_dir_all(to.parent().context("Klasör yok")?)?;
    fs::rename(from, to).context("Oturum klasörü taşınamadı.")
}

// An interrupted move is reconciled to the last saved session location.
fn reconcile(data: &Path, session: &Value, project: &Value) -> Result<()> {
    let id = string(session, "id")?;
    let pending = journal(data, id)?;
    let Some(payload) = crate::core::read_optional(&pending)? else {
        return Ok(());
    };
    let expected_git: Option<PathBuf> = serde_json::from_slice(&payload)?;
    let expected = slot(data, session)?;
    let other = slot_path(data, id, !session["archivedFromDirectory"].is_string())?;
    let current = match (expected.is_dir(), other.is_dir()) {
        (true, false) => &expected,
        (false, true) => &other,
        _ => bail!("Oturum klasörünün konumu doğrulanamadı. Klasörler korunuyor."),
    };
    let git_worktree = worktree(session, project, current)?;
    if git_worktree.as_ref().map(|git| &git.admin) != expected_git.as_ref() {
        bail!("Oturumun Git worktree bağlantısı doğrulanamadı.");
    }
    if current != &expected {
        move_slot(current, &expected)?;
    }
    if let Some(git_worktree) = git_worktree {
        git_worktree.repair(&expected.join("files"))?;
    }
    fs::remove_file(pending)?;
    Ok(())
}

pub(crate) fn recover(data: &Path, db: &Value) {
    if !data.join("archive-transactions").is_dir() {
        return;
    }
    if let (Some(sessions), Some(projects)) = (db["sessions"].as_array(), db["projects"].as_array())
    {
        for session in sessions.iter().filter(|session| isolated(session)) {
            if let Some(project) = projects
                .iter()
                .find(|project| project["id"] == session["projectId"])
            {
                if reconcile(data, session, project).is_err() {
                    eprintln!("An interrupted session archive move could not be reconciled.");
                }
            }
        }
    }
}

pub(crate) fn set(core: &Core, id: &str, mut patch: Value) -> Result<Value> {
    let session = core.session(id)?;
    let archive = patch["archived"] == true;
    let already_moved = session["archivedFromDirectory"].is_string();
    if !isolated(&session) {
        return core.update_session(id, patch);
    }
    let project = core.project(string(&session, "projectId")?)?;
    reconcile(&core.directory, &session, &project)?;
    if archive == already_moved {
        return core.update_session(id, patch);
    }
    let from = slot(&core.directory, &session)?;
    let to = slot_path(&core.directory, id, archive)?;
    let relative = directory_inside_slot(
        &core.directory,
        id,
        already_moved,
        &from,
        string(&session, "directory")?,
    )?;
    if !archive {
        let original = string(&session, "archivedFromDirectory")?;
        if directory_inside_slot(&core.directory, id, false, &to, original)? != relative {
            bail!("Oturum klasörü Workflow çalışma alanında değil.");
        }
    }
    // Historical aliases only prove the relative location. Persist the managed
    // slot itself so retargeting an old alias cannot redirect a future CLI cwd.
    let managed = |slot: &Path| {
        let root = slot.join("files");
        if relative.as_os_str().is_empty() { root } else { root.join(&relative) }
    };
    let directory = json!(external_path(&managed(&to)));
    let original = json!(external_path(&managed(&from)));
    let from_root = safe_path(&from, "files")?;
    let from_directory = safe_path(&from_root, &relative.to_string_lossy())?;
    if !from_directory.is_dir() {
        bail!("Oturumun çalışma klasörü bulunamadı.");
    }
    let git_worktree = worktree(&session, &project, &from)?;
    if let Some(git_worktree) = &git_worktree {
        git_worktree.check_movable(&from_root)?;
    }
    if to.try_exists()? {
        bail!("Hedefte bir oturum klasörü zaten var. Klasörler korunuyor.");
    }

    let pending = journal(&core.directory, id)?;
    crate::core::atomic(
        &pending,
        &serde_json::to_vec(&git_worktree.as_ref().map(|git| &git.admin))?,
    )?;
    let result = (|| {
        move_slot(&from, &to)?;
        if let Some(git_worktree) = git_worktree {
            git_worktree.repair(&to.join("files"))?;
        }
        patch["directory"] = directory;
        patch["archivedFromDirectory"] = if archive {
            original
        } else {
            Value::Null
        };
        core.update_session(id, patch)
    })();
    match result {
        Ok(saved) => {
            // If deletion fails, the same marker will be safe to replay from the saved state.
            let _ = fs::remove_file(pending);
            Ok(saved)
        }
        Err(error) => {
            reconcile(&core.directory, &session, &project).context(
                "Oturum klasörü geri alınamadı. Workflow yeniden açıldığında tekrar denenecek.",
            )?;
            Err(error)
        }
    }
}
