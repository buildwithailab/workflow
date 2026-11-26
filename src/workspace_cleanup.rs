//! Physical cleanup of registered, isolated Workflow session slots.
use crate::{
    core::{external_path, is_link, safe_path, string, Core},
    workspace_archive,
};
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Component, Path, PathBuf},
    time::{Duration, Instant, SystemTime},
};

const MAX_ENTRIES: usize = 1_000_000;
const MAX_DEPTH: usize = 128;
const MAX_DIRECTORY_ENTRIES: usize = 65_536;
const MAX_DURATION: Duration = Duration::from_secs(30);

#[derive(Debug, Clone)]
struct Session {
    id: String,
    name: String,
    project: String,
    directory: PathBuf,
    kind: String,
    archived: bool,
    cleanup_pending: bool,
}

#[derive(Debug)]
struct Target {
    session: Session,
    path: PathBuf,
    project: PathBuf,
    admin: Option<PathBuf>,
    files: u64,
    bytes: u64,
    digest: String,
}

#[derive(Debug)]
pub(crate) struct Plan {
    targets: Vec<Target>,
    roots: BTreeSet<PathBuf>,
    account_paths: Vec<PathBuf>,
    account_locations: BTreeSet<PathBuf>,
    blocked: Vec<Value>,
    preserved_direct_slots: usize,
    preserved_metadata_slots: usize,
}

impl Plan {
    pub(crate) fn roots(&self) -> Vec<PathBuf> {
        self.roots.iter().cloned().collect()
    }

    pub(crate) fn fingerprint(&self) -> String {
        let mut digest = Sha256::new();
        for root in &self.roots {
            field(&mut digest, root.as_os_str().as_encoded_bytes());
        }
        for location in &self.account_locations {
            field(&mut digest, location.as_os_str().as_encoded_bytes());
        }
        for target in &self.targets {
            for text in [&target.session.id, &target.session.kind, &target.digest] {
                field(&mut digest, text.as_bytes());
            }
            for path in [&target.path, &target.project, &target.session.directory] {
                field(&mut digest, path.as_os_str().as_encoded_bytes());
            }
            if let Some(admin) = &target.admin {
                field(&mut digest, admin.as_os_str().as_encoded_bytes());
            }
        }
        for issue in &self.blocked {
            field(&mut digest, issue.to_string().as_bytes());
        }
        format!("{:x}", digest.finalize())
    }

    pub(crate) fn public(&self) -> Value {
        let folders: Vec<_> = self
            .targets
            .iter()
            .map(|target| {
                json!({
                    "id":target.session.id,"name":target.session.name,"kind":target.session.kind,
                    "path":external_path(&target.path),"archived":target.session.archived,
                    "files":target.files,"bytes":target.bytes
                })
            })
            .collect();
        json!({"folders":folders,"files":self.targets.iter().map(|t|t.files).sum::<u64>(),
            "bytes":self.targets.iter().map(|t|t.bytes).sum::<u64>(),"blocked":self.blocked,
            "preservedDirectSlots":self.preserved_direct_slots,"preservedMetadataSlots":self.preserved_metadata_slots,
            "fingerprint":self.fingerprint()})
    }
}

fn field(digest: &mut Sha256, value: &[u8]) {
    digest.update((value.len() as u64).to_le_bytes());
    digest.update(value);
}

struct Budget {
    deadline: Instant,
    entries: usize,
}
impl Budget {
    fn new() -> Self {
        Self {
            deadline: Instant::now() + MAX_DURATION,
            entries: 0,
        }
    }
    fn visit(&mut self) -> Result<()> {
        self.entries += 1;
        if self.entries > MAX_ENTRIES || Instant::now() >= self.deadline {
            bail!("Klasör inceleme sınırı aşıldı. Hiçbir dosya kaldırılmadı.");
        }
        Ok(())
    }
}

#[derive(Default)]
struct Tree {
    digest: Sha256,
    files: u64,
    bytes: u64,
}

fn children(path: &Path, budget: &mut Budget) -> Result<Vec<PathBuf>> {
    let mut paths = Vec::new();
    for entry in fs::read_dir(path)? {
        budget.visit()?;
        if paths.len() >= MAX_DIRECTORY_ENTRIES {
            bail!("Tek klasördeki dosya sayısı inceleme sınırını aşıyor.");
        }
        paths.push(entry?.path());
    }
    paths.sort_unstable();
    Ok(paths)
}

