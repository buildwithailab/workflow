//! Child-process proxy settings. Never change the parent environment, provider
//! configuration, certificate validation, or Windows Internet settings.
use anyhow::{bail, Result};
use std::{path::Path, process::Command};

#[derive(Default)]
pub(crate) struct Plan(Vec<(String, String)>);

impl Plan {
    pub(crate) fn apply(&self, command: &mut Command) {
        command.envs(self.0.iter().map(|(key, value)| (key, value)));
    }
    pub(crate) fn apply_pty(&self, command: &mut portable_pty::CommandBuilder) {
        for (key, value) in &self.0 {
            command.env(key, value);
        }
    }
}

pub(crate) fn prepare(provider: &str, profile: &crate::accounts::Profile) -> Result<Plan> {
    #[cfg(windows)]
    {
        let directory = profile.config_dir();
        if explicit_proxy() || saved_proxy(provider, &directory)? {
            return Ok(Plan::default());
        }
        let urls = endpoints(provider, profile.directory.is_none())?;
        let routes = windows::cached(&urls)?;
        // A setting saved while discovery was in flight wins over discovery.
        if explicit_proxy() || saved_proxy(provider, &directory)? {
            return Ok(Plan::default());
        }
        return plan(
            &urls,
            &routes,
            std::env::var("NO_PROXY").unwrap_or_default(),
        );
    }
    #[cfg(not(windows))]
    {
        let _ = (provider, profile);
        Ok(Plan::default())
    }
}

pub(crate) fn http_client(
    builder: reqwest::blocking::ClientBuilder,
    address: &str,
    profile: &crate::accounts::Profile,
) -> Result<reqwest::blocking::ClientBuilder> {
    #[cfg(windows)]
    {
        if url::Url::parse(address)
            .ok()
            .and_then(|url| url.host_str().map(str::to_owned))
            .is_some_and(|host| {
                host.eq_ignore_ascii_case("localhost")
                    || host
                        .parse::<std::net::IpAddr>()
                        .is_ok_and(|ip| ip.is_loopback())
            })
        {
            return Ok(builder.no_proxy());
        }
        if explicit_proxy() {
            return Ok(builder);
        }
        // The quota HTTP request belongs to the same Claude account as its CLI.
        // Honor that account's explicit env settings without exporting them.
        if let Some(settings) = read_settings(&profile.config_dir())? {
            if let Some(env) = settings["env"].as_object() {
                if explicit_values(
                    env.iter()
                        .filter_map(|(key, value)| Some((key.into(), value.as_str()?.into()))),
                ) {
                    let value = |key: &str| {
                        env.get(&key.to_ascii_lowercase())
                            .or_else(|| env.get(key))
                            .and_then(|v| v.as_str())
                    };
                    let proxy = if address.starts_with("https:") {
                        value("HTTPS_PROXY")
                    } else {
                        value("HTTP_PROXY")
                    }
                    .or_else(|| value("ALL_PROXY"))
                    .filter(|s| !s.is_empty());
                    let bypass = value("NO_PROXY")
                        .map(str::to_owned)
                        .or_else(|| std::env::var("NO_PROXY").ok());
                    return explicit_http_client(builder, proxy, bypass.as_deref());
                }
            }
        }
        return system_http_client(builder,address);
    }
    #[cfg(not(windows))]
    {
        let _ = (address, profile);
        Ok(builder)
    }
}

// Direct integrations use the OS route without another provider's account settings.
pub(crate) fn system_http_client(builder:reqwest::blocking::ClientBuilder,address:&str)->Result<reqwest::blocking::ClientBuilder> {
    #[cfg(windows)] {
        let url=url::Url::parse(address).map_err(|_|anyhow::anyhow!(INVALID))?;
        if url.host_str().is_some_and(|host|host.eq_ignore_ascii_case("localhost") || host.parse::<std::net::IpAddr>().is_ok_and(|ip|ip.is_loopback())) {return Ok(builder.no_proxy());}
        if explicit_proxy(){return Ok(builder);}
        let routes=windows::cached(&[clean_endpoint(address)?])?;
        return match routes.first(){Some(Some(proxy))=>Ok(builder.no_proxy().proxy(reqwest::Proxy::all(proxy).map_err(|_|anyhow::anyhow!(INVALID))?.no_proxy(reqwest::NoProxy::from_env()))),Some(None)=>Ok(builder.no_proxy()),None=>bail!(INVALID)};
    }
    #[cfg(not(windows))] {let _=address;Ok(builder)}
}

