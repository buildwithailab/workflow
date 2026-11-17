use crate::core::{atomic, hash, Core};
use anyhow::{bail, Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use reqwest::{blocking::Client, redirect::Policy, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    fs,
    io::Read,
    os::windows::process::CommandExt,
    path::{Path, PathBuf},
    process::Command,
    sync::{Arc, Mutex},
    time::Duration,
};
use windows::Win32::System::Threading::CREATE_NO_WINDOW;

const REPO: &str = "https://github.com/buildwithailab/workflow";
const RELEASES: &str = "https://api.github.com/repos/buildwithailab/workflow/releases";
const ASSET: &str = "Workflow-Update-Windows-x64.exe";
const VERSION: &str = env!("CARGO_PKG_VERSION");
const DAY: i64 = 24 * 60 * 60;
const MAX_ASSET: u64 = 128 * 1024 * 1024;
const SUPPORTED: bool = cfg!(all(target_arch = "x86_64", not(debug_assertions)));
const CHECK_FAILED: &str = "Güncellemeler kontrol edilemedi.";
const DOWNLOAD_FAILED: &str = "Güncelleme indirilemedi veya doğrulanamadı.";
const BUSY: &str = "Güncelleme işlemi zaten devam ediyor.";

#[derive(Clone, Deserialize, Serialize)]
struct Release {
    version: String,
    notes: String,
    package: Option<Package>,
}

#[derive(Clone, Deserialize, Serialize)]
struct Package {
    size: u64,
    sha256: String,
}

#[derive(Deserialize)]
struct GitHubRelease {
    tag_name: String,
    body: Option<String>,
    draft: bool,
    prerelease: bool,
    #[serde(default)]
    immutable: bool,
    assets: Vec<GitHubAsset>,
}

#[derive(Deserialize)]
struct GitHubAsset {
    name: String,
    size: u64,
    state: String,
    browser_download_url: String,
    digest: Option<String>,
}

#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(default)]
struct Cache {
    automatic: Option<bool>,
    checked_at: Option<i64>,
    installed: Option<Release>,
    latest: Option<Release>,
    error: Option<String>,
}

impl Cache {
    fn automatic(&self) -> bool {
        self.automatic.unwrap_or(true)
    }
}

struct Pending {
    version: String,
    sha256: String,
    path: PathBuf,
    automatic: bool,
}

#[derive(Default)]
struct Installation {
    pending: Option<Pending>,
    closing: bool,
}

pub struct Updates {
    directory: PathBuf,
    cache: Mutex<Cache>,
    operation: Mutex<()>,
    installation: Mutex<Installation>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Automatic,
    Check,
    Download,
}

fn version(value: &str) -> Option<[u64; 3]> {
    let mut components = value.split('.');
    let mut result = [0; 3];
    for number in &mut result {
        let part = components.next()?;
        *number = part.parse().ok()?;
        if number.to_string() != part {
            return None;
        }
    }
    components.next().is_none().then_some(result)
}

fn is_newer(candidate: &str, installed: &str) -> bool {
    matches!((version(candidate), version(installed)), (Some(a), Some(b)) if a > b)
}

fn due(last: Option<i64>, now: i64) -> bool {
    last.is_none_or(|last| last > now || now.saturating_sub(last) >= DAY)
}

fn download_url(version: &str) -> String {
    format!("{REPO}/releases/download/v{version}/{ASSET}")
}

fn release(info: GitHubRelease) -> Option<Release> {
    let tag = info.tag_name.strip_prefix('v')?;
    version(tag)?;
    if info.draft || info.prerelease {
        return None;
    }
    let package = info
        .immutable
        .then(|| {
            let asset = info.assets.iter().find(|asset| asset.name == ASSET)?;
            let digest = asset.digest.as_deref()?.strip_prefix("sha256:")?;
            if asset.state != "uploaded"
                || asset.browser_download_url != download_url(tag)
                || !(64..=MAX_ASSET).contains(&asset.size)
                || digest.len() != 64
                || !digest.bytes().all(|byte| byte.is_ascii_hexdigit())
            {
                return None;
            }
            Some(Package {
                size: asset.size,
                sha256: digest.to_ascii_lowercase(),
            })
        })
        .flatten();
    Some(Release {
        version: tag.to_owned(),
        notes: info.body.unwrap_or_default().chars().take(40_000).collect(),
        package,
    })
}

fn client() -> Result<Client> {
    Ok(Client::builder()
        .user_agent(concat!("Workflow/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(90))
        .redirect(Policy::custom(|attempt| {
            let url = attempt.url();
            if attempt.previous().len() < 5
                && url.scheme() == "https"
                && url.port().is_none()
                && url.username().is_empty()
                && url.password().is_none()
                && matches!(
                    url.host_str(),
                    Some(
                        "github.com"
                            | "release-assets.githubusercontent.com"
                            | "objects.githubusercontent.com"
                    )
                )
            {
                attempt.follow()
            } else {
                attempt.error("Unsupported update redirect")
            }
        }))
        .build()?)
}

fn read_release(client: &Client, reference: &str) -> Result<Option<Release>> {
    let response = client
        .get(format!("{RELEASES}/{reference}"))
        .header("Accept", "application/vnd.github+json")
        .timeout(Duration::from_secs(15))
        .send()?;
    if response.status() == StatusCode::NOT_FOUND {
        return Ok(None);
    }
    let mut body = Vec::new();
    response
        .error_for_status()?
        .take(512 * 1024)
        .read_to_end(&mut body)?;
    Ok(release(serde_json::from_slice(&body)?))
}

fn verified_package(bytes: &[u8], package: &Package) -> bool {
    bytes.len() as u64 == package.size && bytes.starts_with(b"MZ") && hash(bytes) == package.sha256
}

impl Updates {
    pub fn load(profile: &Path) -> Self {
        let directory = profile.join("updates");
        let mut cache: Cache = fs::read(directory.join("state.json"))
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        let receipt = directory.join("install-result");
        if let Ok(value) = fs::read_to_string(&receipt) {
            match value.as_str() {
                "failed" => cache.error = Some("Güncelleme kurulamadı.".into()),
                "restart-failed" => {
                    cache.error = Some("Güncellemeden sonra Workflow yeniden açılamadı.".into())
                }
                _ => {}
            }
            let _ = fs::remove_file(receipt);
        }
        Self {
            directory,
            cache: Mutex::new(cache),
            operation: Mutex::new(()),
            installation: Mutex::new(Installation::default()),
        }
    }

    fn save(&self, cache: &Cache) -> Result<()> {
        atomic(
            &self.directory.join("state.json"),
            &serde_json::to_vec(cache)?,
        )
    }

    fn status(&self) -> Value {
        let cache = self.cache.lock().unwrap();
        let installation = self.installation.lock().unwrap();
        let latest = cache
            .latest
            .as_ref()
            .filter(|release| is_newer(&release.version, VERSION));
        let installed = cache
            .installed
            .as_ref()
            .filter(|release| release.version == VERSION)
            .or_else(|| {
                cache
                    .latest
                    .as_ref()
                    .filter(|release| release.version == VERSION)
            });
        json!({
            "version": VERSION,
            "automatic": cache.automatic() && SUPPORTED,
            "supported": SUPPORTED,
            "checkedAt": cache.checked_at,
            "installedNotes": installed.map(|release| release.notes.as_str()),
            "available": latest.map(|release| json!({
                "version": release.version,
                "notes": release.notes,
                "downloadable": SUPPORTED && release.package.is_some()
            })),
            "pendingVersion": installation.pending.as_ref().map(|update| update.version.as_str()),
            "error": cache.error
        })
    }

    fn set_automatic(&self, enabled: bool) -> Result<()> {
        if enabled && !SUPPORTED {
            bail!("Bu derleme otomatik güncellemeyi desteklemiyor.");
        }
        let mut cache = self.cache.lock().unwrap();
        let mut updated = cache.clone();
        updated.automatic = Some(enabled);
        if enabled && !cache.automatic() {
            updated.checked_at = None;
        }
        self.save(&updated)?;
        *cache = updated;
        if !enabled {
            let mut installation = self.installation.lock().unwrap();
            if installation
                .pending
                .as_ref()
                .is_some_and(|update| update.automatic)
            {
                if let Some(update) = installation.pending.take() {
                    let _ = fs::remove_file(update.path);
                }
            }
        }
        Ok(())
    }

    fn check(&self, mode: Mode) -> Result<()> {
        let _operation = self
            .operation
            .try_lock()
            .map_err(|_| anyhow::anyhow!(BUSY))?;
        let now = chrono::Utc::now().timestamp();
        {
            let cache = self.cache.lock().unwrap();
            if mode == Mode::Automatic
                && (!SUPPORTED || !cache.automatic() || !due(cache.checked_at, now))
            {
                return Ok(());
            }
        }
        let result = self.fetch(mode);
        let mut cache = self.cache.lock().unwrap();
        cache.checked_at = Some(now);
        cache.error = result.as_ref().err().map(ToString::to_string);
        self.save(&cache)?;
        result
    }

    fn fetch(&self, mode: Mode) -> Result<()> {
        let client = client().context(CHECK_FAILED)?;
        let latest = read_release(&client, "latest").context(CHECK_FAILED)?;
        let known_installed = {
            let cache = self.cache.lock().unwrap();
            cache
                .installed
                .iter()
                .chain(cache.latest.iter())
                .find(|release| release.version == VERSION)
                .cloned()
        };
        let installed = if latest
            .as_ref()
            .is_some_and(|release| release.version == VERSION)
        {
            latest.clone()
        } else {
            known_installed.or_else(|| {
                read_release(&client, &format!("tags/v{VERSION}"))
                    .ok()
                    .flatten()
                    .filter(|release| release.version == VERSION)
            })
        };
        {
            let mut cache = self.cache.lock().unwrap();
            if installed.is_some() {
                cache.installed = installed;
            }
            cache.latest = latest.clone();
        }
        let candidate = latest
            .filter(|release| is_newer(&release.version, VERSION) && release.package.is_some());
        let automatic = mode != Mode::Download;
        if SUPPORTED && (!automatic || self.cache.lock().unwrap().automatic()) {
            if let Some(release) = candidate {
                return self
                    .download(&client, release, automatic)
                    .context(DOWNLOAD_FAILED);
            }
        }
        if mode == Mode::Download {
            bail!("Bu sürüm için kurulabilir güncelleme bulunamadı.");
        }
        Ok(())
    }

    fn download(&self, client: &Client, release: Release, automatic: bool) -> Result<()> {
        if self
            .installation
            .lock()
            .unwrap()
            .pending
            .as_ref()
            .is_some_and(|update| update.version == release.version)
        {
            return Ok(());
        }
        let package = release.package.context(DOWNLOAD_FAILED)?;
        let response = client
            .get(download_url(&release.version))
            .send()?
            .error_for_status()?;
        let mut bytes = Vec::new();
        response.take(package.size + 1).read_to_end(&mut bytes)?;
        if !verified_package(&bytes, &package) {
            bail!(DOWNLOAD_FAILED);
        }
        let cache = self.cache.lock().unwrap();
        let mut installation = self.installation.lock().unwrap();
        if installation.closing || (automatic && !cache.automatic()) {
            return Ok(());
        }
        let path = self
            .directory
            .join(format!("{}.download", crate::core::id()));
        atomic(&path, &bytes)?;
        if let Some(previous) = installation.pending.replace(Pending {
            version: release.version,
            sha256: package.sha256,
            path,
            automatic,
        }) {
            let _ = fs::remove_file(previous.path);
        }
        Ok(())
    }

    pub fn install_on_exit(&self) {
        let mut installation = self.installation.lock().unwrap();
        if installation.closing {
            return;
        }
        installation.closing = true;
        let Some(update) = installation.pending.take() else {
            return;
        };
        let receipt = self.directory.join("install-result");
        if run_installer(&update, &receipt, false).is_err() {
            let _ = fs::remove_file(update.path);
            let _ = atomic(&receipt, b"failed");
        }
    }

    fn install_and_restart(&self) -> Result<()> {
        let mut installation = self.installation.lock().unwrap();
        if installation.closing {
            bail!(BUSY);
        }
        let update = installation
            .pending
            .as_ref()
            .context("Henüz indirilen bir güncelleme yok.")?;
        run_installer(update, &self.directory.join("install-result"), true)
            .context("Güncelleme kurulumu başlatılamadı.")?;
        installation.pending = None;
        installation.closing = true;
        Ok(())
    }
}

fn run_installer(update: &Pending, receipt: &Path, restart: bool) -> Result<()> {
    let destination = dunce::canonicalize(std::env::current_exe()?)?;
    if !SUPPORTED
        || !destination
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("exe"))
    {
        bail!("Unsupported Workflow executable");
    }
    let system_root =
        std::env::var_os("SystemRoot").context("Windows system directory unavailable")?;
    let powershell =
        PathBuf::from(system_root).join("System32/WindowsPowerShell/v1.0/powershell.exe");
    let script = STANDARD.encode(
        include_str!("apply_update.ps1")
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect::<Vec<_>>(),
    );
    Command::new(powershell)
        .args(["-NoProfile", "-NonInteractive", "-EncodedCommand", &script])
        .env("WORKFLOW_UPDATE_SOURCE", &update.path)
        .env("WORKFLOW_UPDATE_DESTINATION", destination)
        .env("WORKFLOW_UPDATE_SHA256", &update.sha256)
        .env("WORKFLOW_UPDATE_VERSION", &update.version)
        .env("WORKFLOW_UPDATE_INSTALLED_VERSION", VERSION)
        .env("WORKFLOW_UPDATE_PARENT", std::process::id().to_string())
        .env("WORKFLOW_UPDATE_RECEIPT", receipt)
        .env("WORKFLOW_UPDATE_RESTART", if restart { "1" } else { "0" })
        .creation_flags(CREATE_NO_WINDOW.0)
        .spawn()?;
    Ok(())
}

pub fn start(core: &Arc<Core>) {
    if !SUPPORTED || std::env::var_os("WORKFLOW_TEST_MODE").is_some() {
        return;
    }
    let weak = Arc::downgrade(core);
    tauri::async_runtime::spawn(async move {
        loop {
            let Some(core) = weak.upgrade() else { break };
            tauri::async_runtime::spawn_blocking(move || {
                let _ = core.updates.check(Mode::Automatic);
                core.emit("workflow-update", core.updates.status());
            })
            .await
            .ok();
            tokio::time::sleep(Duration::from_secs(60)).await;
        }
    });
}

pub fn dispatch(core: &Arc<Core>, action: &str, args: &Value) -> Result<Value> {
    let updates = &core.updates;
    match action {
        "status" => {}
        "automatic" => {
            let enabled = args["enabled"]
                .as_bool()
                .context("Güncelleme tercihi geçersiz.")?;
            updates.set_automatic(enabled)?;
            if enabled {
                let core = core.clone();
                tauri::async_runtime::spawn_blocking(move || {
                    let _ = core.updates.check(Mode::Automatic);
                    core.emit("workflow-update", core.updates.status());
                });
            }
        }
        "check" => updates.check(Mode::Check)?,
        "download" => updates.check(Mode::Download)?,
        "install" => {
            updates.install_and_restart()?;
            core.app.exit(0);
            return Ok(json!(true));
        }
        "open_releases" => {
            use tauri_plugin_opener::OpenerExt;
            core.app
                .opener()
                .open_url(format!("{REPO}/releases"), None::<&str>)?;
        }
        _ => bail!("Güncelleme işlemi bulunamadı."),
    }
    Ok(updates.status())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn automatic_updates_default_to_on_and_preserve_explicit_settings() {
        assert!(Cache::default().automatic());
        assert!(serde_json::from_value::<Cache>(json!({}))
            .unwrap()
            .automatic());
        assert!(serde_json::from_value::<Cache>(json!({"automatic": true}))
            .unwrap()
            .automatic());
        assert!(
            !serde_json::from_value::<Cache>(json!({"automatic": false}))
                .unwrap()
                .automatic()
        );
    }

    #[test]
    fn daily_checks_and_strict_version_ordering() {
        assert!(due(None, 100));
        assert!(!due(Some(100), 100 + DAY - 1));
        assert!(due(Some(100), 100 + DAY));
        assert!(due(Some(101), 100));
        assert!(is_newer("0.1.10", "0.1.9"));
        assert!(!is_newer("0.1.9", "0.1.9"));
        assert!(!is_newer("0.1.8", "0.1.9"));
        assert!(!is_newer("9.1.0-beta", VERSION));
        assert!(!is_newer("09.1.0", VERSION));
    }

    #[test]
    fn only_immutable_official_releases_provide_an_installable_asset() {
        let mut info = json!({
            "tag_name": "v0.1.3", "draft": false, "prerelease": false, "immutable": true,
            "body": "Synthetic release notes",
            "assets": [{
                "name": ASSET, "size": 1000, "state": "uploaded",
                "browser_download_url": download_url("0.1.3"),
                "digest": format!("sha256:{}", hash(b"synthetic executable"))
            }]
        });
        let parse = |data: &Value| release(serde_json::from_value(data.clone()).unwrap());
        assert!(parse(&info).unwrap().package.is_some());
        info["immutable"] = json!(false);
        let legacy = parse(&info).unwrap();
        assert!(legacy.package.is_none());
        assert_eq!(legacy.notes, "Synthetic release notes");
        info["immutable"] = json!(true);
        info["assets"][0]["browser_download_url"] = json!("https://example.invalid/update.exe");
        assert!(parse(&info).unwrap().package.is_none());
        info["prerelease"] = json!(true);
        assert!(parse(&info).is_none());
    }

    #[test]
    fn rejects_unverified_or_truncated_executables() {
        let bytes = b"MZ synthetic executable";
        let package = Package {
            size: bytes.len() as u64,
            sha256: hash(bytes),
        };
        assert!(verified_package(bytes, &package));
        assert!(!verified_package(b"MZ altered executable", &package));
        assert!(!verified_package(&bytes[..bytes.len() - 1], &package));
    }
}