fn metadata_only(slot: &Path, budget: &mut Budget) -> Result<bool> {
    for path in children(slot, budget)? {
        let meta = fs::symlink_metadata(&path)?;
        if !meta.is_file()
            || is_link(&meta)
            || !matches!(
                path.file_name().and_then(|s| s.to_str()),
                Some("baseline.json" | "baseline-modes.json")
            )
        {
            return Ok(false);
        }
    }
    Ok(true)
}

fn walk(
    slot: &Path,
    path: &Path,
    worktree: bool,
    depth: usize,
    budget: &mut Budget,
    tree: &mut Tree,
    root_metadata: &fs::Metadata,
) -> Result<()> {
    if depth > MAX_DEPTH {
        bail!("Klasör derinliği inceleme sınırını aşıyor.");
    }
    let meta = fs::symlink_metadata(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if meta.dev() != root_metadata.dev() {
            bail!("Çalışma alanında ayrı bir bağlı disk bulundu.");
        }
        tree.digest.update(meta.ino().to_le_bytes());
    }
    #[cfg(not(unix))]
    let _ = root_metadata;
    if is_link(&meta) {
        // Git and the recursive filesystem operation must see the same tree on
        // every supported platform, including Windows junctions and reparse points.
        bail!("Çalışma alanında dosya sistemi bağlantısı bulundu. Klasör korundu.");
    }
    let rel = path.strip_prefix(slot)?;
    field(&mut tree.digest, rel.as_os_str().as_encoded_bytes());
    tree.digest
        .update([meta.is_dir() as u8, meta.permissions().readonly() as u8]);
    for stamp in [
        meta.modified()?,
        meta.created().unwrap_or(SystemTime::UNIX_EPOCH),
    ] {
        let value = stamp
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default();
        tree.digest.update(value.as_nanos().to_le_bytes());
    }
    if path
        .file_name()
        .is_some_and(|s| s.eq_ignore_ascii_case(".git"))
    {
        if worktree {
            if rel != Path::new("files").join(".git") || !meta.is_file() {
                bail!("Git çalışma alanında iç içe bir depo veya başlatılmış alt modül var.");
            }
        } else if !meta.is_dir() {
            bail!("Kopyada dış Git kaydına bağlanan bir çalışma alanı var.");
        }
    }
    if meta.is_dir() {
        if path
            .file_name()
            .is_some_and(|s| s.eq_ignore_ascii_case(".git"))
            && path.join("worktrees").try_exists()?
        {
            bail!("Kopyadaki Git deposunun başka çalışma alanı kayıtları var.");
        }
        for child in children(path, budget)? {
            walk(
                slot,
                &child,
                worktree,
                depth + 1,
                budget,
                tree,
                root_metadata,
            )?;
        }
    } else if meta.is_file() {
        tree.files += 1;
        tree.bytes = tree
            .bytes
            .checked_add(meta.len())
            .context("Dosya boyutu toplamı taşma sınırında.")?;
        tree.digest.update(meta.len().to_le_bytes());
    } else {
        bail!("Çalışma alanında desteklenmeyen bir dosya türü bulundu.");
    }
    Ok(())
}

fn inspect(
    data: &Path,
    slot: &Path,
    session: &Session,
    project: &Path,
    budget: &mut Budget,
) -> Result<Target> {
    let expected = safe_path(
        data,
        &format!(
            "{}/{}",
            if session.archived {
                "archive"
            } else {
                "workspaces"
            },
            session.id
        ),
    )?;
    if expected != slot || dunce::canonicalize(slot)? != external_path(&expected) {
        bail!("Oturum klasörünün konumu doğrulanamadı.");
    }
    if !matches!(session.kind.as_str(), "copy" | "worktree") {
        bail!("Oturumun ayrı çalışma alanı türü doğrulanamadı.");
    }
    let pending = safe_path(data, &format!("archive-transactions/{}", session.id))?;
    if pending.try_exists()? {
        bail!("Oturumun arşiv taşıma işlemi tamamlanmadı.");
    }
    let files = safe_path(slot, "files")?;
    let relative = external_path(&session.directory)
        .strip_prefix(external_path(&files))
        .context("Oturumun kayıtlı çalışma klasörü yönetilen alanın dışında.")?;
    if !relative
        .components()
        .all(|c| matches!(c, Component::Normal(_)))
    {
        bail!("Oturumun kayıtlı çalışma klasörü doğrulanamadı.");
    }
    let remaining_wrapper = !files.try_exists()?;
    if remaining_wrapper {
        if !session.cleanup_pending
            || !metadata_only(slot, budget)?
            || (session.kind == "worktree"
                && workspace_archive::cleanup_registration_count(project, &files)? != 0)
        {
            bail!("Çalışma alanı dosyaları veya Git kaydı doğrulanamadı.");
        }
    } else {
        let recorded = dunce::canonicalize(&session.directory)?;
        if !recorded.is_dir()
            || dunce::canonicalize(safe_path(&files, &relative.to_string_lossy())?)? != recorded
            || !recorded.starts_with(dunce::canonicalize(&files)?)
        {
            bail!("Oturumun kayıtlı çalışma klasörü doğrulanamadı.");
        }
    }
    let mut tree = Tree::default();
    walk(
        slot,
        slot,
        session.kind == "worktree",
        0,
        budget,
        &mut tree,
        &fs::metadata(data)?,
    )?;
    let admin = if session.kind == "worktree" && !remaining_wrapper {
        Some(workspace_archive::cleanup_preflight(data, project, &files)?)
    } else {
        None
    };
    Ok(Target {
        session: session.clone(),
        path: slot.to_owned(),
        project: project.to_owned(),
        admin,
        files: tree.files,
        bytes: tree.bytes,
        digest: format!("{:x}", tree.digest.finalize()),
    })
}