#[cfg(windows)]
fn explicit_http_client(
    builder: reqwest::blocking::ClientBuilder,
    proxy: Option<&str>,
    bypass: Option<&str>,
) -> Result<reqwest::blocking::ClientBuilder> {
    let builder = builder.no_proxy();
    if bypass == Some("*") {
        return Ok(builder);
    }
    match proxy {
        Some(proxy) => Ok(builder.proxy(
            reqwest::Proxy::all(proxy)
                .map_err(|_| anyhow::anyhow!(SAVED))?
                .no_proxy(bypass.and_then(reqwest::NoProxy::from_string)),
        )),
        None => Ok(builder),
    }
}

const INVALID: &str =
    "Windows proxy ayarı okunamadı. Ağ/proxy yapılandırmasını kontrol edip yeniden dene.";
const INCOMPATIBLE: &str = "Windows bu sağlayıcının adresleri için farklı proxy'ler seçiyor. Sağlayıcının ağ ayarını kontrol et.";
const SAVED: &str = "Sağlayıcının kayıtlı proxy ayarı okunamadı. Ağ ayarını kontrol et.";

#[cfg(all(feature = "desktop-tests", debug_assertions))]
pub(crate) fn verify_fixture(provider: &str) -> Result<()> {
    if std::env::var("WORKFLOW_TEST_MODE").as_deref() != Ok("1") {
        return Ok(());
    }
    let Some(root) = std::env::var_os("WORKFLOW_DATA_DIR").map(std::path::PathBuf::from) else {
        return Ok(());
    };
    let expected = match std::fs::read_to_string(root.join("network-expected-proxy.txt")) {
        Ok(expected) => expected,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.into()),
    };
    let actual = std::env::var("HTTPS_PROXY").unwrap_or_default();
    if actual != if expected == "DIRECT" { "" } else { &expected } {
        bail!("Synthetic child proxy mismatch");
    }
    if expected != "DIRECT"
        && !std::env::var("NO_PROXY")
            .unwrap_or_default()
            .contains("localhost")
    {
        bail!("Synthetic loopback bypass missing");
    }
    std::fs::write(root.join(format!("network-rpc-{provider}.ok")), b"")?;
    Ok(())
}

fn explicit_proxy() -> bool {
    explicit_values(std::env::vars_os())
}
fn explicit_values(
    values: impl IntoIterator<Item = (std::ffi::OsString, std::ffi::OsString)>,
) -> bool {
    values.into_iter().any(|(key, value)| {
        let key = key.to_string_lossy();
        (["HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY"]
            .iter()
            .any(|name| key.eq_ignore_ascii_case(name))
            && !value.is_empty())
            || (key.eq_ignore_ascii_case("NO_PROXY") && value == "*")
    })
}

fn clean_endpoint(address: &str) -> Result<String> {
    let mut url = url::Url::parse(address).map_err(|_| anyhow::anyhow!(INVALID))?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        bail!(INVALID);
    }
    url.set_query(None);
    url.set_fragment(None);
    if url.as_str().len() > 2048 {
        bail!(INVALID);
    }
    Ok(url.into())
}

fn endpoints(provider: &str, inherit_parent: bool) -> Result<Vec<String>> {
    let addresses: &[&str] = match provider {
        "codex" => &[
            "https://auth.openai.com/oauth/token",
            "https://chatgpt.com/backend-api/codex",
            "https://api.openai.com/",
        ],
        "claude" => &[
            "https://claude.ai/",
            "https://console.anthropic.com/",
            "https://api.anthropic.com/",
        ],
        "copilot" => &[
            "https://github.com/login/oauth/access_token",
            "https://api.github.com/",
            "https://api.githubcopilot.com/",
            "https://api.individual.githubcopilot.com/",
            "https://api.business.githubcopilot.com/",
            "https://api.enterprise.githubcopilot.com/",
        ],
        _ => bail!(INVALID),
    };
    let mut urls: Vec<_> = addresses.iter().map(|s| s.to_string()).collect();
    let variables: &[&str] = match provider {
        "codex" => &["OPENAI_BASE_URL"],
        "claude" => &["ANTHROPIC_BASE_URL"],
        _ => &["COPILOT_GH_HOST", "GH_HOST", "COPILOT_PROVIDER_BASE_URL"],
    };
    for name in variables {
        if !inherit_parent {
            break;
        }
        if let Ok(value) = std::env::var(name) {
            if value.trim().is_empty() {
                continue;
            }
            let address = if value.contains("://") {
                value
            } else {
                format!("https://{value}/")
            };
            urls.push(clean_endpoint(&address)?);
        }
    }
    urls.sort();
    urls.dedup();
    Ok(urls)
}

