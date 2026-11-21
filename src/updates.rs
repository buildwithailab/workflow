use crate::core::{atomic, hash, Core};
use crate::update_manifest::{version, Package, Release, SignedManifest, ASSET, FEED, MAX_FEED};
use anyhow::{bail, Context, Result};
#[cfg(windows)]
use base64::{engine::general_purpose::STANDARD, Engine as _};
use reqwest::{blocking::Client, header::HeaderMap, redirect::Policy, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    fs,
    io::Read,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};
#[cfg(windows)]
use std::{os::windows::process::CommandExt, process::Command};
#[cfg(windows)]
use windows::Win32::System::Threading::CREATE_NO_WINDOW;

const REPO: &str = "https://github.com/buildwithailab/workflow";
const VERSION: &str = env!("CARGO_PKG_VERSION");
const DAY: i64 = 24 * 60 * 60;
const SUPPORTED: bool = cfg!(all(not(debug_assertions), any(
    all(windows, target_arch = "x86_64"),
    all(target_os = "macos", target_arch = "aarch64")
)));
const BUSY: &str = "Güncelleme işlemi zaten devam ediyor.";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum FailureKind {
    RateLimited,
    AccessDenied,
    NotPublished,
    Untrusted,
    Network,
    Timeout,
    Tls,
    Server,
    Storage,
    NoUpdate,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct Failure {
    kind: FailureKind,
    retry_at: Option<i64>,
    http_status: Option<u16>,
}
impl Failure {
    fn new(kind: FailureKind) -> Self {
        Self {
            kind,
            retry_at: None,
            http_status: None,
        }
    }
    fn error(kind: FailureKind) -> anyhow::Error {
        Self::new(kind).into()
    }
}
impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self.kind {
            FailureKind::RateLimited => "GitHub istek sınırına ulaşıldı. Belirtilen zamandan sonra tekrar dene.",
            FailureKind::AccessDenied => "Güncelleme sunucusuna erişim engellendi. Ağ veya proxy ayarlarını kontrol et.",
            FailureKind::NotPublished => "İmzalı güncelleme bilgisi henüz yayımlanmamış. GitHub sürümlerini kontrol et.",
            FailureKind::Untrusted => "Güncelleme bilgisi veya dosyası doğrulanamadı. Kurulum yapılmadı.",
            FailureKind::Network => "Güncelleme sunucusuna bağlanılamadı. İnternet veya proxy ayarlarını kontrol et.",
            FailureKind::Timeout => "Güncelleme sunucusu zamanında yanıt vermedi. Daha sonra tekrar dene.",
            FailureKind::Tls => "Güncelleme bağlantısının güvenlik sertifikası doğrulanamadı. Sistem tarihi ve ağ ayarlarını kontrol et.",
            FailureKind::Server => "Güncelleme sunucusu hata döndürdü. Daha sonra tekrar dene.",
            FailureKind::NoUpdate => "Bu sürüm için kurulabilir güncelleme bulunamadı.",
            FailureKind::Storage => "Güncelleme dosyası veya ayarları kaydedilemedi. Klasör izinlerini ve boş alanı kontrol et.",
        })
    }
}
impl std::error::Error for Failure {}