fn protect_project(data: &Path, path: &Path) -> Result<Option<PathBuf>> {
    if !path.is_absolute() || path.components().any(|part| part == Component::ParentDir) {
        bail!("Ana proje konumu doğrulanamadı.");
    }
    let canonical = match dunce::canonicalize(path) {
        Ok(path) => path,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            // An old direct project can already have been removed from disk.
            // A missing source does not authorize deleting any external path.
            if external_path(path).starts_with(external_path(data))
                || data.starts_with(external_path(path))
            {
                bail!("Ana proje Workflow veri alanıyla çakışıyor.");
            }
            return Ok(None);
        }
        Err(error) => return Err(error.into()),
    };
    if !canonical.is_dir() || canonical.starts_with(data) || data.starts_with(&canonical) {
        bail!("Ana proje Workflow veri alanıyla çakışıyor.");
    }
    // A source working tree may keep its Git administration outside the
    // visible source directory. Protect that location too.
    for ancestor in canonical.ancestors() {
        let marker = ancestor.join(".git");
        let meta = match fs::symlink_metadata(&marker) {
            Ok(meta) => meta,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        if dunce::canonicalize(&marker)?.starts_with(data) {
            bail!("Ana projenin Git kayıtları Workflow veri alanında. Klasörler korundu.");
        }
        if meta.is_file() || is_link(&meta) || marker.join("commondir").try_exists()? {
            for flag in ["--absolute-git-dir", "--git-common-dir"] {
                let value = crate::core::git(&canonical, &["rev-parse", flag])?;
                if dunce::canonicalize(canonical.join(value))?.starts_with(data) {
                    bail!("Ana projenin Git kayıtları Workflow veri alanında. Klasörler korundu.");
                }
            }
        }
        break;
    }
    Ok(Some(canonical))
}

// A login can create its account directory after the preview. Resolve the
// existing prefix through filesystem aliases without opening any account file.
fn account_location(path: &Path) -> Result<PathBuf> {
    if !path.is_absolute() || path.components().any(|c| c == Component::ParentDir) {
        bail!("Sağlayıcı hesap klasörünün mutlak konumu doğrulanamadı.");
    }
    let mut existing = path;
    let mut missing = Vec::new();
    loop {
        match fs::symlink_metadata(existing) {
            Ok(_) => {
                let mut resolved = dunce::canonicalize(existing)
                    .context("Sağlayıcı hesap klasörünün bağlantısı doğrulanamadı.")?;
                if !resolved.is_dir() {
                    bail!("Sağlayıcı hesap yolu bir klasör değil.");
                }
                resolved.extend(missing.into_iter().rev());
                return Ok(resolved);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                missing.push(
                    existing
                        .file_name()
                        .context("Sağlayıcı hesap konumu doğrulanamadı.")?,
                );
                existing = existing
                    .parent()
                    .context("Sağlayıcı hesap konumu doğrulanamadı.")?;
            }
            Err(error) => return Err(error).context("Sağlayıcı hesap konumu doğrulanamadı."),
        }
    }
}

fn outside_accounts(target: &Path, locations: &BTreeSet<PathBuf>) -> Result<()> {
    let target = dunce::canonicalize(target)?;
    if locations
        .iter()
        .any(|account| target.starts_with(account) || account.starts_with(&target))
    {
        bail!("Temizlenecek klasör bir sağlayıcı hesap klasörüyle çakışıyor. Dosyalar korunuyor.");
    }
    Ok(())
}

fn account_locations(paths: &[PathBuf]) -> Result<BTreeSet<PathBuf>> {
    paths.iter().map(|path| account_location(path)).collect()
}