fn proxy_address(value: &str) -> Result<String> {
    if value.len() > 2048 {
        bail!(INVALID);
    }
    let url = url::Url::parse(value).map_err(|_| anyhow::anyhow!(INVALID))?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || url.path() != "/"
    {
        bail!(INVALID);
    }
    Ok(url.into())
}

fn plan(urls: &[String], routes: &[Option<String>], existing_bypass: String) -> Result<Plan> {
    if urls.len() != routes.len() {
        bail!(INVALID);
    }
    let mut proxies: [Option<String>; 2] = [None, None];
    let mut direct = Vec::new();
    let mut proxied = Vec::new();
    for (address, route) in urls.iter().zip(routes) {
        let url = url::Url::parse(address).map_err(|_| anyhow::anyhow!(INVALID))?;
        let host = url
            .host_str()
            .ok_or_else(|| anyhow::anyhow!(INVALID))?
            .to_string();
        if let Some(route) = route {
            let route = proxy_address(route)?;
            let proxy = &mut proxies[usize::from(url.scheme() == "https")];
            if proxy.as_ref().is_some_and(|old| old != &route) {
                bail!(INCOMPATIBLE);
            }
            *proxy = Some(route);
            proxied.push(host);
        } else {
            direct.push(host);
        }
    }
    if proxies.iter().all(Option::is_none) {
        return Ok(Plan::default());
    }
    // Environment variables cannot express different PAC routes for two paths
    // on the same host, or multiple upstream proxies. Never choose one silently.
    if direct.iter().any(|host| proxied.contains(host)) {
        bail!(INCOMPATIBLE);
    }
    let mut bypass = vec![
        existing_bypass,
        "localhost".into(),
        "127.0.0.1".into(),
        "::1".into(),
    ];
    bypass.extend(direct);
    bypass.retain(|s| !s.is_empty());
    let mut values = Vec::new();
    for (key, proxy) in ["HTTP_PROXY", "HTTPS_PROXY"].into_iter().zip(proxies) {
        if let Some(proxy) = proxy {
            values.push((key.into(), proxy));
        }
    }
    values.push(("NO_PROXY".into(), bypass.join(",")));
    Ok(Plan(values))
}

// JSONC settings are read without ever rewriting or retaining their contents.
fn saved_proxy(provider: &str, directory: &Path) -> Result<bool> {
    if !matches!(provider, "copilot" | "claude") {
        return Ok(false);
    }
    let Some(value) = read_settings(directory)? else {
        return Ok(false);
    };
    if provider == "copilot" {
        match value.get("proxyUrl") {
            None | Some(serde_json::Value::Null) => Ok(false),
            Some(serde_json::Value::String(s)) => Ok(!s.trim().is_empty()),
            _ => bail!(SAVED),
        }
    } else {
        let env = value["env"].as_object();
        Ok(env.is_some_and(|vars| {
            explicit_values(
                vars.iter()
                    .filter_map(|(key, value)| Some((key.into(), value.as_str()?.into()))),
            )
        }))
    }
}

fn read_settings(directory: &Path) -> Result<Option<serde_json::Value>> {
    use std::io::Read;
    let file = match std::fs::File::open(directory.join("settings.json")) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => bail!(SAVED),
    };
    let mut bytes = Vec::new();
    file.take(512 * 1024 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| anyhow::anyhow!(SAVED))?;
    if bytes.len() > 512 * 1024 {
        bail!(SAVED);
    }
    let value = jsonc(&bytes)?;
    if !value.is_object() {
        bail!(SAVED);
    }
    Ok(Some(value))
}