fn response_failure(status: StatusCode, headers: &HeaderMap, now: i64) -> Failure {
    let seconds = headers
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| {
            v.parse::<i64>()
                .ok()
                .filter(|v| *v >= 0)
                .map(|v| now.saturating_add(v))
                .or_else(|| {
                    chrono::DateTime::parse_from_rfc2822(v)
                        .ok()
                        .map(|v| v.timestamp())
                })
        });
    let remaining_zero = headers
        .get("x-ratelimit-remaining")
        .is_some_and(|v| v == "0");
    let limited = status == StatusCode::TOO_MANY_REQUESTS
        || (status == StatusCode::FORBIDDEN
            && (remaining_zero || headers.contains_key("retry-after")));
    let mut failure = Failure::new(if limited {
        FailureKind::RateLimited
    } else if matches!(
        status,
        StatusCode::FORBIDDEN
            | StatusCode::UNAUTHORIZED
            | StatusCode::PROXY_AUTHENTICATION_REQUIRED
    ) {
        FailureKind::AccessDenied
    } else if status.is_server_error() {
        FailureKind::Server
    } else {
        FailureKind::Untrusted
    });
    failure.http_status = Some(status.as_u16());
    if limited {
        let reset = remaining_zero
            .then(|| {
                headers
                    .get("x-ratelimit-reset")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.parse::<i64>().ok())
            })
            .flatten();
        let retry = seconds.into_iter().chain(reset).max().unwrap_or(now + 60);
        failure.retry_at = Some(retry.max(now + 1).min(now.saturating_add(7 * DAY)));
    }
    failure
}