fn inventory(
    data: &Path,
    projects: &BTreeMap<String, PathBuf>,
    sessions: &BTreeMap<String, Session>,
    account_paths: &[PathBuf],
) -> Result<Plan> {
    if is_link(&fs::symlink_metadata(data)?) {
        bail!("Workflow veri klasörü bir dosya sistemi bağlantısı.");
    }
    let data = dunce::canonicalize(data)?;
    let mut plan = Plan {
        targets: Vec::new(),
        roots: BTreeSet::new(),
        account_paths: account_paths.to_vec(),
        account_locations: BTreeSet::new(),
        blocked: Vec::new(),
        preserved_direct_slots: 0,
        preserved_metadata_slots: 0,
    };
    match account_locations(account_paths) {
        Ok(locations) => plan.account_locations = locations,
        Err(error) => {
            plan.blocked
                .push(json!({"path":external_path(&data),"reason":error.to_string()}));
            return Ok(plan);
        }
    }
    let mut budget = Budget::new();
    let mut seen = BTreeSet::new();
    for project in projects.values() {
        match protect_project(&data, project) {
            Ok(Some(root)) => {
                plan.roots.insert(root);
            }
            Ok(None) => (),
            Err(error) => plan
                .blocked
                .push(json!({"path":project,"reason":error.to_string()})),
        }
    }
    for bucket in ["workspaces", "archive"] {
        let folder = safe_path(&data, bucket)?;
        plan.roots.insert(folder.clone());
        if !folder.try_exists()? {
            continue;
        }
        if plan
            .account_locations
            .iter()
            .any(|account| external_path(&folder).starts_with(account))
        {
            plan.blocked.push(json!({"path":external_path(&folder),"reason":"Çalışma alanları sağlayıcı hesap klasöründe. Dosyalar korunuyor."}));
            continue;
        }
        for slot in children(&folder, &mut budget)? {
            let label = slot.file_name().and_then(|s| s.to_str()).unwrap_or("");
            let inspected = (|| -> Result<()> {
                let identifier = uuid::Uuid::parse_str(label)
                    .context("Klasör adı geçerli bir oturum kimliği değil.")?;
                if identifier.to_string() != label {
                    bail!("Klasör adı kayıtlı oturum adıyla eşleşmiyor.");
                }
                let metadata = fs::symlink_metadata(&slot)?;
                if !metadata.is_dir() || is_link(&metadata) {
                    bail!("Oturum alanı normal bir klasör değil.");
                }
                if !seen.insert(label.to_owned()) {
                    bail!("Aynı oturum için iki çalışma klasörü bulundu.");
                }
                outside_accounts(&slot, &plan.account_locations)?;
                let Some(session) = sessions.get(label) else {
                    if metadata_only(&slot, &mut budget)? && slot.join("baseline.json").is_file() {
                        plan.preserved_metadata_slots += 1;
                        return Ok(());
                    }
                    bail!("Bu klasörün sahipliğini doğrulayan bir oturum kaydı bulunamadı.");
                };
                if session.kind == "direct" && bucket == "workspaces" {
                    plan.preserved_direct_slots += 1;
                    return Ok(());
                }
                let project = projects
                    .get(&session.project)
                    .context("Oturumun ana proje kaydı bulunamadı.")?;
                let target = inspect(&data, &slot, session, project, &mut budget)?;
                if let Some(admin) = &target.admin {
                    outside_accounts(admin, &plan.account_locations)?;
                }
                plan.targets.push(target);
                Ok(())
            })();
            if let Err(error) = inspected {
                plan.blocked
                    .push(json!({"path":external_path(&slot),"reason":error.to_string()}));
            }
        }
    }
    Ok(plan)
}

