use crate::{browser::BrowserState, terminal::LiveTerminal};
use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    fs,
    path::{Component, Path, PathBuf},
    sync::{Arc, Mutex},
};
use tauri::{AppHandle, Emitter};
use uuid::Uuid;

pub fn id() -> String {
    Uuid::new_v4().to_string()
}
pub fn now() -> String {
    chrono::Utc::now().to_rfc3339()
}
pub fn hash(b: &[u8]) -> String {
    format!("{:x}", Sha256::digest(b))
}
pub fn string<'a>(v: &'a Value, k: &str) -> Result<&'a str> {
    v[k].as_str().ok_or_else(|| anyhow!("Eksik alan: {k}"))
}
pub(crate) fn link_needs_reapproval(link: &Value, source: &Value, target: &Value) -> bool {
    [
        (&link["sourceConversation"], &source["conversationId"]),
        (&link["targetConversation"], &target["conversationId"]),
    ]
    .into_iter()
    .any(|(pinned, current)| !pinned.is_null() && pinned != current)
}
pub fn atomic(file: &Path, bytes: &[u8]) -> Result<()> {
    atomic_with_mode(file, bytes, None)
}

pub(crate) fn atomic_json<T: serde::Serialize>(file: &Path, value: &T) -> Result<()> {
    #[cfg(unix)]
    let mode=Some(file_mode(file)?.unwrap_or(0o600));
    #[cfg(not(unix))]
    let mode=None;
    fs::create_dir_all(file.parent().context("Klasör yok")?)?;
    let temporary=file.with_extension(format!("{}.tmp",id()));
    let mut output=create_atomic_temp(&temporary,mode)?;
    let result=(|| -> Result<()> {
        {
            use std::io::Write;
            let mut buffered=std::io::BufWriter::with_capacity(64*1024,&mut output);
            serde_json::to_writer_pretty(&mut buffered,value)?;
            buffered.flush()?;
        }
        #[cfg(unix)]
        if let Some(mode)=mode {
            use std::os::unix::fs::PermissionsExt;
            output.set_permissions(fs::Permissions::from_mode(mode))?;
        }
        output.sync_all()?;
        drop(output);
        fs::rename(&temporary,file)?;
        Ok(())
    })();
    if result.is_err() { let _=fs::remove_file(&temporary); }
    result
}

// Only ordinary Unix permission bits travel with project content. Never copy
// ownership, setuid/setgid or sticky bits from a workspace or backup object.
pub(crate) fn file_mode(file: &Path) -> Result<Option<u32>> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        return match fs::symlink_metadata(file) {
            Ok(metadata) if metadata.is_file() => Ok(Some(metadata.permissions().mode() & 0o777)),
            Ok(_) => bail!("Yalnızca normal dosyaların izinleri kullanılabilir"),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.into()),
        };
    }
    #[cfg(not(unix))]
    {
        let _ = file;
        Ok(None)
    }
}

fn create_atomic_temp(file: &Path, mode: Option<u32>) -> Result<fs::File> {
    let mut options = fs::OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    if mode.is_some() {
        use std::os::unix::fs::OpenOptionsExt;
        // Restrict access at creation, before the first project byte is written.
        options.mode(0o600);
    }
    #[cfg(not(unix))]
    let _ = mode;
    Ok(options.open(file)?)
}

