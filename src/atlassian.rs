//! Read-only Jira/Confluence Data Center APIs. Secrets live only in the OS vault.
use crate::{core::*, credential_store, provider_network};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    io::Read,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};
use url::Url;

const INVALID: &str = "Jira/Confluence bağlantı ayarı geçersiz.";
const CHANGED: &str = "Bağlantı değişti veya kaldırıldı. İşlemi yeniden dene.";
const NETWORK: &str =
    "Sunucuya güvenli bağlantı kurulamadı. Ağ, proxy ve sertifika ayarlarını kontrol et.";
const RESPONSE: &str =
    "Sunucu beklenen API yanıtını vermedi. Uygulamanın temel adresini kontrol et.";
#[derive(Default)]
pub(crate) struct State {
    gate: Mutex<()>,
    active: AtomicUsize,
}
struct Permit<'a>(&'a AtomicUsize);
impl Drop for Permit<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}
impl State {
    fn enter(&self) -> Result<Permit<'_>> {
        if self
            .active
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
                (n < 4).then_some(n + 1)
            })
            .is_err()
        {
            bail!("Çok sayıda bağlantı isteği var. Biraz sonra yeniden dene.");
        }
        Ok(Permit(&self.active))
    }
}
#[derive(Deserialize, Serialize)]
struct Connection {
    base: String,
    token: String,
    revision: String,
}
impl Drop for Connection {
    fn drop(&mut self) {
        unsafe {
            self.token.as_bytes_mut().fill(0);
        }
    }
}
fn kind(value: &str) -> Result<&str> {
    if matches!(value, "jira" | "confluence") {
        Ok(value)
    } else {
        bail!(INVALID)
    }
}
fn base(value: &str) -> Result<Url> {
    if value.len() > 1024 || value.chars().any(char::is_control) {
        bail!(INVALID);
    }
    let parsed = Url::parse(value.trim()).map_err(|_| anyhow::anyhow!(INVALID))?;
    let fixture = cfg!(test)
        || cfg!(all(feature = "desktop-tests", debug_assertions))
            && std::env::var("WORKFLOW_TEST_MODE").as_deref() == Ok("1");
    let local = parsed.host_str().is_some_and(|h| {
        h.parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
    });
    if (parsed.scheme() != "https" && !(fixture && parsed.scheme() == "http" && local))
        || parsed.host_str().is_none()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
    {
        bail!("Temel adres HTTPS olmalı; kullanıcı bilgisi, sorgu veya parça içermemeli.");
    }
    Url::parse(&format!("{}/", parsed.as_str().trim_end_matches('/')))
        .map_err(|_| anyhow::anyhow!(INVALID))
}
fn validate_token(value: &str) -> Result<()> {
    if value.is_empty() || value.len() > 1024 || !value.bytes().all(|b| (33..=126).contains(&b)) {
        bail!("Token boş olamaz ve boşluk veya satır sonu içeremez.");
    }
    Ok(())
}
fn target(core: &Core, product: &str) -> Result<String> {
    kind(product)?;
    let directory = dunce::canonicalize(&core.directory).map_err(|_| anyhow::anyhow!(INVALID))?;
    Ok(format!(
        "local.workflow.desktop.atlassian.{}.{product}",
        hash(external_path(&directory).to_string_lossy().as_bytes())
    ))
}
fn load(core: &Core, product: &str) -> Result<Option<Connection>> {
    let Some(mut text) = credential_store::read(&target(core, product)?)? else {
        return Ok(None);
    };
    let parsed = serde_json::from_str::<Connection>(&text);
    unsafe {
        text.as_bytes_mut().fill(0);
    }
    let saved = parsed.map_err(|_| anyhow::anyhow!(credential_store::ERROR))?;
    if base(&saved.base)?.as_str() != saved.base
        || validate_token(&saved.token).is_err()
        || uuid::Uuid::parse_str(&saved.revision).is_err()
    {
        bail!(credential_store::ERROR);
    }
    Ok(Some(saved))
}
fn public(saved: Option<&Connection>) -> Value {
    match saved {
        Some(s) => json!({"configured":true,"baseUrl":s.base,"access":"read-only"}),
        None => json!({"configured":false,"baseUrl":"","access":"read-only"}),
    }
}
fn unchanged(core: &Core, product: &str, revision: &str) -> Result<()> {
    if load(core, product)?.is_none_or(|current| current.revision != revision) {
        bail!(CHANGED);
    }
    Ok(())
}
pub(crate) fn dispatch(core: &Arc<Core>, action: &str, args: &Value) -> Result<Value> {
    if action == "status" {
        let _guard = core.atlassian.gate.lock().unwrap();
        let mut statuses = json!({});
        for product in ["jira", "confluence"] {
            statuses[product] = match load(core, product) {
                Ok(s) => public(s.as_ref()),
                Err(_) => {
                    json!({"configured":false,"unavailable":true,"message":credential_store::ERROR})
                }
            };
        }
        return Ok(statuses);
    }
    let product = kind(string(args, "kind")?)?;
    if action == "remove" {
        let _guard = core.atlassian.gate.lock().unwrap();
        credential_store::remove(&target(core, product)?)?;
        return Ok(json!(true));
    }
    if !matches!(action, "test" | "save") {
        bail!(INVALID);
    }
    let _permit = core.atlassian.enter()?;
    let address = base(string(args, "baseUrl")?)?;
    let old = {
        let _guard = core.atlassian.gate.lock().unwrap();
        load(core, product)?
    };
    let supplied = args["token"].as_str().unwrap_or("");
    let token = if supplied.is_empty() {
        let saved = old
            .as_ref()
            .filter(|s| s.base == address.as_str())
            .context("Yeni adres için token gir. Kayıtlı token başka bir sunucuya gönderilmez.")?;
        saved.token.clone()
    } else {
        validate_token(supplied)?;
        supplied.to_owned()
    };
    let saved = Connection {
        base: address.to_string(),
        token,
        revision: id(),
    };
    verify(product, &saved)?;
    let _guard = core.atlassian.gate.lock().unwrap();
    if load(core, product)?.as_ref().map(|s| s.revision.clone())
        != old.as_ref().map(|s| s.revision.clone())
    {
        bail!(CHANGED);
    }
    if action == "save" {
        let mut encoded = serde_json::to_string(&saved)?;
        let result = credential_store::write(&target(core, product)?, &encoded);
        unsafe {
            encoded.as_bytes_mut().fill(0);
        }
        result?;
    }
    Ok(json!({"connected":true,"saved":action=="save","connection":public(Some(&saved))}))
}
fn get(saved: &Connection, endpoint: &str, query: &[(&str, String)]) -> Result<Value> {
    let origin = base(&saved.base)?;
    let mut address = origin
        .join(endpoint)
        .map_err(|_| anyhow::anyhow!(INVALID))?;
    if address.origin() != origin.origin() || !address.path().starts_with(origin.path()) {
        bail!(INVALID);
    }
    address
        .query_pairs_mut()
        .extend_pairs(query.iter().map(|(k, v)| (*k, v.as_str())));
    let client = provider_network::system_http_client(
        reqwest::blocking::Client::builder(),
        address.as_str(),
    )?
    .redirect(reqwest::redirect::Policy::none())
    .connect_timeout(Duration::from_secs(4))
    .timeout(Duration::from_secs(8))
    .build()
    .map_err(|_| anyhow::anyhow!(NETWORK))?;
    let mut authorization =
        reqwest::header::HeaderValue::from_str(&format!("Bearer {}", saved.token))
            .map_err(|_| anyhow::anyhow!(INVALID))?;
    authorization.set_sensitive(true);
    let response = client
        .get(address)
        .header(reqwest::header::AUTHORIZATION, authorization)
        .header(reqwest::header::ACCEPT, "application/json")
        .send()
        .map_err(|_| anyhow::anyhow!(NETWORK))?;
    match response.status().as_u16(){
        200=>{},401=>bail!("Token geçersiz veya süresi dolmuş. Bağlantıyı yeniden kaydet."),403=>bail!("Bu içeriği okumak için yetkin yok."),
        404=>bail!("İçerik bulunamadı veya erişim yetkin yok. Adresi ve içerik kimliğini kontrol et."),429=>bail!("Sunucu istek sınırına ulaşıldı. Daha sonra yeniden dene."),
        300..=399=>bail!("API isteği başka adrese yönlendirildi. Temel adresi ve token ile API erişimini kontrol et."),_=>bail!("Sunucu API isteğini tamamlayamadı. Daha sonra yeniden dene.")
    }
    let mut bytes = Vec::new();
    response
        .take(1024 * 1024 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| anyhow::anyhow!(NETWORK))?;
    if bytes.len() > 1024 * 1024 {
        bail!("API yanıtı çok büyük. Aramayı daralt veya sonraki sayfayı kullan.");
    }
    serde_json::from_slice(&bytes).map_err(|_| anyhow::anyhow!(RESPONSE))
}
fn verify(product: &str, saved: &Connection) -> Result<()> {
    let v = get(
        saved,
        if product == "jira" {
            "rest/api/2/myself"
        } else {
            "rest/api/user/current"
        },
        &[],
    )?;
    if !v.is_object()
        || v["type"] == "anonymous"
        || !["name", "displayName", "userKey", "username"]
            .iter()
            .any(|key| v[*key].as_str().is_some_and(|s| !s.is_empty()))
    {
        bail!(RESPONSE);
    }
    Ok(())
}
fn text(value: &Value, limit: usize) -> String {
    value
        .as_str()
        .unwrap_or("")
        .chars()
        .filter(|c| !c.is_control() || matches!(c, '\n' | '\t'))
        .take(limit)
        .collect()
}
fn issue_key(value: &Value) -> Result<String> {
    let key = value.as_str().unwrap_or("").to_ascii_uppercase();
    if key.len() > 80
        || !regex::Regex::new(r"^[A-Z][A-Z0-9_]*-[1-9][0-9]*$")
            .unwrap()
            .is_match(&key)
    {
        bail!("Görev anahtarı geçersiz.");
    }
    Ok(key)
}
fn page_number(value: &Value) -> Result<u64> {
    match value {
        Value::Null => Ok(0),
        v => v
            .as_u64()
            .filter(|n| *n <= 100000)
            .context("Sayfalama değeri geçersiz."),
    }
}
fn person(value: &Value) -> Value {
    json!({"name":text(&value["displayName"],160)})
}
fn brief_issue(value: &Value) -> Value {
    let f = &value["fields"];
    json!({"key":text(&value["key"],80),"summary":text(&f["summary"],1000),"status":text(&f["status"]["name"],160),"type":text(&f["issuetype"]["name"],160)})
}
fn page_list<'a>(v: &'a Value, field: &str) -> Result<&'a Vec<Value>> {
    v[field]
        .as_array()
        .filter(|rows| rows.len() <= 20)
        .context(RESPONSE)
}
fn search_query(args: &Value, key: &str) -> Result<String> {
    let v = string(args, key)?;
    if v.trim().is_empty() || v.len() > 2000 || v.chars().any(char::is_control) {
        bail!("Arama sorgusu geçersiz.");
    }
    Ok(v.into())
}
fn plain_html(value: &str) -> String {
    // Retrieved markup is reference text, never evaluated in an application view.
    let blocks = regex::Regex::new(r"(?is)<(script|style)\b[^>]*>.*?</(?:script|style)\s*>")
        .unwrap()
        .replace_all(value, "");
    let breaks = regex::Regex::new(r"(?i)</?(?:p|div|br|h[1-6]|li|tr)\b[^>]*>")
        .unwrap()
        .replace_all(&blocks, "\n");
    let tags = regex::Regex::new(r"(?s)<[^>]*>")
        .unwrap()
        .replace_all(&breaks, "");
    tags.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .chars()
        .take(20000)
        .collect()
}
fn read_action(product: &str, saved: &Connection, method: &str, args: &Value) -> Result<Value> {
    match method {
        "jira_read_issue" => {
            let key = issue_key(&args["key"])?;
            let start = page_number(&args["commentStart"])?;
            let v=get(saved,&format!("rest/api/2/issue/{key}"),&[("fields","summary,description,status,issuetype,priority,assignee,reporter,parent,issuelinks,subtasks,labels,updated".into())])?;
            if v["key"] != key || !v["fields"].is_object() {
                bail!(RESPONSE);
            }
            let f = &v["fields"];
            let mut out = brief_issue(&v);
            out["description"] = json!(text(&f["description"], 20000));
            out["assignee"] = person(&f["assignee"]);
            out["reporter"] = person(&f["reporter"]);
            out["priority"] = json!(text(&f["priority"]["name"], 160));
            out["parent"] = brief_issue(&f["parent"]);
            out["url"] = json!(base(&saved.base)?.join(&format!("browse/{key}"))?.as_str());
            out["linkedIssues"]=json!(f["issuelinks"].as_array().into_iter().flatten().take(30).map(|link|json!({"relation":text(&link["type"]["name"],160),"issue":brief_issue(if link["outwardIssue"].is_object(){&link["outwardIssue"]}else{&link["inwardIssue"]})})).collect::<Vec<_>>());
            match get(
                saved,
                &format!("rest/api/2/issue/{key}/comment"),
                &[("startAt", start.to_string()), ("maxResults", "20".into())],
            ) {
                Ok(comments) => {
                    let rows = page_list(&comments, "comments")?;
                    let items:Vec<_>=rows.iter().take(20).map(|r|json!({"author":person(&r["author"]),"body":text(&r["body"],3000),"created":text(&r["created"],80)})).collect();
                    let next = start + items.len() as u64;
                    out["comments"] = json!({"items":items,"total":comments["total"].as_u64(),"nextStart":comments["total"].as_u64().filter(|total|next>start&&next<*total).map(|_|next)});
                }
                Err(_) => {
                    out["comments"] = json!({"unavailable":true});
                }
            }
            match get(saved,&format!("rest/api/2/issue/{key}/remotelink"),&[]) {Ok(Value::Array(links))=>out["documentLinks"]=json!(links.iter().take(30).map(|r|json!({"title":text(&r["object"]["title"],240),"url":text(&r["object"]["url"],2048)})).collect::<Vec<_>>()),_=>out["documentLinks"]=json!({"unavailable":true})}
            Ok(out)
        }
        "jira_search" => {
            let jql = search_query(args, "jql")?;
            let start = page_number(&args["start"])?;
            let v = get(
                saved,
                "rest/api/2/search",
                &[
                    ("jql", jql),
                    ("startAt", start.to_string()),
                    ("maxResults", "20".into()),
                    ("fields", "summary,status,issuetype".into()),
                ],
            )?;
            let rows = page_list(&v, "issues")?;
            let next = start + rows.len() as u64;
            Ok(
                json!({"items":rows.iter().take(20).map(brief_issue).collect::<Vec<_>>(),"total":v["total"].as_u64(),"nextStart":v["total"].as_u64().filter(|total|next>start&&next<*total).map(|_|next)}),
            )
        }
        "confluence_search" => {
            let query = search_query(args, "query")?;
            let start = page_number(&args["start"])?;
            let escaped = query.replace('\\', "\\\\").replace('"', "\\\"");
            let v = get(
                saved,
                "rest/api/content/search",
                &[
                    ("cql", format!("type = page AND text ~ \"{escaped}\"")),
                    ("start", start.to_string()),
                    ("limit", "20".into()),
                ],
            )?;
            let rows = page_list(&v, "results")?;
            Ok(
                json!({"items":rows.iter().take(20).map(|p|json!({"id":text(&p["id"],80),"title":text(&p["title"],1000)})).collect::<Vec<_>>(),"nextStart":(rows.len()==20).then_some(start+20)}),
            )
        }
        "confluence_read_page" => {
            let page = string(args, "pageId")?;
            if page.is_empty() || page.len() > 32 || !page.bytes().all(|b| b.is_ascii_digit()) {
                bail!("Sayfa kimliği geçersiz.");
            }
            let v = get(
                saved,
                &format!("rest/api/content/{page}"),
                &[("expand", "body.view,version,space".into())],
            )?;
            if v["id"] != page || !v["body"]["view"]["value"].is_string() {
                bail!(RESPONSE);
            }
            let html = v["body"]["view"]["value"].as_str().unwrap();
            Ok(
                json!({"id":page,"title":text(&v["title"],1000),"body":plain_html(html),"truncated":html.len()>20000,"version":v["version"]["number"].as_u64(),"space":text(&v["space"]["key"],80),"url":base(&saved.base)?.join(&format!("pages/viewpage.action?pageId={page}"))?.as_str()}),
            )
        }
        _ => {
            let _ = product;
            bail!("Jira/Confluence aracı bulunamadı.");
        }
    }
}
fn session_access(core: &Core, source: &str, run: &str) -> Result<()> {
    let s = core.session(source)?;
    if s["tools"] == false || s["login"] == true {
        bail!("Bu oturumun MCP erişimi kapalı.");
    }
    if s["archived"] == true {
        bail!("Arşivlenmiş oturumun bağlantı erişimi kapalı.");
    }
    let terms = core.terms.lock().unwrap();
    let live = terms.get(source).context("Oturum kapalı")?;
    if live.run_id != run || live.exited.load(Ordering::SeqCst) {
        bail!("Oturum çalıştırması kapalı");
    }
    Ok(())
}
fn redact(v: &mut Value, secret: &str) {
    match v {
        Value::String(s) => *s = s.replace(secret, "[redacted]"),
        Value::Array(a) => {
            for v in a {
                redact(v, secret)
            }
        }
        Value::Object(o) => {
            for v in o.values_mut() {
                redact(v, secret)
            }
        }
        _ => {}
    }
}
pub(crate) fn tool(
    core: &Arc<Core>,
    source: &str,
    run: &str,
    method: &str,
    args: &Value,
) -> Result<Value> {
    let product = match method {
        "jira_read_issue" | "jira_search" => "jira",
        "confluence_read_page" | "confluence_search" => "confluence",
        _ => bail!(INVALID),
    };
    session_access(core, source, run)?;
    let _permit = core.atlassian.enter()?;
    let saved = {
        let _guard = core.atlassian.gate.lock().unwrap();
        load(core, product)?.context(
            "Önce Ayarlar → Bağlantılar bölümünden Jira veya Confluence bağlantısını kaydet.",
        )?
    };
    let mut result = read_action(product, &saved, method, args)?;
    let _guard = core.atlassian.gate.lock().unwrap();
    unchanged(core, product, &saved.revision)?;
    session_access(core, source, run)?;
    redact(&mut result, &saved.token);
    Ok(json!({"source":product,"access":"read-only","contentIsUntrusted":true,"data":result}))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn addresses_keys_and_page_numbers_cannot_change_the_authenticated_destination() {
        assert_eq!(
            base("https://service.example.invalid/jira")
                .unwrap()
                .join("rest/api/2/myself")
                .unwrap()
                .as_str(),
            "https://service.example.invalid/jira/rest/api/2/myself"
        );
        for value in [
            "http://service.example.invalid",
            "https://user@service.example.invalid",
            "https://service.example.invalid/?x=1",
            "https://service.example.invalid/#fragment",
        ] {
            assert!(base(value).is_err());
        }
        assert_eq!(issue_key(&json!("fixture-123")).unwrap(), "FIXTURE-123");
        for value in [
            "../secret",
            "X-1?redirect=bad",
            "http://other.invalid",
            "X-0",
        ] {
            assert!(issue_key(&json!(value)).is_err());
        }
        assert!(page_number(&json!(-1)).is_err());
        assert!(page_number(&json!(1.5)).is_err());
        assert!(validate_token("line\nbreak").is_err());
    }
    #[test]
    fn retrieved_text_is_bounded_and_authentication_values_are_redacted() {
        assert_eq!(
            plain_html("<p>First &amp; second</p><script>ignored</script><p>Third</p>").trim(),
            "First & second\n\nThird"
        );
        let secret = id();
        let mut v = json!({"text":format!("before {secret} after"),"nested":[secret]});
        redact(&mut v, &secret);
        assert!(!v.to_string().contains(&secret));
        let state = State::default();
        let guards = (0..4).map(|_| state.enter().unwrap()).collect::<Vec<_>>();
        assert!(state.enter().is_err());
        drop(guards);
        assert!(state.enter().is_ok());
    }
}