pub(crate) fn plan(core: &Core) -> Result<Plan> {
    let mut projects = BTreeMap::new();
    let mut sessions = BTreeMap::new();
    let mut named_accounts = Vec::new();
    {
        let db = core.db.lock().unwrap();
        for provider in ["codex", "claude", "copilot"] {
            if let Some(accounts) = db["providers"][provider]["accounts"].as_object() {
                named_accounts.extend(accounts.keys().map(|id| (provider, id.clone())));
            }
        }
        for project in db["projects"]
            .as_array()
            .context("Proje kayıtları geçersiz.")?
        {
            let directory = PathBuf::from(string(project, "directory")?);
            if projects
                .insert(string(project, "id")?.to_owned(), directory)
                .is_some()
            {
                bail!("Yinelenen proje kaydı.");
            }
        }
        for value in db["sessions"]
            .as_array()
            .context("Oturum kayıtları geçersiz.")?
        {
            let session = Session {
                id: string(value, "id")?.to_owned(),
                name: string(value, "name")?.to_owned(),
                project: string(value, "projectId")?.to_owned(),
                directory: string(value, "directory")?.into(),
                kind: string(value, "workspaceKind")?.to_owned(),
                archived: value["archivedFromDirectory"].is_string(),
                cleanup_pending: value["workspaceCleanupPending"] == true,
            };
            if sessions.insert(session.id.clone(), session).is_some() {
                bail!("Yinelenen oturum kaydı.");
            }
        }
    }
    for directory in projects.values() {
        if let Ok(path) = dunce::canonicalize(directory) {
            core.check_project_directory(&path)?;
        }
    }
    let mut accounts = vec![core.directory.join("accounts")];
    if let Some(directory)=std::env::var_os("GH_CONFIG_DIR") {
        accounts.push(directory.into());
    }
    for provider in ["codex", "claude", "copilot"] {
        accounts.push(
            crate::accounts::Profile::for_history(core, provider, &Value::Null)?.config_dir(),
        );
    }
    for (provider, id) in named_accounts {
        let directory=crate::accounts::Profile::for_history(core, provider, &Value::String(id))?.config_dir();
        // The nested GitHub CLI profile may itself be redirected by a junction.
        if provider=="copilot" { accounts.push(directory.join("github-cli")); }
        accounts.push(directory);
    }
    inventory(&core.directory, &projects, &sessions, &accounts)
}

#[derive(Debug)]
pub(crate) struct CleanupError {
    pub(crate) completed: Vec<String>,
    pub(crate) error: anyhow::Error,
}

impl std::fmt::Display for CleanupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} (tamamlanan klasör sayısı: {})",
            self.error,
            self.completed.len()
        )
    }
}
impl std::error::Error for CleanupError {}

fn remove_target(data: &Path, target: &Target, account_paths: &[PathBuf]) -> Result<()> {
    // Recheck the immediate target after any earlier removals. In particular a
    // replaced bucket or slot may now be a junction outside the application.
    outside_accounts(&target.path, &account_locations(account_paths)?)?;
    let checked = inspect(
        data,
        &target.path,
        &target.session,
        &target.project,
        &mut Budget::new(),
    )?;
    if checked.digest != target.digest || checked.admin != target.admin {
        bail!("Dosyalar değişti. Önizlemeyi yenileyin.");
    }
    if let Some(admin) = &target.admin {
        let protected = account_locations(account_paths)?;
        outside_accounts(&target.path, &protected)?;
        outside_accounts(admin, &protected)?;
        workspace_archive::cleanup_remove(
            data,
            &target.project,
            &safe_path(&target.path, "files")?,
            admin,
        )?;
    }
    let bucket = if target.session.archived {
        "archive"
    } else {
        "workspaces"
    };
    let slot = safe_path(data, &format!("{bucket}/{}", target.session.id))?;
    if slot != target.path {
        bail!("Temizlenecek klasör değişti.");
    }
    outside_accounts(&slot, &account_locations(account_paths)?)?;
    fs::remove_dir_all(&slot).context("Yönetilen oturum klasörü tamamen kaldırılamadı.")?;
    Ok(())
}

pub(crate) fn remove(
    core: &Core,
    expected: &Plan,
) -> std::result::Result<Vec<String>, CleanupError> {
    let current = plan(core).map_err(|error| CleanupError {
        completed: Vec::new(),
        error,
    })?;
    remove_verified(&core.directory, expected, &current)
}

