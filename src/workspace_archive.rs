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

fn directory_inside_slot(
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
    let relative = directory
        .strip_prefix(external_path(&worktree))
        .or_else(|_| directory.strip_prefix(external_path(&original_spelling)))
        .context("Oturum klasörü Workflow çalışma alanında değil.")?;
    if !relative
        .components()
        .all(|part| matches!(part, Component::Normal(_)))
    {
        bail!("Oturum klasörü Workflow çalışma alanında değil.");
    }
    Ok(relative.to_path_buf())
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
    let directory = if archive {
        json!(external_path(&to.join("files").join(&relative)))
    } else {
        let original = string(&session, "archivedFromDirectory")?;
        if directory_inside_slot(&core.directory, id, false, &to, original)? != relative {
            bail!("Oturum klasörü Workflow çalışma alanında değil.");
        }
        json!(original)
    };
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
            session["directory"].clone()
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