pub(crate) fn atomic_with_mode(file: &Path, bytes: &[u8], mode: Option<u32>) -> Result<()> {
    if mode.is_some_and(|mode| mode > 0o777) {
        bail!("Geçersiz dosya izinleri")
    }
    #[cfg(unix)]
    let mode = mode.or(file_mode(file)?);
    fs::create_dir_all(file.parent().context("Klasör yok")?)?;
    let tmp = file.with_extension(format!("{}.tmp", id()));
    let mut f = create_atomic_temp(&tmp, mode)?;
    use std::io::Write;
    let result = (|| {
        f.write_all(bytes)?;
        #[cfg(unix)]
        if let Some(mode) = mode {
            use std::os::unix::fs::PermissionsExt;
            f.set_permissions(fs::Permissions::from_mode(mode))?;
        }
        f.sync_all()?;
        drop(f);
        fs::rename(&tmp, file)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    Ok(result?)
}
pub fn safe_path(root: &Path, rel: &str) -> Result<PathBuf> {
    fn allowed(component: Component<'_>) -> bool {
        let Component::Normal(name) = component else {
            return false;
        };
        let name = name.to_string_lossy();
        !name.eq_ignore_ascii_case(".git")
            && (!cfg!(windows) || (!name.contains(':') && !name.ends_with(['.', ' '])))
    }
    let root = root.canonicalize()?;
    let p = Path::new(rel);
    if p.is_absolute() || rel.contains('\0') || !p.components().all(allowed) {
        bail!("Geçersiz göreli dosya yolu")
    }
    let mut at = root.clone();
    for component in p.components() {
        at.push(component);
        match fs::symlink_metadata(&at) {
            Ok(m) if is_link(&m) => {
                bail!("Bağlantılı dosyalar bu işlemde desteklenmiyor")
            }
            Ok(_) => {
                at = at.canonicalize()?;
                if !at
                    .strip_prefix(&root)
                    .is_ok_and(|p| p.components().all(allowed))
                {
                    bail!("Dosya çalışma alanı dışında")
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(at)
}
pub(crate) fn is_link(metadata: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        use windows::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;
        return metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0;
    }
    #[cfg(not(windows))]
    metadata.file_type().is_symlink()
}
pub fn read_optional(p: &Path) -> Result<Option<Vec<u8>>> {
    match fs::read(p) {
        Ok(b) => Ok(Some(b)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}
pub fn file_hash(p: &Path) -> Result<Option<String>> {
    Ok(read_optional(p)?.map(|b| hash(&b)))
}
pub fn scan(root: &Path) -> Result<HashMap<String, String>> {
    scan_with(root, |_, path| Ok(hash(&fs::read(path)?)))
}

pub(crate) fn relative_name(path: &Path) -> Result<String> {
    let text = path.to_str().context("Dosya adı UTF-8 olarak işlenemiyor")?;
    if cfg!(windows) { Ok(text.replace('\\', "/")) } else { Ok(text.to_owned()) }
}

// Share enumeration, ignore rules and limits while letting snapshot creation
// hash and persist the same bytes without a second source-file read.
pub(crate) fn scan_with(
    root: &Path,
    mut read: impl FnMut(&str, &Path) -> Result<String>,
) -> Result<HashMap<String, String>> {
    let mut out = HashMap::new();
    let walker = ignore::WalkBuilder::new(root)
        .add_custom_ignore_filename(".agentloomignore")
        .add_custom_ignore_filename(".workflowignore")
        .hidden(false)
        .git_ignore(true)
        .git_exclude(true)
        .git_global(false)
        .require_git(false)
        .filter_entry(|e| {
            let n = e.file_name().to_string_lossy();
            !matches!(
                n.as_ref(),
                ".git"
                    | "node_modules"
                    | "target"
                    | ".workflow"
                    | ".agentloom"
                    | "bin"
                    | "obj"
                    | ".env"
                    | ".ssh"
                    | ".aws"
            )
        })
        .build();
    for e in walker {
        let e = e?;
        if !e.file_type().map(|t| t.is_file()).unwrap_or(false) {
            continue;
        }
        let m = e.metadata()?;
        if m.len() > 128 * 1024 * 1024 {
            bail!(
                "Dosya çalışma alanı sınırını aşıyor; .workflowignore ile hariç tutabilirsin: {}",
                e.path().display()
            )
        }
        let relative = relative_name(e.path().strip_prefix(root)?)?;
        if out.len() > 100_000 {
            bail!("Çalışma alanı dosya sınırını aşıyor")
        };
        let digest = read(&relative, e.path())?;
        out.insert(relative, digest);
    }
    Ok(out)
}

pub struct Core {
    pub directory: PathBuf,
    pub db: Mutex<Value>,
    pub terms: Mutex<HashMap<String, Arc<LiveTerminal>>>,
    pub login_terminal: Mutex<Option<crate::terminal::LoginTerminal>>,
    pub(crate) chat_log: Mutex<crate::chat::LogCache>,
    pub(crate) session_usage: Mutex<crate::session_usage::Cache>,
    pub(crate) model_catalog: crate::models::CatalogCache,
    pub(crate) custom_usage: crate::custom_usage::Queries,
    pub(crate) notifications: Mutex<crate::notifications::Notifications>,
    pub browsers: Mutex<BrowserState>,
    #[cfg(any(windows, target_os = "macos"))]
    pub previews: crate::preview::Previews,
    #[cfg(any(windows, target_os = "macos"))]
    pub updates: crate::updates::Updates,
    pub operations: Mutex<()>,
    pub(crate) repository_operations: crate::repository_operations::RepositoryOperations,
    pub app: AppHandle,
    pub endpoint: Mutex<String>,
    pub pending: Mutex<HashMap<String, Value>>,
    pub(crate) browser_opening: std::sync::atomic::AtomicUsize,
    pub _instance: fs::File,
}
impl Core {
    pub fn new(directory: PathBuf, app: AppHandle) -> Result<Arc<Self>> {
        #[cfg(target_os = "macos")]
        {
            use tauri::Manager;
            crate::platform::macos::private_data_directory(&directory,Some(&app.path().app_data_dir()?))?;
        }
        fs::create_dir_all(&directory)?;
        let instance = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(directory.join("instance.lock"))?;
        instance
            .try_lock()
            .context("Workflow bu veri klasörüyle zaten açık")?;
        let file = directory.join("workspace.json");
        let saved = match read_optional(&file)? {
            Some(saved) => Some(saved),
            None => read_optional(&file.with_extension("previous"))?,
        };
        let mut db = if let Some(saved) = saved {
            serde_json::from_slice(&saved).context("Çalışma alanı okunamadı; dosya korundu")?
        } else {
            json!({"version":1,"projects":[],"sessions":[],"links":[],"jobs":[],"preferences":{},"providers":{}})
        };
        crate::verification::recover(&mut db);
        crate::workspace_archive::recover(&directory, &db);
        Ok(Arc::new(Self {
            #[cfg(any(windows, target_os = "macos"))]
            updates: crate::updates::Updates::load(&directory),
            directory,
            db: Mutex::new(db),
            terms: Mutex::new(HashMap::new()),
            login_terminal: Mutex::new(None),
            chat_log: Mutex::new(crate::chat::LogCache::default()),
            session_usage: Mutex::new(crate::session_usage::Cache::default()),
            model_catalog: crate::models::CatalogCache::default(),
            custom_usage: crate::custom_usage::Queries::default(),
            notifications: Mutex::new(crate::notifications::Notifications::default()),
            browsers: Mutex::new(BrowserState::default()),
            #[cfg(any(windows, target_os = "macos"))]
            previews: crate::preview::Previews::default(),
            operations: Mutex::new(()),
            repository_operations: crate::repository_operations::RepositoryOperations::default(),
            app,
            endpoint: Mutex::new(String::new()),
            pending: Mutex::new(HashMap::new()),
            browser_opening: std::sync::atomic::AtomicUsize::new(0),
            _instance: instance,
        }))
    }
    pub fn save(&self) -> Result<()> {
        self.save_workspace(&self.db.lock().unwrap())
    }
    pub(crate) fn save_workspace(&self, db: &Value) -> Result<()> {
        atomic_json(&self.directory.join("workspace.json"),db)
    }
    pub fn emit(&self, event: &str, value: Value) {
        let _ = self.app.emit_to("main", event, value);
    }
    pub fn snapshot(&self) -> Value {
        // Verification plans/results are fetched for the selected session only.
        // Do not resend every archived session's detailed report on each update.
        let mut v = Value::Object(self.db.lock().unwrap().as_object().unwrap().iter()
            .filter(|(key, _)| key.as_str() != "verifications")
            .map(|(key, value)| (key.clone(), value.clone())).collect());
        let mut links = v["links"].take();
        if let (Some(sessions), Some(links)) = (v["sessions"].as_array(), links.as_array_mut()) {
            let sessions: HashMap<_, _> = sessions
                .iter()
                .filter_map(|s| Some((s["id"].as_str()?, s)))
                .collect();
            for link in links {
                if let (Some(source), Some(target)) = (
                    link["source"].as_str().and_then(|id| sessions.get(id)),
                    link["target"].as_str().and_then(|id| sessions.get(id)),
                ) {
                    link["needsReapproval"] = json!(link_needs_reapproval(link, source, target));
                }
            }
        }
        v["links"] = links;
        if let Some(providers) = v["providers"].as_object_mut() {
            for provider in providers.values_mut() {
                if let Some(accounts) = provider.get_mut("accounts").and_then(Value::as_object_mut)
                {
                    for account in accounts.values_mut().filter_map(Value::as_object_mut) {
                        account.remove("identity");
                    }
                }
            }
        }
        v["active"] = json!(self
            .terms
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, t)| !t.exited.load(std::sync::atomic::Ordering::SeqCst))
            .map(|(id, t)| json!({"id":id,"runId":t.run_id,"pid":t.pid}))
            .collect::<Vec<_>>());
        v["directory"] = json!(self.directory);
        v
    }
    pub fn session(&self, id: &str) -> Result<Value> {
        self.db.lock().unwrap()["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["id"] == id)
            .cloned()
            .context("Oturum bulunamadı")
    }
    pub fn project(&self, id: &str) -> Result<Value> {
        self.db.lock().unwrap()["projects"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["id"] == id)
            .cloned()
            .context("Proje bulunamadı")
    }
    pub fn check_project_directory(&self, directory: &Path) -> Result<()> {
        let data = dunce::canonicalize(&self.directory)?;
        if directory.starts_with(&data) || data.starts_with(directory) {
            bail!("Workflow veri klasörü veya onu içeren klasör proje olarak kullanılamaz")
        }
        Ok(())
    }
    pub fn root(&self, args: &Value) -> Result<PathBuf> {
        if let Some(s) = args["sessionId"].as_str() {
            return Ok(PathBuf::from(string(&self.session(s)?, "directory")?));
        }
        let directory = dunce::canonicalize(string(
            &self.project(string(args, "projectId")?)?,
            "directory",
        )?)?;
        self.check_project_directory(&directory)?;
        Ok(directory)
    }
    pub fn update_session(&self, id: &str, patch: Value) -> Result<Value> {
        let out;
        {
            let patch = patch.as_object().context("Geçersiz güncelleme")?;
            let mut db = self.db.lock().unwrap();
            let index = db["sessions"]
                .as_array()
                .unwrap()
                .iter()
                .position(|s| s["id"] == id)
                .context("Oturum bulunamadı")?;
            let previous = db["sessions"][index].clone();
            let s = &mut db["sessions"][index];
            for (k, v) in patch {
                s[k] = v.clone();
            }
            out = s.clone();
            if let Err(error) = self.save_workspace(&db) {
                db["sessions"][index] = previous;
                return Err(error);
            }
        }
        self.emit("workspace", self.snapshot());
        Ok(out)
    }
    pub fn shutdown(&self) {
        self.custom_usage.shutdown();
        crate::notification_popup::shutdown(self);
        #[cfg(any(windows, target_os = "macos"))]
        self.previews.shutdown();
        for t in self.terms.lock().unwrap().values() {
            t.stop();
        }
        if let Some(login) = self.login_terminal.lock().unwrap().take() {
            login.terminal.stop();
        }
        self.browsers.lock().unwrap().close();
    }
}

pub fn home() -> PathBuf {
    std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
        .map(PathBuf::from)
        .unwrap_or_default()
}
// Keep filesystem validation canonical; use compatible paths at external process boundaries.
pub fn external_path(path: &Path) -> &Path {
    dunce::simplified(path)
}
pub fn run(root: &Path, program: &str, args: &[String]) -> Result<std::process::Output> {
    #[cfg(target_os = "macos")]
    let git_executable = if program=="git" {Some(crate::platform::macos::git_executable().context("Git bulunamadı; Git isteğe bağlıdır.")?)}else{None};
    #[cfg(target_os = "macos")]
    let program = git_executable.as_ref().map(|path|path.to_str().context("Git yolu geçersiz")).transpose()?.unwrap_or(program);
    let mut c = std::process::Command::new(program);
    c.args(args)
        .current_dir(external_path(root))
        .env("GIT_TERMINAL_PROMPT", "0");
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        c.creation_flags(0x08000000);
    }
    c.output()
        .with_context(|| format!("Program çalıştırılamadı: {program}"))
}
pub fn git(root: &Path, args: &[&str]) -> Result<String> {
    Ok(String::from_utf8_lossy(&git_bytes(root, args)?)
        .trim_end()
        .to_string())
}
pub fn git_bytes(root: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let output = run(
        root,
        "git",
        &args
            .iter()
            .map(|s| {
                if s.starts_with("\\\\?\\") {
                    external_path(Path::new(s)).to_string_lossy().into_owned()
                } else {
                    s.to_string()
                }
            })
            .collect::<Vec<_>>(),
    )?;
    if !output.status.success() {
        bail!("{}", String::from_utf8_lossy(&output.stderr).trim())
    }
    Ok(output.stdout)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn streamed_workspace_json_replaces_atomically_and_failed_serialization_keeps_original() {
        struct Fail;
        impl serde::Serialize for Fail {
            fn serialize<S:serde::Serializer>(&self,_serializer:S)->std::result::Result<S::Ok,S::Error> {
                Err(serde::ser::Error::custom("synthetic serialization failure"))
            }
        }
        let directory=std::env::temp_dir().join(format!("workflow-json-{}",id()));
        fs::create_dir(&directory).unwrap();
        let path=directory.join("workspace.json");
        let value=json!({"sessions":(0..600).map(|i|json!({"id":i,"archived":i%2==0})).collect::<Vec<_>>()});
        atomic_json(&path,&value).unwrap();
        let before=fs::read(&path).unwrap();
        assert_eq!(serde_json::from_slice::<Value>(&before).unwrap(),value);
        assert!(atomic_json(&path,&Fail).is_err());
        assert_eq!(fs::read(&path).unwrap(),before);
        assert_eq!(fs::read_dir(&directory).unwrap().count(),1);
        fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn unix_scan_keeps_literal_backslashes_and_rejects_non_utf8_names() {
        use std::{ffi::OsString, os::unix::ffi::OsStringExt};
        let root = std::env::temp_dir().join(id());
        fs::create_dir_all(root.join("a")).unwrap();
        fs::write(root.join("a\\b.txt"), b"literal filename").unwrap();
        fs::write(root.join("a/b.txt"), b"nested filename").unwrap();
        let files = scan(&root).unwrap();
        assert_eq!(files.len(), 2);
        assert_eq!(files["a\\b.txt"], hash(b"literal filename"));
        assert_eq!(files["a/b.txt"], hash(b"nested filename"));
        let invalid = OsString::from_vec(vec![0xff, b'.', b't']);
        assert!(relative_name(Path::new(&invalid)).is_err());
        // APFS rejects ill-formed UTF-8 names at creation; other Unix filesystems may accept them.
        #[cfg(not(target_os = "macos"))]
        {
            fs::write(root.join(invalid), b"synthetic").unwrap();
            assert!(scan(&root).is_err());
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn link_reapproval_requires_a_changed_pinned_conversation() {
        let original = json!({"conversationId":"first"});
        let changed = json!({"conversationId":"second"});
        let unbound = json!({});
        let link = json!({"sourceConversation":"first","targetConversation":"first"});
        assert!(!link_needs_reapproval(&link, &original, &original));
        assert!(link_needs_reapproval(&link, &changed, &original));
        assert!(link_needs_reapproval(&link, &original, &unbound));
        assert!(!link_needs_reapproval(&json!({}), &original, &changed));
    }
    #[cfg(windows)]
    #[test]
    fn external_paths_keep_windows_identity() {
        assert_eq!(
            external_path(Path::new(r"\\?\C:\Clone parent ç\kampus")),
            Path::new(r"C:\Clone parent ç\kampus")
        );
        let unc = Path::new(r"\\?\UNC\server\share\kampus");
        assert_eq!(external_path(unc), unc);
        let reserved = Path::new(r"\\?\C:\CON");
        assert_eq!(external_path(reserved), reserved);
        assert_eq!(
            external_path(Path::new("relative/file")),
            Path::new("relative/file")
        );
    }
    #[test]
    fn paths_reject_escape() {
        let p = std::env::temp_dir().join(id());
        fs::create_dir(&p).unwrap();
        assert!(safe_path(&p, "../outside").is_err());
        assert!(safe_path(&p, ".GIT/config").is_err());
        #[cfg(windows)]
        assert!(safe_path(&p, "file.txt:stream").is_err());
        assert!(safe_path(&p, "a/b.txt")
            .unwrap()
            .starts_with(p.canonicalize().unwrap()));
        fs::remove_dir(&p).unwrap();
    }
    #[test]
    fn atomic_replaces() {
        let p = std::env::temp_dir().join(id());
        fs::create_dir(&p).unwrap();
        atomic(&p.join("x"), b"one").unwrap();
        atomic(&p.join("x"), b"two").unwrap();
        assert_eq!(fs::read(p.join("x")).unwrap(), b"two");
        fs::remove_dir_all(&p).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn private_atomic_temp_is_restricted_before_first_write() {
        use std::io::Write;
        use std::os::unix::fs::PermissionsExt;
        let directory = std::env::temp_dir().join(id());
        fs::create_dir(&directory).unwrap();
        let path = directory.join("temporary");
        let mut file = create_atomic_temp(&path, Some(0o600)).unwrap();
        assert_eq!(file.metadata().unwrap().len(), 0);
        assert_eq!(file.metadata().unwrap().permissions().mode() & 0o077, 0);
        file.write_all(b"synthetic content").unwrap();
        assert_eq!(file.metadata().unwrap().permissions().mode() & 0o077, 0);
        drop(file);
        fs::remove_file(path).unwrap();
        fs::remove_dir(directory).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn atomic_retains_private_permissions_and_new_file_defaults() {
        use std::os::unix::fs::PermissionsExt;
        let directory = std::env::temp_dir().join(id());
        fs::create_dir(&directory).unwrap();
        let private = directory.join("private");
        fs::write(&private, b"synthetic before").unwrap();
        fs::set_permissions(&private, fs::Permissions::from_mode(0o600)).unwrap();
        atomic(&private, b"synthetic after").unwrap();
        assert_eq!(file_mode(&private).unwrap(), Some(0o600));
        let control = directory.join("control");
        fs::write(&control, b"synthetic default").unwrap();
        let created = directory.join("created");
        atomic(&created, b"synthetic default").unwrap();
        assert_eq!(file_mode(&created).unwrap(), file_mode(&control).unwrap());
        for file in [private, control, created] { fs::remove_file(file).unwrap(); }
        fs::remove_dir(directory).unwrap();
    }
}