fn remove_verified(
    data: &Path,
    expected: &Plan,
    current: &Plan,
) -> std::result::Result<Vec<String>, CleanupError> {
    let mut completed = Vec::new();
    let operation = (|| -> Result<()> {
        if !current.blocked.is_empty() || !expected.blocked.is_empty() {
            bail!("Temizlemeyi engelleyen klasörler var.");
        }
        if current.fingerprint() != expected.fingerprint() {
            bail!("Dosyalar veya klasör kayıtları değişti. Önizlemeyi yenileyin.");
        }
        for target in &current.targets {
            remove_target(data, target, &current.account_paths)?;
            completed.push(target.session.id.clone());
        }
        Ok(())
    })();
    match operation {
        Ok(()) => Ok(completed),
        Err(error) => Err(CleanupError { completed, error }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture {
        root: PathBuf,
        data: PathBuf,
        source: PathBuf,
        projects: BTreeMap<String, PathBuf>,
        sessions: BTreeMap<String, Session>,
    }
    impl Fixture {
        fn new() -> Self {
            let root =
                std::env::temp_dir().join(format!("workflow-cleanup-test-{}", crate::core::id()));
            fs::create_dir(&root).unwrap();
            let data = root.join("data");
            let source = root.join("main-project");
            fs::create_dir(&data).unwrap();
            fs::create_dir(&source).unwrap();
            fs::write(source.join("project-marker"), b"protected").unwrap();
            let projects = BTreeMap::from([("project".to_owned(), source.clone())]);
            Self {
                root,
                data,
                source,
                projects,
                sessions: BTreeMap::new(),
            }
        }
        fn add(&mut self, kind: &str, archived: bool) -> PathBuf {
            let id = crate::core::id();
            let slot = self
                .data
                .join(if archived { "archive" } else { "workspaces" })
                .join(&id);
            fs::create_dir_all(&slot).unwrap();
            fs::write(slot.join("baseline.json"), b"{}").unwrap();
            let directory = if kind == "direct" {
                self.source.clone()
            } else {
                let files = slot.join("files");
                fs::create_dir(&files).unwrap();
                fs::write(files.join("work-file"), b"only inside the synthetic slot").unwrap();
                files
            };
            self.sessions.insert(
                id.clone(),
                Session {
                    id,
                    name: "synthetic".into(),
                    project: "project".into(),
                    directory,
                    kind: kind.into(),
                    archived,
                    cleanup_pending: false,
                },
            );
            slot
        }
        fn plan(&self) -> Plan {
            self.with_accounts(&[self.data.join("accounts")])
        }
        fn with_accounts(&self, roots: &[PathBuf]) -> Plan {
            inventory(&self.data, &self.projects, &self.sessions, roots).unwrap()
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let parent = dunce::canonicalize(std::env::temp_dir()).unwrap();
            let root = dunce::canonicalize(&self.root).unwrap();
            assert_eq!(root.parent(), Some(parent.as_path()));
            assert!(root
                .file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("workflow-cleanup-test-"));
            fs::remove_dir_all(root).unwrap();
        }
    }

    #[test]
    fn only_registered_isolated_slots_are_removed_and_source_direct_and_accounts_survive() {
        let mut f = Fixture::new();
        let working = f.add("copy", false);
        let archived = f.add("copy", true);
        let direct = f.add("direct", false);
        let account = f.data.join("accounts");
        fs::create_dir(&account).unwrap();
        fs::write(account.join("marker"), b"protected fixture").unwrap();
        f.projects
            .insert("previous".into(), f.root.join("previously-removed-project"));
        let preview = f.plan();
        assert!(preview.blocked.is_empty(), "{:?}", preview.blocked);
        assert_eq!(preview.public()["folders"].as_array().unwrap().len(), 2);
        assert_eq!(preview.public()["preservedDirectSlots"], 1);
        assert_eq!(
            remove_verified(&f.data, &preview, &f.plan()).unwrap().len(),
            2
        );
        assert!(!working.exists() && !archived.exists());
        assert!(direct.join("baseline.json").is_file());
        assert!(f.source.join("project-marker").is_file());
        assert!(account.join("marker").is_file());
    }

    #[test]
    fn changed_files_or_unregistered_slots_block_all_targets_before_the_first_removal() {
        let mut f = Fixture::new();
        let first = f.add("copy", false);
        let second = f.add("copy", true);
        let preview = f.plan();
        fs::write(second.join("files").join("new"), b"after preview").unwrap();
        assert!(remove_verified(&f.data, &preview, &f.plan())
            .unwrap_err()
            .completed
            .is_empty());
        let unknown = f.data.join("archive").join(crate::core::id());
        fs::create_dir(&unknown).unwrap();
        let blocked = f.plan();
        assert!(!blocked.blocked.is_empty());
        assert!(remove_verified(&f.data, &blocked, &blocked)
            .unwrap_err()
            .completed
            .is_empty());
        assert!(first.is_dir() && second.is_dir() && unknown.is_dir());
    }

    #[test]
    fn all_provider_account_roots_and_future_descendants_block_physical_cleanup() {
        for provider in ["codex", "claude", "copilot", "github-cli"] {
            let mut f = Fixture::new();
            let other = f.add("copy", false);
            let slot = f.add("copy", true);
            let account = slot.join("files").join(provider);
            fs::create_dir(&account).unwrap();
            fs::write(account.join("synthetic-marker"), b"keep").unwrap();
            for root in [
                &account,
                &account.join("future").join("profile"),
                &slot,
                &f.data.join("archive"),
            ] {
                let preview = f.with_accounts(&[root.clone()]);
                assert!(!preview.blocked.is_empty(), "{provider}");
                assert!(remove_verified(&f.data, &preview, &preview)
                    .unwrap_err()
                    .completed
                    .is_empty());
                assert!(other.is_dir() && account.join("synthetic-marker").is_file());
            }
            let relative = f.with_accounts(&[PathBuf::from("local-profile")]);
            assert!(!relative.blocked.is_empty());
            assert!(remove_verified(&f.data, &relative, &relative).is_err());
        }
    }

    #[cfg(any(windows, unix))]
    #[test]
    fn outside_account_junction_redirected_after_preview_protects_its_future_target() {
        let mut f = Fixture::new();
        let slot = f.add("copy", false);
        let profile = slot.join("files").join("auth");
        fs::create_dir(&profile).unwrap();
        fs::write(profile.join("synthetic-marker"), b"keep").unwrap();
        let alias = f.root.join("external-account");
        let roots = [alias.join("future-profile")];
        let before = f.with_accounts(&roots);
        assert!(before.blocked.is_empty());
        #[cfg(windows)]
        {
            let result = std::process::Command::new("cmd")
                .args(["/D", "/C", "mklink", "/J"])
                .arg(&alias)
                .arg(&profile)
                .output()
                .unwrap();
            assert!(
                result.status.success(),
                "Could not create synthetic account junction"
            );
        }
        #[cfg(unix)]
        std::os::unix::fs::symlink(&profile, &alias).unwrap();
        assert!(!f.with_accounts(&roots).blocked.is_empty());
        // Simulate retargeting after the whole-plan recheck, immediately before
        // the target is processed. The external alias never exists inside the slot.
        let failure = remove_verified(&f.data, &before, &before).unwrap_err();
        assert!(failure.completed.is_empty());
        assert!(profile.join("synthetic-marker").is_file());
        #[cfg(windows)]
        fs::remove_dir(alias).unwrap();
        #[cfg(unix)]
        fs::remove_file(alias).unwrap();
    }

    #[test]
    fn preview_counts_hundreds_of_copy_and_archive_slots_without_reading_file_contents() {
        let mut f = Fixture::new();
        for i in 0..300 {
            f.add("copy", i % 2 == 0);
        }
        let preview = f.plan();
        assert!(preview.blocked.is_empty(), "{:?}", preview.blocked);
        assert_eq!(preview.targets.len(), 300);
        assert_eq!(preview.public()["files"], 600);
        assert_eq!(
            preview
                .targets
                .iter()
                .filter(|t| t.session.archived)
                .count(),
            150
        );
        assert!(f.source.join("project-marker").is_file());
    }

    #[test]
    fn nested_gitfile_and_source_overlap_block_the_entire_cleanup() {
        let mut f = Fixture::new();
        let slot = f.add("copy", false);
        let nested = slot.join("files").join("embedded");
        fs::create_dir(&nested).unwrap();
        fs::write(nested.join(".git"), b"gitdir: outside").unwrap();
        assert!(!f.plan().blocked.is_empty());
        fs::remove_file(nested.join(".git")).unwrap();
        f.projects.insert("another-project".into(), nested);
        let preview = f.plan();
        assert!(!preview.blocked.is_empty());
        assert!(remove_verified(&f.data, &preview, &preview).is_err());
        assert!(slot.is_dir());
    }

    #[test]
    fn a_late_change_reports_only_previously_removed_targets() {
        let mut f = Fixture::new();
        let first = f.add("copy", false);
        let second = f.add("copy", true);
        let preview = f.plan();
        let verified = f.plan();
        fs::write(
            second.join("files").join("late"),
            b"changed after preflight",
        )
        .unwrap();
        let error = remove_verified(&f.data, &preview, &verified).unwrap_err();
        assert_eq!(
            error.completed,
            [first.file_name().unwrap().to_str().unwrap()]
        );
        assert!(!first.exists() && second.join("files").join("late").is_file());
        assert!(f.source.join("project-marker").is_file());
    }

    #[test]
    fn unregistered_baselines_are_preserved_and_an_interrupted_wrapper_can_be_retried() {
        let mut f = Fixture::new();
        let direct = f.add("direct", false);
        f.sessions
            .remove(direct.file_name().unwrap().to_str().unwrap());
        let working = f.add("copy", false);
        let id = working.file_name().unwrap().to_str().unwrap();
        fs::remove_file(working.join("files").join("work-file")).unwrap();
        fs::remove_dir(working.join("files")).unwrap();
        assert!(!f.plan().blocked.is_empty());
        f.sessions.get_mut(id).unwrap().cleanup_pending = true;
        let preview = f.plan();
        assert!(preview.blocked.is_empty(), "{:?}", preview.blocked);
        assert_eq!(preview.preserved_metadata_slots, 1);
        assert_eq!(remove_verified(&f.data, &preview, &f.plan()).unwrap(), [id]);
        assert!(direct.join("baseline.json").is_file());
    }

    #[cfg(any(windows, unix))]
    #[test]
    fn a_directory_link_to_the_source_blocks_cleanup_without_following_it() {
        let mut f = Fixture::new();
        let slot = f.add("copy", false);
        let junction = slot.join("files").join("outside");
        #[cfg(windows)]
        {
            let result = std::process::Command::new("cmd")
                .args(["/D", "/C", "mklink", "/J"])
                .arg(&junction)
                .arg(&f.source)
                .output()
                .unwrap();
            assert!(
                result.status.success(),
                "Could not create synthetic junction"
            );
        }
        #[cfg(unix)]
        std::os::unix::fs::symlink(&f.source, &junction).unwrap();
        let preview = f.plan();
        assert!(!preview.blocked.is_empty());
        assert!(remove_verified(&f.data, &preview, &preview)
            .unwrap_err()
            .completed
            .is_empty());
        assert!(f.source.join("project-marker").is_file());
        #[cfg(windows)]
        fs::remove_dir(junction).unwrap();
        #[cfg(unix)]
        fs::remove_file(junction).unwrap();
    }

    // Build an unborn Git administration layout entirely inside the temporary
    // fixture. This uses no initialization, commit, branch deletion or push.
    fn link_test_worktree(source: &Path, files: &Path) -> PathBuf {
        let common = source.join(".git");
        let admin = common.join("worktrees").join("fixture");
        fs::create_dir_all(&admin).unwrap();
        fs::create_dir_all(common.join("objects")).unwrap();
        fs::create_dir_all(common.join("refs").join("heads")).unwrap();
        fs::write(common.join("HEAD"), b"ref: refs/heads/main\n").unwrap();
        fs::write(
            common.join("config"),
            b"[core]\nrepositoryformatversion = 0\nbare = false\n",
        )
        .unwrap();
        fs::write(
            admin.join("HEAD"),
            b"ref: refs/heads/workflow-session-fixture\n",
        )
        .unwrap();
        fs::write(admin.join("commondir"), b"../..\n").unwrap();
        fs::write(
            common
                .join("refs")
                .join("heads")
                .join("workflow-session-fixture"),
            b"0000000000000000000000000000000000000001\n",
        )
        .unwrap();
        fs::write(
            admin.join("gitdir"),
            format!(
                "{}\n",
                files.join(".git").to_string_lossy().replace('\\', "/")
            ),
        )
        .unwrap();
        fs::write(
            files.join(".git"),
            format!("gitdir: {}\n", admin.to_string_lossy().replace('\\', "/")),
        )
        .unwrap();
        admin
    }

    #[test]
    fn registered_git_worktree_is_removed_by_git_but_locked_or_mismatched_links_are_preserved() {
        if std::process::Command::new("git")
            .arg("--version")
            .output()
            .is_err()
        {
            return;
        }
        let mut f = Fixture::new();
        let slot = f.add("worktree", false);
        let files = slot.join("files");
        let admin = link_test_worktree(&f.source, &files);
        fs::write(admin.join("locked"), b"locked").unwrap();
        assert!(!f.plan().blocked.is_empty());
        fs::remove_file(admin.join("locked")).unwrap();
        fs::write(
            admin.join("gitdir"),
            format!("{}\n", f.source.join(".git").display()),
        )
        .unwrap();
        assert!(!f.plan().blocked.is_empty());
        fs::write(
            admin.join("gitdir"),
            format!(
                "{}\n",
                files.join(".git").to_string_lossy().replace('\\', "/")
            ),
        )
        .unwrap();
        let preview = f.plan();
        assert!(preview.blocked.is_empty(), "{:?}", preview.blocked);
        let protected_admin = f.with_accounts(&[admin.join("future-account")]);
        assert!(!protected_admin.blocked.is_empty());
        assert!(remove_verified(&f.data, &protected_admin, &protected_admin).is_err());
        assert_eq!(
            remove_verified(&f.data, &preview, &f.plan()).unwrap().len(),
            1
        );
        assert!(!slot.exists() && !admin.exists());
        assert!(f.source.join("project-marker").is_file());
        assert!(f.source.join(".git").join("HEAD").is_file());
        assert!(f
            .source
            .join(".git/refs/heads/workflow-session-fixture")
            .is_file());
    }
}