fn transport_failure(error: &(dyn std::error::Error + 'static)) -> anyhow::Error {
    let mut kind = FailureKind::Network;
    let mut source = Some(error);
    while let Some(current) = source {
        if let Some(http) = current.downcast_ref::<reqwest::Error>() {
            if http.is_timeout() {
                kind = FailureKind::Timeout;
                break;
            }
            if http.is_redirect() {
                kind = FailureKind::Untrusted;
                break;
            }
        }
        if let Some(io) = current.downcast_ref::<std::io::Error>() {
            if io.kind() == std::io::ErrorKind::TimedOut {
                kind = FailureKind::Timeout;
                break;
            }
            if let Some(inner) = io.get_ref() {
                source = Some(inner);
                continue;
            }
        }
        let description = current.to_string().to_ascii_lowercase();
        if ["certificate", "tls", "ssl", "schannel"]
            .iter()
            .any(|word| description.contains(word))
        {
            kind = FailureKind::Tls;
        }
        source = current.source();
    }
    // Keep raw transport/proxy strings out of status, logs and persisted errors.
    Failure::error(kind)
}

#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(default)]
struct Cache {
    automatic: Option<bool>,
    checked_at: Option<i64>,
    installed: Option<Release>,
    latest: Option<Release>,
    error: Option<String>,
    failure: Option<Failure>,
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

fn is_newer(candidate: &str, installed: &str) -> bool {
    matches!((version(candidate), version(installed)), (Some(a), Some(b)) if a > b)
}

fn due(last: Option<i64>, now: i64) -> bool {
    last.is_none_or(|last| last > now || now.saturating_sub(last) >= DAY)
}

fn download_url(version: &str) -> String {
    format!("{REPO}/releases/download/v{version}/{ASSET}")
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
        .build()
        .map_err(|error| transport_failure(&error))?)
}

fn metadata_url(reference: &str) -> Option<String> {
    if reference == "latest" {
        return Some(format!("{REPO}/releases/latest/download/{FEED}"));
    }
    let tag = reference.strip_prefix("tags/v")?;
    version(tag)?;
    Some(format!("{REPO}/releases/download/v{tag}/{FEED}"))
}

fn read_release(client: &Client, reference: &str) -> Result<Option<Release>> {
    let address = metadata_url(reference).ok_or_else(|| Failure::error(FailureKind::Untrusted))?;
    let response = client
        .get(address)
        .timeout(Duration::from_secs(15))
        .send()
        .map_err(|error| transport_failure(&error))?;
    parse_release_response(response, reference)
}

fn parse_release_response(
    mut response: reqwest::blocking::Response,
    reference: &str,
) -> Result<Option<Release>> {
    if response.status() == StatusCode::NOT_FOUND {
        return Ok(None);
    }
    if !response.status().is_success() {
        return Err(response_failure(
            response.status(),
            response.headers(),
            chrono::Utc::now().timestamp(),
        )
        .into());
    }
    if response
        .content_length()
        .is_some_and(|size| size > MAX_FEED as u64)
    {
        return Err(Failure::error(FailureKind::Untrusted));
    }
    let mut body = Vec::new();
    (&mut response)
        .take(MAX_FEED as u64 + 1)
        .read_to_end(&mut body)
        .map_err(|error| transport_failure(&error))?;
    let release =
        SignedManifest::parse(&body).ok_or_else(|| Failure::error(FailureKind::Untrusted))?;
    if reference != "latest" && reference.strip_prefix("tags/v") != Some(release.version.as_str()) {
        return Err(Failure::error(FailureKind::Untrusted));
    }
    Ok(Some(release))
}

fn verified_package(bytes: &[u8], package: &Package) -> bool {
    let format = if cfg!(target_os = "macos") { bytes.starts_with(b"PK\x03\x04") }
        else { bytes.starts_with(b"MZ") };
    bytes.len() as u64 == package.size && format && hash(bytes) == package.sha256
}

impl Updates {
    pub fn load(profile: &Path) -> Self {
        let directory = profile.join("updates");
        let mut cache: Cache = fs::File::open(directory.join("state.json"))
            .ok()
            .and_then(|file| {
                let mut bytes = Vec::new();
                file.take(1024 * 1024 + 1).read_to_end(&mut bytes).ok()?;
                (bytes.len() <= 1024 * 1024).then_some(bytes)
            })
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
        let visible_failure = cache
            .failure
            .as_ref()
            .filter(|failure| cache.error.as_deref() == Some(failure.to_string().as_str()));
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
                "downloadable": SUPPORTED && release.trusted_package().is_some()
            })),
            "pendingVersion": installation.pending.as_ref().map(|update| update.version.as_str()),
            "error": cache.error,
            "errorKind": visible_failure.map(|failure| failure.kind),
            "retryAt": visible_failure.and_then(|failure| failure.retry_at),
            "httpStatus": visible_failure.and_then(|failure| failure.http_status)
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
            if let Some(failure) = cache.failure.as_ref().filter(|failure| {
                failure.kind == FailureKind::RateLimited
                    && failure.retry_at.is_some_and(|retry| retry > now)
            }) {
                return if mode == Mode::Automatic {
                    Ok(())
                } else {
                    Err(failure.clone().into())
                };
            }
            let retry_due = cache
                .failure
                .as_ref()
                .and_then(|failure| failure.retry_at)
                .is_some_and(|retry| retry <= now);
            if mode == Mode::Automatic
                && (!SUPPORTED || !cache.automatic() || (!due(cache.checked_at, now) && !retry_due))
            {
                return Ok(());
            }
        }
        self.finish_check(now, self.fetch(mode))
    }

    fn finish_check(&self, now: i64, result: Result<()>) -> Result<()> {
        let mut cache = self.cache.lock().unwrap();
        cache.checked_at = Some(now);
        cache.failure = result.as_ref().err().map(|error| {
            error
                .downcast_ref::<Failure>()
                .cloned()
                .unwrap_or_else(|| Failure::new(FailureKind::Storage))
        });
        if let Some(failure) = &mut cache.failure {
            if matches!(
                failure.kind,
                FailureKind::Network
                    | FailureKind::Tls
                    | FailureKind::Timeout
                    | FailureKind::Server
            ) {
                failure.retry_at = Some(now + 5 * 60);
            }
        }
        cache.error = cache.failure.as_ref().map(ToString::to_string);
        if self.save(&cache).is_err() {
            let failure = Failure::new(FailureKind::Storage);
            cache.error = Some(failure.to_string());
            cache.failure = Some(failure.clone());
            return Err(failure.into());
        }
        result
    }

    fn fetch(&self, mode: Mode) -> Result<()> {
        let client = client()?;
        let latest = Some(
            read_release(&client, "latest")?
                .ok_or_else(|| Failure::error(FailureKind::NotPublished))?,
        );
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
        let candidate = latest.filter(|release| {
            is_newer(&release.version, VERSION) && release.trusted_package().is_some()
        });
        let automatic = mode != Mode::Download;
        if SUPPORTED && (!automatic || self.cache.lock().unwrap().automatic()) {
            if let Some(release) = candidate {
                return self.download(&client, release, automatic);
            }
        }
        if mode == Mode::Download {
            return Err(Failure::error(FailureKind::NoUpdate));
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
        let package = release
            .trusted_package()
            .ok_or_else(|| Failure::error(FailureKind::Untrusted))?;
        let mut response = client
            .get(download_url(&release.version))
            .send()
            .map_err(|error| transport_failure(&error))?;
        if !response.status().is_success() {
            return Err(response_failure(
                response.status(),
                response.headers(),
                chrono::Utc::now().timestamp(),
            )
            .into());
        }
        if response
            .content_length()
            .is_some_and(|size| size != package.size)
        {
            return Err(Failure::error(FailureKind::Untrusted));
        }
        let mut bytes = Vec::new();
        (&mut response)
            .take(package.size + 1)
            .read_to_end(&mut bytes)
            .map_err(|error| transport_failure(&error))?;
        if !verified_package(&bytes, &package) {
            return Err(Failure::error(FailureKind::Untrusted));
        }
        let cache = self.cache.lock().unwrap();
        let mut installation = self.installation.lock().unwrap();
        if installation.closing || (automatic && !cache.automatic()) {
            return Ok(());
        }
        let path = self
            .directory
            .join(format!("{}.download", crate::core::id()));
        atomic(&path, &bytes).map_err(|_| Failure::error(FailureKind::Storage))?;
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

#[cfg(windows)]
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

#[cfg(target_os = "macos")]
fn run_installer(update: &Pending, receipt: &Path, restart: bool) -> Result<()> {
    if !SUPPORTED { bail!("Unsupported Workflow application"); }
    crate::platform::macos::update_installer(
        &update.path, receipt, restart, &update.sha256, &update.version,
    )
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
    use reqwest::blocking::Response;
    use std::{
        io::{BufRead, BufReader, Write},
        net::TcpListener,
        sync::mpsc,
        thread,
    };

    fn with_loopback_response(wire_response: Vec<u8>, check: impl FnOnce(Response)) {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let (finish, finished) = mpsc::channel();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            {
                let mut request = BufReader::new(&mut stream);
                loop {
                    let mut line = String::new();
                    let count = request.read_line(&mut line).unwrap();
                    if count == 0 || line == "\r\n" {
                        break;
                    }
                }
            }
            stream.write_all(&wire_response).unwrap();
            // Keep the connection open so an incomplete response exercises the read timeout.
            let _ = finished.recv_timeout(Duration::from_secs(5));
        });
        let response = Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(2))
            .build()
            .unwrap()
            .get(format!("http://{address}/"))
            .send()
            .unwrap();
        check(response);
        finish.send(()).unwrap();
        server.join().unwrap();
    }

    fn assert_failure(result: Result<Option<Release>>, kind: FailureKind) {
        let Err(error) = result else {
            panic!("Expected an update error");
        };
        assert_eq!(error.downcast_ref::<Failure>().unwrap().kind, kind);
    }

    #[test]
    fn metadata_urls_use_public_release_assets() {
        #[cfg(windows)]
        {
        assert_eq!(
            metadata_url("latest").as_deref(),
            Some("https://github.com/buildwithailab/workflow/releases/latest/download/Workflow-Update-Windows-x64.json")
        );
        assert_eq!(
            metadata_url("tags/v1.2.3").as_deref(),
            Some("https://github.com/buildwithailab/workflow/releases/download/v1.2.3/Workflow-Update-Windows-x64.json")
        );
        assert_eq!(
            download_url("1.2.3"),
            "https://github.com/buildwithailab/workflow/releases/download/v1.2.3/Workflow-Update-Windows-x64.exe"
        );
        }
        #[cfg(target_os = "macos")]
        {
            assert_eq!(metadata_url("latest").unwrap(),
                "https://github.com/buildwithailab/workflow/releases/latest/download/Workflow-Update-macOS-arm64.json");
            assert_eq!(metadata_url("tags/v1.2.3").unwrap(),
                "https://github.com/buildwithailab/workflow/releases/download/v1.2.3/Workflow-Update-macOS-arm64.json");
            assert_eq!(download_url("1.2.3"),
                "https://github.com/buildwithailab/workflow/releases/download/v1.2.3/Workflow-Update-macOS-arm64.app.zip");
        }
        assert!(metadata_url("tags/v1.2.3-rc").is_none());
        assert!(metadata_url("tags/v1.2.3/../latest").is_none());
    }

    #[test]
    fn http_failures_distinguish_rate_limits_access_denial_and_server_errors() {
        let now = 1_700_000_000;
        let mut headers = HeaderMap::new();
        headers.insert("x-ratelimit-remaining", "0".parse().unwrap());
        headers.insert("x-ratelimit-reset", "1700000120".parse().unwrap());
        headers.insert("retry-after", "90".parse().unwrap());
        let quota = response_failure(StatusCode::FORBIDDEN, &headers, now);
        assert_eq!(quota.kind, FailureKind::RateLimited);
        assert_eq!(quota.http_status, Some(403));
        assert_eq!(quota.retry_at, Some(now + 120));

        headers.clear();
        let rate_limit = response_failure(StatusCode::TOO_MANY_REQUESTS, &headers, now);
        assert_eq!(rate_limit.kind, FailureKind::RateLimited);
        assert_eq!(rate_limit.http_status, Some(429));
        assert_eq!(rate_limit.retry_at, Some(now + 60));

        for status in [
            StatusCode::FORBIDDEN,
            StatusCode::UNAUTHORIZED,
            StatusCode::PROXY_AUTHENTICATION_REQUIRED,
        ] {
            let denied = response_failure(status, &headers, now);
            assert_eq!(denied.kind, FailureKind::AccessDenied);
            assert_eq!(denied.http_status, Some(status.as_u16()));
            assert_eq!(denied.retry_at, None);
        }
        let unavailable = response_failure(StatusCode::SERVICE_UNAVAILABLE, &headers, now);
        assert_eq!(unavailable.kind, FailureKind::Server);
        assert_eq!(unavailable.http_status, Some(503));
        assert_eq!(unavailable.retry_at, None);
    }

    #[test]
    fn release_response_distinguishes_missing_and_untrusted_metadata() {
        with_loopback_response(
            b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n".to_vec(),
            |response| {
                assert!(matches!(
                    parse_release_response(response, "latest"),
                    Ok(None)
                ))
            },
        );
        with_loopback_response(
            b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}".to_vec(),
            |response| {
                assert_failure(
                    parse_release_response(response, "latest"),
                    FailureKind::Untrusted,
                );
            },
        );
        with_loopback_response(
            format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n",
                MAX_FEED + 1
            )
            .into_bytes(),
            |response| {
                assert_failure(
                    parse_release_response(response, "latest"),
                    FailureKind::Untrusted,
                );
            },
        );
    }

    #[test]
    fn release_response_reports_a_body_read_timeout() {
        with_loopback_response(
            b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{".to_vec(),
            |response| {
                assert_failure(
                    parse_release_response(response, "latest"),
                    FailureKind::Timeout,
                );
            },
        );
    }

    #[test]
    fn live_rate_backoff_defers_automatic_and_manual_update_requests() {
        let profile = std::env::temp_dir().join(crate::core::id());
        let updates = Updates::load(&profile);
        let now = chrono::Utc::now().timestamp();
        let last_check = now - 2 * DAY;
        let retry_at = now + 300;
        let rate_limit = Failure {
            kind: FailureKind::RateLimited,
            retry_at: Some(retry_at),
            http_status: Some(429),
        };
        {
            let mut cache = updates.cache.lock().unwrap();
            cache.checked_at = Some(last_check);
            cache.failure = Some(rate_limit.clone());
            cache.error = Some(rate_limit.to_string());
        }

        updates.check(Mode::Automatic).unwrap();
        for mode in [Mode::Check, Mode::Download] {
            let error = updates.check(mode).unwrap_err();
            let failure = error.downcast_ref::<Failure>().unwrap();
            assert_eq!(failure.kind, FailureKind::RateLimited);
            assert_eq!(failure.retry_at, Some(retry_at));
        }

        let status = updates.status();
        assert_eq!(status["errorKind"], "rate_limited");
        assert_eq!(status["checkedAt"], last_check);
        assert_eq!(status["retryAt"], retry_at);
        assert_eq!(status["httpStatus"], 429);
        assert!(!profile.exists());
    }

    #[test]
    fn storage_save_failure_appears_in_update_status() {
        let profile = std::env::temp_dir().join(crate::core::id());
        fs::write(&profile, b"synthetic file blocking the profile directory").unwrap();
        let updates = Updates::load(&profile);
        let now = 1_700_000_000;

        let result = updates.finish_check(now, Ok(()));
        let status = updates.status();
        fs::remove_file(&profile).unwrap();

        let failure = result.unwrap_err().downcast::<Failure>().unwrap();
        assert_eq!(failure.kind, FailureKind::Storage);
        assert_eq!(status["error"], failure.to_string());
        assert_eq!(status["errorKind"], "storage");
        assert_eq!(status["checkedAt"], now);
        assert!(status["httpStatus"].is_null());
        assert!(status["retryAt"].is_null());
    }

    #[test]
    fn installer_receipt_hides_obsolete_http_details_and_preserves_backoff() {
        let profile = std::env::temp_dir().join(crate::core::id());
        let updates = Updates::load(&profile);
        let now = chrono::Utc::now().timestamp();
        let retry_at = now + 300;
        let failure = Failure {
            kind: FailureKind::RateLimited,
            retry_at: Some(retry_at),
            http_status: Some(429),
        };
        updates
            .save(&Cache {
                checked_at: Some(now),
                error: Some(failure.to_string()),
                failure: Some(failure),
                ..Cache::default()
            })
            .unwrap();
        let receipt = updates.directory.join("install-result");
        fs::write(&receipt, b"failed").unwrap();

        let restored = Updates::load(&profile);
        assert!(!receipt.exists());
        fs::remove_file(updates.directory.join("state.json")).unwrap();
        fs::remove_dir(&updates.directory).unwrap();
        fs::remove_dir(&profile).unwrap();

        let status = restored.status();
        assert_eq!(status["error"], "Güncelleme kurulamadı.");
        assert!(status["errorKind"].is_null());
        assert!(status["httpStatus"].is_null());
        assert!(status["retryAt"].is_null());
        let cache = restored.cache.lock().unwrap();
        let suppressed = cache.failure.as_ref().unwrap();
        assert_eq!(suppressed.kind, FailureKind::RateLimited);
        assert_eq!(suppressed.retry_at, Some(retry_at));
    }

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
    fn rejects_unverified_or_truncated_executables() {
        let bytes: &[u8] = if cfg!(target_os = "macos") { b"PK\x03\x04 synthetic application archive" }
            else { b"MZ synthetic executable" };
        let package = Package {
            size: bytes.len() as u64,
            sha256: hash(bytes),
        };
        assert!(verified_package(bytes, &package));
        assert!(!verified_package(b"altered package", &package));
        assert!(!verified_package(&bytes[..bytes.len() - 1], &package));
        let foreign: &[u8] = if cfg!(target_os = "macos") { b"MZ foreign executable" }
            else { b"PK\x03\x04 foreign archive" };
        let foreign_package = Package { size: foreign.len() as u64, sha256: hash(foreign) };
        assert!(!verified_package(foreign, &foreign_package));
    }
}