fn jsonc(bytes: &[u8]) -> Result<serde_json::Value> {
    let bytes = bytes.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(bytes);
    let mut result = bytes.to_vec();
    let (mut i, mut string, mut escape) = (0, false, false);
    while i < result.len() {
        let b = result[i];
        if string {
            if escape {
                escape = false;
            } else if b == b'\\' {
                escape = true;
            } else if b == b'"' {
                string = false;
            }
            i += 1;
            continue;
        }
        if b == b'"' {
            string = true;
            i += 1;
            continue;
        }
        if b == b'/' && result.get(i + 1) == Some(&b'/') {
            while i < result.len() && result[i] != b'\n' {
                result[i] = b' ';
                i += 1;
            }
            continue;
        }
        if b == b'/' && result.get(i + 1) == Some(&b'*') {
            result[i] = b' ';
            result[i + 1] = b' ';
            i += 2;
            let mut closed = false;
            while i < result.len() {
                if result[i] == b'*' && result.get(i + 1) == Some(&b'/') {
                    result[i] = b' ';
                    result[i + 1] = b' ';
                    i += 2;
                    closed = true;
                    break;
                }
                result[i] = b' ';
                i += 1;
            }
            if !closed {
                bail!(SAVED);
            }
            continue;
        }
        i += 1;
    }
    let (mut string, mut escape) = (false, false);
    for i in 0..result.len() {
        let b = result[i];
        if string {
            if escape {
                escape = false;
            } else if b == b'\\' {
                escape = true;
            } else if b == b'"' {
                string = false;
            }
            continue;
        }
        if b == b'"' {
            string = true;
            continue;
        }
        if b == b','
            && result[i + 1..]
                .iter()
                .find(|b| !b.is_ascii_whitespace())
                .is_some_and(|b| matches!(b, b'}' | b']'))
        {
            result[i] = b' ';
        }
    }
    serde_json::from_slice(&result).map_err(|_| anyhow::anyhow!(SAVED))
}

#[cfg(windows)]
mod windows {
    use super::*;
    use base64::{engine::general_purpose::STANDARD, Engine};
    use std::{
        collections::VecDeque,
        io::{Read, Write},
        process::Stdio,
        sync::{
            atomic::{AtomicUsize, Ordering},
            Arc, Mutex, OnceLock,
        },
        time::{Duration, Instant},
    };

    const TIMEOUT: &str =
        "Windows proxy ayarı zamanında okunamadı. Ağ bağlantısını kontrol edip yeniden dene.";
    const BUSY: &str = "Proxy keşfi şu anda meşgul. Biraz sonra yeniden dene.";
    const SCRIPT: &str = r#"
$ErrorActionPreference='Stop'
[Console]::InputEncoding=[Text.UTF8Encoding]::new($false)
[Console]::OutputEncoding=[Text.UTF8Encoding]::new($false)
try {
  $urls=ConvertFrom-Json ([Console]::In.ReadToEnd())
  $settings=[Net.WebRequest]::DefaultWebProxy
  $routes=@()
  foreach($address in $urls) {
    $target=[uri]$address
    $proxy=if($settings){$settings.GetProxy($target)}else{$null}
    $routes+=if(!$proxy -or $proxy.AbsoluteUri -eq $target.AbsoluteUri){$null}else{$proxy.AbsoluteUri}
  }
  ConvertTo-Json -InputObject @($routes) -Compress
} catch {exit 1}
"#;
    type Outcome = std::result::Result<Vec<Option<String>>, String>;
    struct Slot {
        key: Vec<String>,
        value: Mutex<Option<(Instant, Outcome)>>,
    }
    static CACHE: OnceLock<Mutex<VecDeque<Arc<Slot>>>> = OnceLock::new();
    static ACTIVE: AtomicUsize = AtomicUsize::new(0);
    struct Permit;
    impl Drop for Permit {
        fn drop(&mut self) {
            ACTIVE.fetch_sub(1, Ordering::SeqCst);
        }
    }

