use crate::{
    accounts::{self, Profile},
    core::*,
    models::Rpc,
    terminal,
};
use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use serde_json::{json, Value};
use std::{
    fs,
    io::Read,
    path::PathBuf,
    process::{Command, Stdio},
    sync::{mpsc, Arc},
    time::Duration,
};

fn status(state: &str, message: &str) -> Value {
    let connection = match state {
        "missing" | "signed-out" | "expired" => state,
        _ => "unknown",
    };
    json!({"state":state,"connection":connection,"message":message,"windows":[],"updatedAt":now()})
}
fn connected(mut usage: Value) -> Value {
    usage["connection"] = json!("connected");
    usage
}
fn quota_unavailable() -> Value {
    connected(status("unavailable", "Kullanım alınamadı. Yeniden dene."))
}
fn ready(windows: Vec<Value>) -> Value {
    connected(if windows.is_empty() {
        status("unavailable", "Bu hesap kota bilgisi paylaşmıyor.")
    } else {
        json!({"state":"ready","windows":windows,"updatedAt":now()})
    })
}
fn text(v: &str) -> String {
    v.chars().filter(|c| !c.is_control()).take(80).collect()
}
fn percent(v: &Value) -> Option<f64> {
    v.as_f64()
        .filter(|v| v.is_finite() && *v >= 0.)
        .map(|v| v.min(100.))
}
fn reset(v: &Value) -> Option<String> {
    if let Some(seconds) = v.as_i64().filter(|s| *s > 0) {
        DateTime::from_timestamp(seconds, 0).map(|d| d.to_rfc3339())
    } else {
        v.as_str()
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            .map(|d| d.to_rfc3339())
    }
}
fn window(id: &str, label: &str, used: f64, resets: &Value) -> Value {
    json!({"id":id,"label":text(label),"usedPercent":used,"remainingPercent":100.-used,"resetsAt":reset(resets)})
}
fn period(minutes: &Value, fallback: &str) -> String {
    match minutes.as_u64().filter(|m| *m > 0) {
        Some(10080) => "Haftalık".into(),
        Some(1440) => "Günlük".into(),
        Some(m) if m % 1440 == 0 => format!("{} günlük", m / 1440),
        Some(m) if m % 60 == 0 => format!("{} saatlik", m / 60),
        Some(m) => format!("{m} dakikalık"),
        None => fallback.into(),
    }
}
pub(crate) fn codex_windows(result: &Value) -> Vec<Value> {
    let mut out = vec![];
    let fallback = json!({"codex":result["rateLimits"]});
    let map = result["rateLimitsByLimitId"]
        .as_object()
        .filter(|m| !m.is_empty())
        .unwrap_or_else(|| fallback.as_object().unwrap());
    for (key, bucket) in map {
        for (slot, fallback) in [("primary", "Kullanım"), ("secondary", "Ek limit")] {
            let row = &bucket[slot];
            if let Some(used) = percent(&row["usedPercent"]) {
                let period = period(&row["windowDurationMins"], fallback);
                let label = if key == "codex" {
                    period
                } else {
                    format!(
                        "{} · {period}",
                        text(bucket["limitName"].as_str().unwrap_or(key))
                    )
                };
                out.push(window(
                    &format!("{key}:{slot}"),
                    &label,
                    used,
                    &row["resetsAt"],
                ));
            }
        }
    }
    out
}
pub(crate) fn claude_windows(result: &Value) -> Vec<Value> {
    let mut out = vec![];
    for (key, value) in result.as_object().into_iter().flatten() {
        if key == "extra_usage" {
            continue;
        }
        if let Some(used) = percent(&value["utilization"]) {
            let label = match key.as_str() {
                "five_hour" => "5 saatlik".into(),
                "seven_day" => "Haftalık".into(),
                "seven_day_opus" => "Opus · Haftalık".into(),
                "seven_day_sonnet" => "Sonnet · Haftalık".into(),
                "seven_day_oauth_apps" => "Uygulamalar · Haftalık".into(),
                _ => text(&key.replace('_', " ")),
            };
            out.push(window(key, &label, used, &value["resets_at"]));
        }
    }
    let extra = &result["extra_usage"];
    if extra["is_enabled"] == true {
        if let Some(used) = percent(&extra["utilization"]) {
            out.push(window(
                "extra_usage",
                "Ek kullanım",
                used,
                &extra["resets_at"],
            ));
        } else if extra.get("monthly_limit").is_some_and(Value::is_null) {
            out.push(json!({"id":"extra_usage","label":"Ek kullanım","unlimited":true}));
        }
    }
    out.sort_by_key(|w| match w["id"].as_str().unwrap_or("") {
        "five_hour" => 0,
        "seven_day" => 1,
        _ => 2,
    });
    out
}
pub(crate) fn copilot_windows(result: &Value) -> Vec<Value> {
    let mut out = vec![];
    for (key, value) in result["quotaSnapshots"].as_object().into_iter().flatten() {
        let label = match key.as_str() {
            "premium_interactions" => "Premium istekler".into(),
            "chat" => "Sohbet".into(),
            "completions" => "Kod tamamlama".into(),
            "ai_credits" => "AI kredileri".into(),
            _ => text(&key.replace('_', " ")),
        };
        if value["isUnlimitedEntitlement"] == true
            || value["entitlementRequests"].as_f64() == Some(-1.)
        {
            out.push(json!({"id":key,"label":label,"unlimited":true}));
            continue;
        }
        let remaining = percent(&value["remainingPercentage"]).or_else(|| {
            let total = value["entitlementRequests"]
                .as_f64()
                .filter(|n| n.is_finite() && *n > 0.)?;
            let used = value["usedRequests"]
                .as_f64()
                .filter(|n| n.is_finite() && *n >= 0.)?;
            Some((100. - used / total * 100.).clamp(0., 100.))
        });
        if let Some(left) = remaining {
            out.push(window(key, &label, 100. - left, &value["resetDate"]));
        }
    }
    out.sort_by_key(|w| match w["id"].as_str().unwrap_or("") {
        "premium_interactions" => 0,
        "ai_credits" => 1,
        "chat" => 2,
        "completions" => 3,
        _ => 4,
    });
    out
}
pub(crate) fn executable(core: &Core, provider: &str) -> Option<PathBuf> {
    if std::env::var("WORKFLOW_TEST_MODE").ok().as_deref() == Some("1") {
        if let Some(exe) = std::env::var_os("WORKFLOW_TEST_RPC_EXE") {
            return Some(PathBuf::from(exe));
        }
    }
    let custom = core.db.lock().unwrap()["providers"][provider]["executable"]
        .as_str()
        .map(str::to_string);
    terminal::executable(provider, custom.as_deref())
}
fn codex_rpc(core: &Core, exe: &std::path::Path, profile: &Profile) -> Result<Rpc> {
    let mut rpc = Rpc::new("codex", exe, &core.directory, profile)?;
    rpc.request("initialize", json!({"clientInfo":{"name":"workflow_usage","version":"0.1.0"},"capabilities":{"experimentalApi":false}}))?;
    rpc.send(json!({"method":"initialized"}))?;
    Ok(rpc)
}
fn copilot_rpc(core: &Core, exe: &std::path::Path, profile: &Profile) -> Result<Rpc> {
    let mut rpc = Rpc::new("copilot", exe, &core.directory, profile)?;
    rpc.request(
        "connect",
        json!({"clientInfo":{"editorName":"Workflow","editorVersion":"0.1.0"}}),
    )?;
    Ok(rpc)
}
fn identity(provider: &str, auth: &Value) -> Option<String> {
    let key = match provider {
        "codex"
            if matches!(
                auth["account"]["type"].as_str(),
                Some("chatgpt" | "chatgptAuthTokens")
            ) =>
        {
            auth["account"]["email"].as_str().map(str::to_owned)
        }
        "claude" if auth["loggedIn"] == true && auth["authMethod"] == "claude.ai" => auth["email"]
            .as_str()
            .map(|email| format!("{email}\n{}", auth["orgId"].as_str().unwrap_or(""))),
        "copilot" if auth["isAuthenticated"] == true => auth["login"]
            .as_str()
            .or_else(|| auth["username"].as_str())
            .map(|login| format!("{login}\n{}", auth["host"].as_str().unwrap_or("github.com"))),
        _ => None,
    }?;
    (!key.trim().is_empty() && !key.starts_with('\n'))
        .then(|| hash(key.trim().to_lowercase().as_bytes()))
}
fn check_identity(profile: &Profile, provider: &str, auth: &Value) -> Option<Value> {
    profile
        .check(identity(provider, auth).as_deref())
        .err()
        .map(|error| status("signed-out", &error.to_string()))
}
fn codex(core: &Core, exe: &std::path::Path, profile: &Profile) -> Result<Value> {
    let mut rpc = codex_rpc(core, exe, profile)?;
    let before = rpc.request("account/read", json!({"refreshToken":false}))?;
    if let Some(issue) = check_identity(profile, "codex", &before) {
        return Ok(issue);
    }
    if before["account"].is_null() {
        return Ok(status("signed-out", "Kullanım için hesabına giriş yap."));
    }
    if !matches!(
        before["account"]["type"].as_str(),
        Some("chatgpt" | "chatgptAuthTokens")
    ) {
        return Ok(connected(status(
            "unsupported",
            "Bu giriş türü abonelik kotası paylaşmıyor.",
        )));
    }
    let Ok(limits) = rpc.request("account/rateLimits/read", json!({})) else {
        return Ok(quota_unavailable());
    };
    let after = rpc.request("account/read", json!({"refreshToken":false}))?;
    if before["account"] != after["account"] {
        return Ok(status("unavailable", "Hesap değişti. Kullanımı yenile."));
    }
    Ok(ready(codex_windows(&limits)))
}
fn copilot(core: &Core, exe: &std::path::Path, profile: &Profile) -> Result<Value> {
    let mut rpc = copilot_rpc(core, exe, profile)?;
    let before = rpc.request("auth.getStatus", json!({}))?;
    if let Some(issue) = check_identity(profile, "copilot", &before) {
        return Ok(issue);
    }
    if before["isAuthenticated"] != true {
        return Ok(status("signed-out", "Kullanım için hesabına giriş yap."));
    }
    let Ok(quota) = rpc.request("account.getQuota", json!({})) else {
        return Ok(quota_unavailable());
    };
    let after = rpc.request("auth.getStatus", json!({}))?;
    if before != after {
        return Ok(status("unavailable", "Hesap değişti. Kullanımı yenile."));
    }
    Ok(ready(copilot_windows(&quota)))
}
fn test_directory() -> Option<PathBuf> {
    (std::env::var("WORKFLOW_TEST_MODE").ok().as_deref() == Some("1")
        && std::env::var_os("WORKFLOW_TEST_RPC_EXE").is_some())
    .then(|| std::env::var_os("WORKFLOW_TEST_USAGE_DIR").map(PathBuf::from))
    .flatten()
}
fn claude_status(exe: &std::path::Path, profile: &Profile) -> Result<Value> {
    let args = if test_directory().is_some() {
        vec!["--usage-auth-fixture"]
    } else {
        vec!["auth", "status"]
    };
    let mut command = if cfg!(windows)
        && exe
            .extension()
            .is_some_and(|s| s == "cmd" || s == "bat" || s == "ps1")
    {
        let q = |s: &str| format!("'{}'", s.replace('\'', "''"));
        let mut c = Command::new("powershell.exe");
        c.args([
            "-NoLogo",
            "-NoProfile",
            "-Command",
            &format!(
                "& {} {};exit $LASTEXITCODE",
                q(&exe.to_string_lossy()),
                args.iter().map(|s| q(s)).collect::<Vec<_>>().join(" ")
            ),
        ]);
        c
    } else {
        let mut c = Command::new(exe);
        c.args(args);
        c
    };
    for (k, _) in std::env::vars() {
        if terminal::internal_variable(&k) {
            command.env_remove(k);
        }
    }
    profile.apply(&mut command);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000);
    }
    let mut child = command.spawn()?;
    let stdout = child.stdout.take().context("CLI çıkışı yok")?;
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut bytes = vec![];
        let result = stdout
            .take(256 * 1024 + 1)
            .read_to_end(&mut bytes)
            .map(|_| bytes);
        let _ = tx.send(result);
    });
    let output = rx.recv_timeout(Duration::from_secs(8));
    // Only this short-lived read-only probe is stopped; provider terminals are untouched.
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let _ = Command::new("taskkill.exe")
            .args(["/pid", &child.id().to_string(), "/t", "/f"])
            .creation_flags(0x08000000)
            .output();
    }
    let _ = child.kill();
    let _ = child.wait();
    let bytes = output.context("CLI hesap bilgisi gecikti")??;
    if bytes.len() > 256 * 1024 {
        bail!("CLI hesap yanıtı çok büyük")
    }
    Ok(serde_json::from_slice(&bytes)?)
}
fn credential(auth: &Value, profile: Option<&Profile>) -> Result<(String, Option<i64>)> {
    let method = auth["authMethod"].as_str().unwrap_or("");
    if !matches!(method, "claude.ai" | "oauth_token") {
        bail!("OAuth girişi değil")
    }
    if method == "oauth_token" && test_directory().is_none() {
        return Ok((
            std::env::var("CLAUDE_CODE_OAUTH_TOKEN").context("OAuth erişimi okunamıyor")?,
            None,
        ));
    }
    let directory = if let Some(directory) = profile.and_then(|p| p.directory.clone()) {
        directory
    } else if let Some(test) = test_directory() {
        test
    } else {
        auth["configDirectory"]
            .as_str()
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("CLAUDE_CONFIG_DIR").map(PathBuf::from))
            .unwrap_or_else(|| home().join(".claude"))
    };
    let raw: Value = serde_json::from_slice(&fs::read(directory.join(".credentials.json"))?)?;
    let c = &raw["claudeAiOauth"];
    Ok((
        string(c, "accessToken")?.to_string(),
        c["expiresAt"].as_i64(),
    ))
}
fn claude(exe: &std::path::Path, profile: &Profile) -> Result<Value> {
    let before = claude_status(exe, profile)?;
    if let Some(issue) = check_identity(profile, "claude", &before) {
        return Ok(issue);
    }
    if before["loggedIn"] != true {
        return Ok(status("signed-out", "Kullanım için hesabına giriş yap."));
    }
    if !matches!(
        before["authMethod"].as_str(),
        Some("claude.ai" | "oauth_token")
    ) {
        return Ok(connected(status(
            "unsupported",
            "Bu giriş türü abonelik kotası paylaşmıyor.",
        )));
    }
    let Ok((token, expires)) = credential(&before, Some(profile)) else {
        return Ok(connected(status(
            "unavailable",
            "CLI kullanım erişimi okunamıyor.",
        )));
    };
    if expires.is_some_and(|t| t <= Utc::now().timestamp_millis()) {
        return Ok(status("expired", "CLI girişinin süresi doldu."));
    }
    if token.trim().is_empty() {
        return Ok(connected(status(
            "unavailable",
            "CLI kullanım erişimi okunamıyor.",
        )));
    }
    Ok(claude_quota(exe, profile, &before, &token).unwrap_or_else(|_| quota_unavailable()))
}
fn claude_quota(
    exe: &std::path::Path,
    profile: &Profile,
    before: &Value,
    token: &str,
) -> Result<Value> {
    let address = if test_directory().is_some() {
        let u = url::Url::parse(&std::env::var("WORKFLOW_TEST_USAGE_URL")?)?;
        if u.scheme() != "http" || u.host_str() != Some("127.0.0.1") {
            bail!("Test kullanım adresi yerel olmalı")
        };
        u.to_string()
    } else {
        "https://api.anthropic.com/api/oauth/usage".into()
    };
    let client = reqwest::blocking::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(4))
        .timeout(Duration::from_secs(10))
        .build()?;
    let response = client
        .get(address)
        .bearer_auth(token)
        .header("anthropic-beta", "oauth-2025-04-20")
        .header("Accept", "application/json")
        .send()?;
    match response.status().as_u16() {
        401 | 403 => {
            return Ok(connected(status(
                "unavailable",
                "Kota erişimi doğrulanamadı.",
            )))
        }
        429 => {
            return Ok(connected(status(
                "unavailable",
                "Kullanım servisi yoğun. Biraz sonra yenile.",
            )))
        }
        200 => {}
        _ => return Ok(quota_unavailable()),
    }
    let mut body = vec![];
    response.take(256 * 1024 + 1).read_to_end(&mut body)?;
    if body.len() > 256 * 1024 {
        bail!("Kota yanıtı çok büyük")
    }
    let result: Value = serde_json::from_slice(&body)?;
    let after = claude_status(exe, profile)?;
    let current = credential(&after, Some(profile)).ok();
    if before != &after || current.as_ref().map(|c| c.0.as_str()) != Some(token) {
        return Ok(status("unavailable", "Hesap değişti. Kullanımı yenile."));
    }
    Ok(ready(claude_windows(&result)))
}
fn probe_identity(core: &Core, provider: &str, profile: &Profile) -> Result<Option<String>> {
    let exe = executable(core, provider).context("Sağlayıcının CLI kurulumu bulunamadı.")?;
    let auth = match provider {
        "codex" => codex_rpc(core, &exe, profile)?
            .request("account/read", json!({"refreshToken":false}))?,
        "claude" => claude_status(&exe, profile)?,
        _ => copilot_rpc(core, &exe, profile)?.request("auth.getStatus", json!({}))?,
    };
    Ok(identity(provider, &auth))
}
pub fn require_account(core: &Core, provider: &str, profile: &Profile) -> Result<()> {
    if profile.directory.is_none() {
        return Ok(());
    }
    let found = probe_identity(core, provider, profile).map_err(|_| {
        anyhow::anyhow!(
            "Seçili hesabın girişi doğrulanamadı. Hesaplar bölümünden girişini kontrol et."
        )
    })?;
    profile.check(found.as_deref())
}
pub fn verify_account(core: &Core, args: &Value) -> Result<Value> {
    let provider = string(args, "provider")?;
    let profile = Profile::get(core, provider, &args["accountId"])?;
    if profile.directory.is_none() {
        bail!("Doğrulanacak ek hesabı seç.");
    }
    let found = probe_identity(core, provider, &profile)
        .ok()
        .flatten()
        .context("Hesap doğrulanamadı. Sağlayıcının girişini tamamlayıp tekrar dene.")?;
    let default = Profile::get(core, provider, &Value::Null)?;
    if probe_identity(core, provider, &default)
        .ok()
        .flatten()
        .as_deref()
        == Some(found.as_str())
    {
        bail!("Bu hesap zaten varsayılan giriş olarak bağlı.");
    }
    accounts::verify(core, provider, &profile, &found)?;
    Ok(json!(true))
}
pub fn read(core: &Arc<Core>, args: &Value) -> Result<Value> {
    let provider = string(args, "provider")?;
    let profile = Profile::get(core, provider, &args["accountId"])?;
    let Some(exe) = executable(core, provider) else {
        return Ok(status("missing", "Kullanım için CLI kurulumu gerekli."));
    };
    if !profile.verified() {
        return Ok(status("signed-out", "Kullanım için hesabına giriş yap."));
    }
    let result = match provider {
        "codex" => codex(core, &exe, &profile),
        "claude" => claude(&exe, &profile),
        _ => copilot(core, &exe, &profile),
    };
    // Return normalized limits only. Never expose credentials or raw provider errors via IPC.
    Ok(result.unwrap_or_else(|_| {
        status(
            "unavailable",
            "Kullanım alınamadı. CLI girişini ve bağlantıyı kontrol et.",
        )
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn codex_uses_reported_periods_and_preserves_zero() {
        let v = json!({"rateLimitsByLimitId":{"codex":{"primary":{"usedPercent":0,"windowDurationMins":300},"secondary":{"usedPercent":80,"windowDurationMins":10080}},"other":{"limitName":"Ek","primary":{"usedPercent":50,"windowDurationMins":15}}}});
        let rows = codex_windows(&v);
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0]["remainingPercent"], 100.);
        assert_eq!(rows[1]["label"], "Haftalık");
        assert_eq!(rows[2]["label"], "Ek · 15 dakikalık");
        assert!(codex_windows(&json!({"rateLimits":{"primary":{"usedPercent":null}}})).is_empty());
    }
    #[test]
    fn claude_has_independent_windows_and_unlimited_extra() {
        let rows = claude_windows(
            &json!({"five_hour":{"utilization":15},"seven_day":{"utilization":45},"seven_day_opus":null,"extra_usage":{"is_enabled":true,"monthly_limit":null}}),
        );
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0]["remainingPercent"], 85.);
        assert_eq!(rows[1]["remainingPercent"], 55.);
        assert_eq!(rows[2]["unlimited"], true);
        assert!(credential(&json!({"authMethod":"api_key"}), None).is_err());
    }
    #[test]
    fn copilot_distinguishes_exhausted_unlimited_and_unknown() {
        let rows = copilot_windows(
            &json!({"quotaSnapshots":{"premium_interactions":{"remainingPercentage":0},"chat":{"entitlementRequests":-1},"empty":{"entitlementRequests":0,"usedRequests":0},"completions":{"entitlementRequests":100,"usedRequests":25}}}),
        );
        assert_eq!(rows.len(), 3);
        assert_eq!(
            rows.iter()
                .find(|w| w["id"] == "premium_interactions")
                .unwrap()["remainingPercent"],
            0.
        );
        assert_eq!(
            rows.iter().find(|w| w["id"] == "completions").unwrap()["remainingPercent"],
            75.
        );
        assert!(rows.iter().any(|w| w["unlimited"] == true));
        assert_eq!(ready(vec![])["state"], "unavailable");
        assert!(reset(&json!("bad timestamp")).is_none());
    }
}
