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
pub fn atomic(file: &Path, bytes: &[u8]) -> Result<()> {
    fs::create_dir_all(file.parent().context("Klasör yok")?)?;
    let tmp = file.with_extension(format!("{}.tmp", id()));
    let mut f = fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&tmp)?;
    use std::io::Write;
    let result = (|| {
        f.write_all(bytes)?;
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
            Ok(m) if m.file_type().is_symlink() => {
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
        let relative = e
            .path()
            .strip_prefix(root)?
            .to_string_lossy()
            .replace('\\', "/");
        if out.len() > 100_000 {
            bail!("Çalışma alanı dosya sınırını aşıyor")
        };
        out.insert(relative, hash(&fs::read(e.path())?));
    }
    Ok(out)
}

pub struct Core {
    pub directory: PathBuf,
    pub db: Mutex<Value>,
    pub terms: Mutex<HashMap<String, Arc<LiveTerminal>>>,
    pub login_terminal: Mutex<Option<crate::terminal::LoginTerminal>>,
    pub browsers: Mutex<BrowserState>,
    #[cfg(windows)]
    pub previews: crate::preview::Previews,
    pub operations: Mutex<()>,
    pub app: AppHandle,
    pub endpoint: Mutex<String>,
    pub pending: Mutex<HashMap<String, Value>>,
    pub _instance: fs::File,
}
impl Core {
    pub fn new(directory: PathBuf, app: AppHandle) -> Result<Arc<Self>> {
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
        let db = if let Some(saved) = saved {
            serde_json::from_slice(&saved).context("Çalışma alanı okunamadı; dosya korundu")?
        } else {
            json!({"version":1,"projects":[],"sessions":[],"links":[],"jobs":[],"preferences":{},"providers":{}})
        };
        Ok(Arc::new(Self {
            directory,
            db: Mutex::new(db),
            terms: Mutex::new(HashMap::new()),
            login_terminal: Mutex::new(None),
            browsers: Mutex::new(BrowserState::default()),
            #[cfg(windows)]
            previews: crate::preview::Previews::default(),
            operations: Mutex::new(()),
            app,
            endpoint: Mutex::new(String::new()),
            pending: Mutex::new(HashMap::new()),
            _instance: instance,
        }))
    }
    pub fn save(&self) -> Result<()> {
        self.save_workspace(&self.db.lock().unwrap())
    }
    pub(crate) fn save_workspace(&self, db: &Value) -> Result<()> {
        atomic(
            &self.directory.join("workspace.json"),
            &serde_json::to_vec_pretty(db)?,
        )
    }
    pub fn emit(&self, event: &str, value: Value) {
        let _ = self.app.emit_to("main", event, value);
    }
    pub fn snapshot(&self) -> Value {
        let mut v = self.db.lock().unwrap().clone();
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
        #[cfg(windows)]
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
}