    pub(super) fn cached(urls: &[String]) -> Result<Vec<Option<String>>> {
        let slot = {
            let mut cache = CACHE.get_or_init(Default::default).lock().unwrap();
            if let Some(index) = cache.iter().position(|slot| slot.key == urls) {
                let slot = cache.remove(index).unwrap();
                cache.push_front(slot.clone());
                slot
            } else {
                if cache.len() >= 16 {
                    if let Some(index) = cache.iter().rposition(|slot| Arc::strong_count(slot) == 1)
                    {
                        cache.remove(index);
                    } else {
                        bail!(BUSY);
                    }
                }
                let slot = Arc::new(Slot {
                    key: urls.to_vec(),
                    value: Mutex::new(None),
                });
                cache.push_front(slot.clone());
                slot
            }
        };
        let mut state = slot.value.lock().unwrap();
        if let Some((at, result)) = &*state {
            if at.elapsed() < Duration::from_secs(if result.is_ok() { 60 } else { 3 }) {
                return result.clone().map_err(anyhow::Error::msg);
            }
        }
        if ACTIVE
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
                (n < 4).then_some(n + 1)
            })
            .is_err()
        {
            bail!(BUSY);
        }
        let _permit = Permit;
        let result = resolve(urls).map_err(|e| e.to_string());
        *state = Some((Instant::now(), result.clone()));
        result.map_err(anyhow::Error::msg)
    }

    fn resolve(urls: &[String]) -> Result<Vec<Option<String>>> {
        #[allow(unused_mut)]
        let mut prefix = String::new();
        #[cfg(all(feature = "desktop-tests", debug_assertions))]
        if std::env::var("WORKFLOW_TEST_MODE").as_deref() == Ok("1") {
            if let Some(root) = std::env::var_os("WORKFLOW_DATA_DIR") {
                let path = std::path::PathBuf::from(root).join("network-fixture.json");
                if path.is_file() {
                    let config: serde_json::Value = serde_json::from_slice(&std::fs::read(path)?)?;
                    if config["mode"] == "fail" {
                        bail!(INVALID);
                    }
                    if config["mode"] == "timeout" {
                        prefix.push_str("Start-Sleep -Seconds 30\n");
                    } else if config["mode"] == "direct" {
                        prefix.push_str("[Net.WebRequest]::DefaultWebProxy=$null\n");
                    } else {
                        let proxy = proxy_address(
                            config["proxy"]
                                .as_str()
                                .ok_or_else(|| anyhow::anyhow!(INVALID))?,
                        )?;
                        prefix.push_str(&format!(
                            "[Net.WebRequest]::DefaultWebProxy=[Net.WebProxy]::new('{}')\n",
                            proxy.replace('\'', "''")
                        ));
                    }
                }
            }
        }
        run_helper(urls, &prefix, Duration::from_secs(6))
    }

    fn run_helper(urls: &[String], prefix: &str, timeout: Duration) -> Result<Vec<Option<String>>> {
        use std::os::windows::process::CommandExt;
        let input = serde_json::to_vec(urls)?;
        let script = format!("{prefix}{SCRIPT}");
        let bytes: Vec<u8> = script.encode_utf16().flat_map(u16::to_le_bytes).collect();
        let executable = std::env::var_os("SystemRoot")
            .map(std::path::PathBuf::from)
            .ok_or_else(|| anyhow::anyhow!(INVALID))?
            .join("System32/WindowsPowerShell/v1.0/powershell.exe");
        let mut child = Command::new(executable)
            .args([
                "-NoLogo",
                "-NoProfile",
                "-NonInteractive",
                "-EncodedCommand",
                &STANDARD.encode(bytes),
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .creation_flags(0x08000000)
            .spawn()
            .map_err(|_| anyhow::anyhow!(INVALID))?;
        let job = match crate::terminal::Job::new(child.id()) {
            Ok(job) => job,
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                bail!(INVALID);
            }
        };
        let stdout = child.stdout.take().unwrap();
        let reader = std::thread::spawn(move || {
            let mut bytes = Vec::new();
            stdout
                .take(32 * 1024 + 1)
                .read_to_end(&mut bytes)
                .map(|_| bytes)
        });
        // Writing to an anonymous pipe can block too, particularly while a PAC
        // resolver is stalled. Keep that write inside the same process deadline.
        let mut stdin = child.stdin.take().unwrap();
        let writer = std::thread::spawn(move || stdin.write_all(&input));
        let start = Instant::now();
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break Ok(status),
                Ok(None) => {}
                Err(_) => break Err(anyhow::anyhow!(INVALID)),
            }
            if start.elapsed() >= timeout {
                break Err(anyhow::anyhow!(TIMEOUT));
            }
            std::thread::sleep(Duration::from_millis(15));
        };
        drop(job);
        let _ = child.kill();
        let _ = child.wait();
        let write = writer.join().map_err(|_| anyhow::anyhow!(INVALID))?;
        let bytes = reader
            .join()
            .map_err(|_| anyhow::anyhow!(INVALID))?
            .map_err(|_| anyhow::anyhow!(INVALID))?;
        if !status?.success() || write.is_err() || bytes.len() > 32 * 1024 {
            bail!(INVALID);
        }
        let routes: Vec<Option<String>> =
            serde_json::from_slice(bytes.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(&bytes))
                .map_err(|_| anyhow::anyhow!(INVALID))?;
        if routes.len() != urls.len() {
            bail!(INVALID);
        }
        routes
            .into_iter()
            .map(|route| route.map(|s| proxy_address(&s)).transpose())
            .collect()
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        #[test]
        fn native_windows_discovery_honors_proxy_and_direct_without_changing_the_os() {
            let urls = vec![
                "https://network.example.invalid/".to_string(),
                "https://localhost/".into(),
            ];
            let prefix="[Net.WebRequest]::DefaultWebProxy=[Net.WebProxy]::new('http://127.0.0.1:43123', $true)\n";
            let routes = run_helper(&urls, prefix, Duration::from_secs(6)).unwrap();
            assert_eq!(routes, vec![Some("http://127.0.0.1:43123/".into()), None]);
            assert_eq!(
                run_helper(
                    &urls,
                    "[Net.WebRequest]::DefaultWebProxy=$null\n",
                    Duration::from_secs(6)
                )
                .unwrap(),
                vec![None, None]
            );
        }
        #[test]
        fn stalled_discovery_is_bounded_and_no_proxy_value_appears_in_errors() {
            let start = Instant::now();
            let error = run_helper(
                &["https://network.example.invalid/".into()],
                "Start-Sleep -Seconds 30\n",
                Duration::from_millis(250),
            )
            .unwrap_err();
            assert_eq!(error.to_string(), TIMEOUT);
            assert!(start.elapsed() < Duration::from_secs(3));
            let start = Instant::now();
            let urls = vec![format!("https://network.example.invalid/{}", "a".repeat(2000)); 32];
            assert_eq!(
                run_helper(
                    &urls,
                    "Start-Sleep -Seconds 30\n",
                    Duration::from_millis(250)
                )
                .unwrap_err()
                .to_string(),
                TIMEOUT
            );
            assert!(
                start.elapsed() < Duration::from_secs(3),
                "full stdin pipe must not bypass the deadline"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(windows)]
    #[test]
    fn quota_transport_uses_selected_proxy_and_keeps_loopback_direct() {
        use std::{
            io::{Read, Write},
            net::TcpListener,
            time::Duration,
        };
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let proxy = format!("http://{}", listener.local_addr().unwrap());
        let intercepted = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let mut bytes = [0; 4096];
            let count = stream.read(&mut bytes).unwrap();
            let target = String::from_utf8_lossy(&bytes[..count]);
            assert!(target.starts_with("CONNECT network.example.invalid:443 "));
            stream
                .write_all(b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\n\r\n")
                .unwrap();
        });
        let client = explicit_http_client(
            reqwest::blocking::Client::builder(),
            Some(&proxy),
            Some("localhost,127.0.0.1,::1"),
        )
        .unwrap()
        .timeout(Duration::from_secs(3))
        .build()
        .unwrap();
        assert!(client
            .get("https://network.example.invalid/")
            .send()
            .is_err());
        intercepted.join().unwrap();
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = format!("http://{}/", listener.local_addr().unwrap());
        let direct = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let mut bytes = [0; 4096];
            let count = stream.read(&mut bytes).unwrap();
            assert!(String::from_utf8_lossy(&bytes[..count]).starts_with("GET / HTTP/1.1"));
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")
                .unwrap();
        });
        assert_eq!(client.get(address).send().unwrap().status(), 200);
        direct.join().unwrap();
    }
    #[test]
    fn all_providers_keep_measured_direct_exceptions_and_loopback_outside_the_proxy() {
        for provider in ["codex", "claude", "copilot"] {
            let urls = endpoints(provider, false).unwrap();
            let mut routes = vec![Some("http://127.0.0.1:43123/".into()); urls.len()];
            routes[0] = None;
            let planned = plan(&urls, &routes, "existing.invalid".into()).unwrap();
            assert_eq!(
                planned.0[0],
                ("HTTPS_PROXY".into(), "http://127.0.0.1:43123/".into())
            );
            assert!(planned.0[1].1.contains("localhost,127.0.0.1,::1"));
            assert!(planned.0[1].1.contains("existing.invalid"));
            assert!(plan(&urls, &vec![None; urls.len()], String::new())
                .unwrap()
                .0
                .is_empty());
        }
    }
    #[test]
    fn conflicting_pac_routes_and_credential_bearing_addresses_fail_without_disclosure() {
        let urls = vec![
            "https://network.example.invalid/a".into(),
            "https://network.example.invalid/b".into(),
        ];
        assert!(plan(
            &urls,
            &[None, Some("http://127.0.0.1:43123".into())],
            String::new()
        )
        .is_err());
        assert!(plan(
            &urls,
            &[
                Some("http://127.0.0.1:43123".into()),
                Some("http://127.0.0.1:43124".into())
            ],
            String::new()
        )
        .is_err());
        assert!(proxy_address("file:///unexpected").is_err());
        assert!(proxy_address("http://account@proxy.invalid").is_err());
        assert!(clean_endpoint("https://account@service.invalid/").is_err());
        assert_eq!(
            clean_endpoint("https://network.example.invalid/path?opaque=synthetic#fragment")
                .unwrap(),
            "https://network.example.invalid/path"
        );
    }
    #[test]
    fn http_and_https_targets_keep_their_own_measured_upstream() {
        let urls = vec![
            "http://local-gateway.example.invalid/".into(),
            "https://service.example.invalid/".into(),
        ];
        let result = plan(
            &urls,
            &[
                Some("http://127.0.0.1:43123/".into()),
                Some("http://127.0.0.1:43124/".into()),
            ],
            String::new(),
        )
        .unwrap();
        assert!(result
            .0
            .contains(&("HTTP_PROXY".into(), "http://127.0.0.1:43123/".into())));
        assert!(result
            .0
            .contains(&("HTTPS_PROXY".into(), "http://127.0.0.1:43124/".into())));
        assert!(plan(
            &[
                "http://same.example.invalid/".into(),
                "https://same.example.invalid/".into()
            ],
            &[None, Some("http://127.0.0.1:43123/".into())],
            String::new()
        )
        .is_err());
    }
    #[test]
    fn explicit_copilot_jsonc_settings_are_preserved_and_malformed_settings_fail_closed() {
        let value=jsonc(br#"{ // comment
            "unrelated": "https://network.example.invalid/*literal*/", "proxyUrl": "http://proxy.invalid:3128", /* comment */
            "nested": [1,2,],
        }"#).unwrap();
        assert_eq!(value["proxyUrl"], "http://proxy.invalid:3128");
        assert_eq!(
            value["unrelated"],
            "https://network.example.invalid/*literal*/"
        );
        assert!(jsonc(b"{/* unfinished").is_err());
    }
    #[test]
    fn explicit_environment_and_saved_account_settings_take_precedence_without_rewriting_them() {
        for name in [
            "HTTPS_PROXY",
            "https_proxy",
            "HTTP_PROXY",
            "ALL_PROXY",
            "NO_PROXY",
        ] {
            let value = if name == "NO_PROXY" {
                "*"
            } else {
                "http://proxy.invalid:3128"
            };
            assert!(explicit_values([(name.into(), value.into())]));
        }
        assert!(!explicit_values([("HTTPS_PROXY".into(), "".into())]));
        let folder =
            std::env::temp_dir().join(format!("workflow-network-settings-{}", crate::core::id()));
        std::fs::create_dir(&folder).unwrap();
        let file = folder.join("settings.json");
        for (provider, content) in [
            (
                "copilot",
                br#"{"proxyUrl":"http://proxy.invalid:3128",} // preserve"#.as_slice(),
            ),
            (
                "claude",
                br#"{"env":{"HTTPS_PROXY":"http://proxy.invalid:3128"}}"#.as_slice(),
            ),
        ] {
            std::fs::write(&file, content).unwrap();
            assert!(saved_proxy(provider, &folder).unwrap());
            assert!(std::fs::read(&file).unwrap() == content);
        }
        std::fs::remove_file(file).unwrap();
        std::fs::remove_dir(folder).unwrap();
        let keys = [
            "HTTPS_PROXY",
            "HTTP_PROXY",
            "NO_PROXY",
            "NODE_EXTRA_CA_CERTS",
            "SSL_CERT_FILE",
        ];
        let before: Vec<_> = keys.iter().map(std::env::var_os).collect();
        let mut command = Command::new("synthetic-unused-command");
        Plan(vec![(
            "HTTPS_PROXY".into(),
            "http://proxy.invalid:3128".into(),
        )])
        .apply(&mut command);
        assert!(keys
            .iter()
            .zip(before)
            .all(|(key, before)| std::env::var_os(key) == before));
    }
}
