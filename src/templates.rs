use crate::{
    core::{atomic, hash, id, safe_path, string, Core},
    files, prepare_session, usage,
};
use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::HashSet,
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{Arc, OnceLock},
};
use uuid::Uuid;

const MAX_BYTES: u64 = 128 * 1024;
const BUILTINS: &str = include_str!("templates.json");

fn portable_text(text: &str) -> bool {
    static ABSOLUTE_PATH: OnceLock<regex::Regex> = OnceLock::new();
    let absolute_path = ABSOLUTE_PATH.get_or_init(|| {
        regex::Regex::new(r#"(?i)(?:^|[^\w])[a-z]:[\\/]|(?:^|[\s"'`=(\[{])/[^/\s]|file://"#)
            .expect("valid template path expression")
    });
    !crate::sharing::contains_credentials(text.as_bytes())
        && !text.contains(r"\\")
        && !absolute_path.is_match(text)
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct Template {
    version: u8,
    name: String,
    sessions: Vec<Session>,
    links: Vec<Link>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct Session {
    name: String,
    provider: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    model: Option<String>,
    workspace_mode: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    initial_prompt: Option<String>,
    position: Position,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Position {
    x: i32,
    y: i32,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Link {
    source: usize,
    target: usize,
    read: bool,
    files: bool,
    work: bool,
}

impl Template {
    fn validate(&self) -> Result<()> {
        if self.version != 1
            || self.name.trim().is_empty()
            || self.name.len() > 160
            || !portable_text(&self.name)
            || self.sessions.is_empty()
            || self.sessions.len() > 20
            || self.links.len() > 120
        {
            bail!("Şablon biçimi veya boyutu geçersiz.");
        }
        for s in &self.sessions {
            if s.name.trim().is_empty()
                || s.name.len() > 160
                || !portable_text(&s.name)
                || !["codex", "claude", "copilot"].contains(&s.provider.as_str())
                || !["direct", "isolated"].contains(&s.workspace_mode.as_str())
                || s.model.as_ref().is_some_and(|v| {
                    v.len() > 128 || !portable_text(v) || v.chars().any(|c| c.is_control())
                })
                || s.initial_prompt.as_ref().is_some_and(|v| {
                    v.len() > 4000
                        || !portable_text(v)
                        || v.chars()
                            .any(|c| c.is_control() && c != '\n' && c != '\r' && c != '\t')
                })
                || s.position.x.unsigned_abs() > 100_000
                || s.position.y.unsigned_abs() > 100_000
            {
                bail!("Şablonda geçersiz oturum var.");
            }
        }
        let mut directions = HashSet::new();
        for link in &self.links {
            if link.source == link.target
                || link.source >= self.sessions.len()
                || link.target >= self.sessions.len()
                || !directions.insert((link.source, link.target))
            {
                bail!("Şablonda geçersiz bağlantı var.");
            }
        }
        Ok(())
    }
}

fn builtins() -> Result<Vec<Template>> {
    Ok(serde_json::from_str(BUILTINS)?)
}

fn read_template(path: &Path) -> Result<Template> {
    let file = fs::File::open(path)?;
    if !file.metadata()?.is_file() {
        bail!("Bir şablon dosyası seç.");
    }
    let mut contents = Vec::new();
    file.take(MAX_BYTES + 1).read_to_end(&mut contents)?;
    if contents.len() as u64 > MAX_BYTES {
        bail!("Şablon dosyası çok büyük.");
    }
    let template: Template =
        serde_json::from_slice(&contents).context("Şablon JSON biçimi geçersiz.")?;
    template.validate()?;
    Ok(template)
}

fn local_template(core: &Core, template_id: &str) -> Result<Template> {
    if let Some(index) = template_id.strip_prefix("builtin:") {
        return builtins()?
            .into_iter()
            .nth(index.parse()?)
            .context("Şablon bulunamadı.");
    }
    read_template(&custom_path(core, template_id)?)
}

fn custom_path(core: &Core, template_id: &str) -> Result<PathBuf> {
    if template_id.starts_with("builtin:") {
        bail!("Hazır şablonlar değiştirilemez veya silinemez.");
    }
    let key = Uuid::parse_str(template_id).context("Şablon bulunamadı.")?;
    safe_path(&core.directory, &format!("templates/{key}.json"))
}

fn template_entry(key: &str, template: &Template) -> Result<Value> {
    let revision = hash(&serde_json::to_vec(template)?);
    Ok(json!({"id":key,"template":template,"revision":revision}))
}

fn provider_layout(template: &Template) -> Result<String> {
    let slots: Vec<_> = template
        .sessions
        .iter()
        .map(|session| (&session.name, &session.provider, &session.position))
        .collect();
    Ok(hash(&serde_json::to_vec(&slots)?))
}

fn catalog_entry(key: &str, template: &Template, preferences: &Value) -> Result<Value> {
    let mut entry = if key.starts_with("builtin:") {
        json!({"id":key,"template":template})
    } else {
        template_entry(key, template)?
    };
    let saved = &preferences[key];
    if let Some(layout) = saved["layout"].as_str() {
        if layout == provider_layout(template)? {
            entry["selectedProviders"] = saved["providers"].clone();
        }
    }
    Ok(entry)
}

fn verify_revision(path: &Path, args: &Value) -> Result<()> {
    let current = read_template(path)?;
    if hash(&serde_json::to_vec(&current)?) != string(args, "revision")? {
        bail!("Şablon değişti. Listeyi yeniden aç ve tekrar dene.");
    }
    Ok(())
}

fn template_bytes(template: &Template) -> Result<Vec<u8>> {
    template.validate()?;
    let contents = serde_json::to_vec_pretty(template)?;
    if contents.len() as u64 > MAX_BYTES {
        bail!("Şablon dosyası çok büyük.");
    }
    Ok(contents)
}

fn save_template(core: &Core, template: &Template) -> Result<Value> {
    let contents = template_bytes(template)?;
    let key = id();
    atomic(&custom_path(core, &key)?, &contents)?;
    template_entry(&key, template)
}

fn capture(core: &Core, args: &Value) -> Result<Template> {
    let project = string(args, "projectId")?;
    let db = core.db.lock().unwrap();
    let sessions: Vec<_> = db["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|s| s["projectId"] == project && s["archived"] != true)
        .collect();
    if sessions.is_empty() || sessions.len() > 20 {
        bail!("Şablon için 1–20 oturum gerekir.");
    }
    let mut output = Template {
        version: 1,
        name: string(args, "name")?.trim().to_owned(),
        sessions: Vec::new(),
        links: Vec::new(),
    };
    for (index, s) in sessions.iter().enumerate() {
        let coordinate = |key, default| {
            s["position"][key]
                .as_f64()
                .map(|v| v.round() as i32)
                .unwrap_or(default)
        };
        output.sessions.push(Session {
            name: string(s, "name")?.to_owned(),
            provider: string(s, "provider")?.to_owned(),
            model: s["model"].as_str().map(str::to_owned),
            workspace_mode: if s["workspaceKind"] == "direct" {
                "direct"
            } else {
                "isolated"
            }
            .into(),
            initial_prompt: s["initialPrompt"].as_str().map(str::to_owned),
            position: Position {
                x: coordinate("x", 40 + (index as i32 % 2) * 780),
                y: coordinate("y", 50 + (index as i32 / 2) * 580),
            },
        });
    }
    for link in db["links"].as_array().unwrap() {
        let source = sessions.iter().position(|s| s["id"] == link["source"]);
        let target = sessions.iter().position(|s| s["id"] == link["target"]);
        if let (Some(source), Some(target)) = (source, target) {
            output.links.push(Link {
                source,
                target,
                read: link["read"] == true,
                files: link["files"] == true,
                work: link["work"] == true,
            });
        }
    }
    Ok(output)
}

fn apply(core: &Core, args: &Value) -> Result<Value> {
    let _guard = core.operations.lock().unwrap();
    let project = core.project(string(args, "projectId")?)?;
    if project["removed"] == true {
        bail!("Projeyi önce çalışma alanına ekle.");
    }
    let template_id = string(args, "id")?;
    let template = local_template(core, template_id)?;
    template.validate()?;
    let providers = args["providers"]
        .as_array()
        .context("Şablon sağlayıcılarını seç.")?;
    if providers.len() != template.sessions.len() {
        bail!("Şablon sağlayıcılarını seç.");
    }
    for provider in providers {
        if !provider.as_str().is_some_and(|p| {
            ["codex", "claude", "copilot"].contains(&p) && usage::executable(core, p).is_some()
        }) {
            bail!("Her oturum için kurulu bir sağlayıcı seç.");
        }
    }
    let mut sessions = Vec::new();
    let result = (|| -> Result<()> {
        let bottom = {
            let db = core.db.lock().unwrap();
            db["sessions"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|s| s["projectId"] == project["id"] && s["archived"] != true)
                .enumerate()
                .map(|(i, s)| {
                    s["position"]["y"]
                        .as_f64()
                        .unwrap_or(50. + (i / 2) as f64 * 580.)
                        + s["size"]["height"].as_f64().unwrap_or(500.)
                        + 80.
                })
                .reduce(f64::max)
        };
        let first_y = template
            .sessions
            .iter()
            .map(|s| s.position.y)
            .min()
            .unwrap();
        for (entry, provider) in template.sessions.iter().zip(providers) {
            let model = (provider.as_str() == Some(entry.provider.as_str()))
                .then(|| entry.model.as_deref())
                .flatten();
            let mut session = prepare_session(
                core,
                &project,
                &json!({
                    "name":entry.name, "provider":provider, "model":model, "workspaceMode":entry.workspace_mode,
                }),
            )?;
            session["position"] = json!({
                "x":entry.position.x, "y":bottom.map(|y| y + f64::from(entry.position.y - first_y)).unwrap_or(f64::from(entry.position.y)),
            });
            session["initialPrompt"] = json!(entry.initial_prompt);
            sessions.push(session);
        }
        let links = template.links.iter().map(|l| json!({
            "id":id(), "source":sessions[l.source]["id"], "target":sessions[l.target]["id"],
            "sourceConversation":null, "targetConversation":null, "read":l.read, "files":l.files, "work":l.work,
        }));
        let mut db = core.db.lock().unwrap();
        let mut updated = db.clone();
        updated["sessions"]
            .as_array_mut()
            .unwrap()
            .extend(sessions.iter().cloned());
        updated["links"].as_array_mut().unwrap().extend(links);
        updated["preferences"]["templateProviders"][template_id] =
            json!({"layout":provider_layout(&template)?,"providers":providers});
        core.save_workspace(&updated)?;
        *db = updated;
        Ok(())
    })();
    if let Err(error) = result {
        let cleanup_failures = sessions
            .iter()
            .filter(|s| files::discard_new_workspace(core, s).is_err())
            .count();
        if cleanup_failures > 0 {
            return Err(anyhow!(
                "{error}. Tamamlanamayan {cleanup_failures} çalışma alanı temizlenemedi."
            ));
        }
        return Err(error);
    }
    core.emit("workspace", core.snapshot());
    Ok(json!(sessions))
}

pub fn dispatch(core: &Arc<Core>, action: &str, args: &Value) -> Result<Value> {
    match action {
        "list" => {
            let preferences = core.db.lock().unwrap()["preferences"]["templateProviders"].clone();
            let mut templates: Vec<_> = builtins()?
                .into_iter()
                .enumerate()
                .map(|(i, template)| {
                    catalog_entry(&format!("builtin:{i}"), &template, &preferences)
                })
                .collect::<Result<_>>()?;
            let directory = safe_path(&core.directory, "templates")?;
            fs::create_dir_all(&directory)?;
            for entry in fs::read_dir(directory)? {
                let entry = entry?;
                let path = entry.path();
                if !entry.file_type()?.is_file() || path.extension().is_none_or(|ext| ext != "json")
                {
                    continue;
                }
                let Some(key) = path
                    .file_stem()
                    .and_then(|v| v.to_str())
                    .and_then(|v| Uuid::parse_str(v).ok())
                else {
                    continue;
                };
                if let Ok(template) = read_template(&path) {
                    templates.push(catalog_entry(&key.to_string(), &template, &preferences)?);
                }
            }
            Ok(json!(templates))
        }
        "capture" => save_template(core, &capture(core, args)?),
        "import" => save_template(core, &read_template(Path::new(string(args, "path")?))?),
        "update" => {
            let _guard = core.operations.lock().unwrap();
            let key = string(args, "id")?;
            let path = custom_path(core, key)?;
            verify_revision(&path, args)?;
            let template = if args["projectId"].is_string() {
                capture(core, args)?
            } else {
                serde_json::from_value(args["template"].clone())
                    .context("Şablon JSON biçimi geçersiz.")?
            };
            let contents = template_bytes(&template)?;
            atomic(&path, &contents)?;
            template_entry(key, &template)
        }
        "delete" => {
            let _guard = core.operations.lock().unwrap();
            let path = custom_path(core, string(args, "id")?)?;
            verify_revision(&path, args)?;
            fs::remove_file(path)?;
            Ok(json!(true))
        }
        "export" => {
            let path = Path::new(string(args, "path")?);
            if path
                .extension()
                .is_none_or(|ext| !ext.eq_ignore_ascii_case("json"))
            {
                bail!("Şablon dosyasının uzantısı .json olmalı.");
            }
            let template = local_template(core, string(args, "id")?)?;
            let contents = template_bytes(&template)?;
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(path)
                .context("Şablonu dışa aktarmak için yeni bir dosya adı seç.")?;
            if let Err(error) = file.write_all(&contents).and_then(|()| file.sync_all()) {
                drop(file);
                let _ = fs::remove_file(path);
                return Err(error.into());
            }
            Ok(json!(true))
        }
        "apply" => apply(core, args),
        _ => bail!("Şablon işlemi bulunamadı."),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn templates_reject_local_metadata_and_invalid_links() {
        for template in builtins().unwrap() {
            template.validate().unwrap();
        }
        let template = builtins().unwrap().remove(0);
        let mut document = serde_json::to_value(template).unwrap();
        document["sessions"][0]["accountId"] = json!("default");
        assert!(serde_json::from_value::<Template>(document.clone()).is_err());
        document["sessions"][0]
            .as_object_mut()
            .unwrap()
            .remove("accountId");
        document["links"][0]["target"] = json!(500);
        assert!(serde_json::from_value::<Template>(document)
            .unwrap()
            .validate()
            .is_err());
    }

    #[test]
    fn validates_portability_and_serialized_size() {
        assert!(!portable_text(r#"Review "/scratch/project/source.rs""#));
        assert!(!portable_text("project=/scratch/project"));
        assert!(portable_text(
            "Review src/main.rs and https://example.com/docs"
        ));
        let mut template = builtins().unwrap().remove(0);
        let mut session = serde_json::to_value(&template.sessions[0]).unwrap();
        session["initialPrompt"] = json!("\"".repeat(3180));
        template.sessions = (0..20)
            .map(|_| serde_json::from_value(session.clone()).unwrap())
            .collect();
        assert!(serde_json::to_vec(&template).unwrap().len() as u64 <= MAX_BYTES);
        assert!(template_bytes(&template).is_err());
    }
}
