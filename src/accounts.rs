use crate::core::*;
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::{path::PathBuf, process::Command};

pub const DEFAULT: &str = "default";

pub struct Profile {
    provider: String,
    pub id: String,
    pub directory: Option<PathBuf>,
    identity: Option<String>,
}

impl Profile {
    pub fn get(core: &Core, provider: &str, value: &Value) -> Result<Self> {
        Self::resolve(core, provider, value, false)
    }

    // History may read a retired profile, but launches, login, models and quota requests may not.
    pub(crate) fn for_history(core: &Core, provider: &str, value: &Value) -> Result<Self> {
        Self::resolve(core, provider, value, true)
    }

    fn resolve(core: &Core, provider: &str, value: &Value, allow_removed: bool) -> Result<Self> {
        let variable = match provider {
            "codex" => "CODEX_HOME",
            "claude" => "CLAUDE_CONFIG_DIR",
            "copilot" => "COPILOT_HOME",
            _ => bail!("Sağlayıcı geçersiz"),
        };
        let id = if value.is_null() {
            DEFAULT
        } else {
            value.as_str().context("Hesap seçimi geçersiz")?
        };
        if id == DEFAULT {
            return Ok(Self {
                provider: provider.into(),
                id: id.into(),
                directory: None,
                identity: None,
            });
        }
        uuid::Uuid::parse_str(id).context("Hesap seçimi geçersiz")?;
        let db = core.db.lock().unwrap();
        let entry = &db["providers"][provider]["accounts"][id];
        if !entry.is_object() {
            bail!("Bu sağlayıcı için hesap bulunamadı");
        }
        if !allow_removed && entry["removed"] == true {
            bail!("Bu hesap Workflow’dan kaldırılmış. Hesaplar bölümünden geri ekle.");
        }
        let directory = core.directory.join("accounts").join(variable).join(id);
        Ok(Self {
            provider: provider.into(),
            id: id.into(),
            directory: Some(directory),
            identity: entry["identity"].as_str().map(str::to_owned),
        })
    }

    fn variable(&self) -> &str {
        match self.provider.as_str() {
            "codex" => "CODEX_HOME",
            "claude" => "CLAUDE_CONFIG_DIR",
            _ => "COPILOT_HOME",
        }
    }

    pub fn config_dir(&self) -> PathBuf {
        self.directory.clone().unwrap_or_else(|| {
            std::env::var_os(self.variable())
                .map(PathBuf::from)
                .unwrap_or_else(|| {
                    home().join(match self.provider.as_str() {
                        "codex" => ".codex",
                        "claude" => ".claude",
                        _ => ".copilot",
                    })
                })
        })
    }

    fn credentials(&self) -> &[&str] {
        match self.provider.as_str() {
            "codex" => &["OPENAI_API_KEY", "CODEX_API_KEY", "OPENAI_BASE_URL"],
            "claude" => &[
                "ANTHROPIC_API_KEY",
                "ANTHROPIC_AUTH_TOKEN",
                "CLAUDE_CODE_OAUTH_TOKEN",
                "ANTHROPIC_PROFILE",
                "ANTHROPIC_FEDERATION_RULE_ID",
                "ANTHROPIC_ORGANIZATION_ID",
                "ANTHROPIC_BASE_URL",
                "CLAUDE_CODE_USE_BEDROCK",
                "CLAUDE_CODE_USE_VERTEX",
                "CLAUDE_CODE_USE_FOUNDRY",
            ],
            _ => &[
                "GITHUB_COPILOT_API_TOKEN",
                "COPILOT_API_URL",
                "COPILOT_GITHUB_TOKEN",
                "GH_TOKEN",
                "GITHUB_TOKEN",
                "GH_ENTERPRISE_TOKEN",
                "GITHUB_ENTERPRISE_TOKEN",
                "COPILOT_PROVIDER_BASE_URL",
                "COPILOT_PROVIDER_API_KEY",
                "COPILOT_PROVIDER_TYPE",
            ],
        }
    }

    fn env(&self, mut set: impl FnMut(&str, Option<&std::path::Path>)) {
        if let Some(directory) = &self.directory {
            for key in self.credentials() {
                set(key, None);
            }
            set(self.variable(), Some(directory));
            if self.provider == "copilot" {
                set("GH_CONFIG_DIR", Some(&directory.join("github-cli")));
            }
        }
    }

    pub fn apply(&self, command: &mut Command) {
        self.env(|key, value| match value {
            Some(value) => {
                command.env(key, value);
            }
            None => {
                command.env_remove(key);
            }
        });
    }

    pub fn apply_pty(&self, command: &mut portable_pty::CommandBuilder) {
        self.env(|key, value| match value {
            Some(value) => command.env(key, value),
            None => command.env_remove(key),
        });
    }

    pub fn check(&self, identity: Option<&str>) -> Result<()> {
        if self.directory.is_none() {
            return Ok(());
        }
        let expected = self.identity.as_deref().context("Önce bu hesabı bağla.")?;
        if identity != Some(expected) {
            bail!("Seçili hesabın girişi doğrulanamadı. Hesaplar bölümünden yeniden giriş yap.");
        }
        Ok(())
    }

    pub fn verified(&self) -> bool {
        self.directory.is_none() || self.identity.is_some()
    }
}

fn rows(db: &Value, provider: &str, removed: bool) -> Vec<Value> {
    let mut rows = if removed {
        vec![]
    } else {
        vec![json!({"id":DEFAULT,"name":"Varsayılan hesap","verified":true})]
    };
    rows.extend(db["providers"][provider]["accounts"].as_object().into_iter().flatten()
        .filter(|(_, entry)| (entry["removed"] == true) == removed)
        .map(|(id, entry)| json!({"id":id,"name":entry["name"],"verified":entry["identity"].is_string()})));
    rows
}

pub fn list(core: &Core, provider: &str) -> Vec<Value> {
    rows(&core.db.lock().unwrap(), provider, false)
}

pub fn removed(core: &Core, provider: &str) -> Vec<Value> {
    rows(&core.db.lock().unwrap(), provider, true)
}

fn change_membership(
    db: &mut Value,
    provider: &str,
    account_id: &str,
    removed: bool,
) -> Result<()> {
    if account_id == DEFAULT {
        bail!("Varsayılan hesap Workflow’dan kaldırılamaz.");
    }
    let entry = db
        .get_mut("providers")
        .and_then(|providers| providers.get_mut(provider))
        .and_then(|provider| provider.get_mut("accounts"))
        .and_then(|accounts| accounts.get_mut(account_id))
        .filter(|entry| entry.is_object())
        .context("Bu sağlayıcı için hesap bulunamadı")?;
    entry["removed"] = json!(removed);
    Ok(())
}

pub fn set_removed(core: &Core, args: &Value, removed: bool) -> Result<Value> {
    let provider = string(args, "provider")?;
    let account_id = string(args, "accountId")?;
    let _guard = core.operations.lock().unwrap();
    Profile::for_history(core, provider, &args["accountId"])?;
    if account_id == DEFAULT {
        bail!("Varsayılan hesap Workflow’dan kaldırılamaz.");
    }
    if core
        .login_terminal
        .lock()
        .unwrap()
        .as_ref()
        .is_some_and(|login| login.uses_account(provider, account_id))
    {
        bail!("Bu hesap için açık olan giriş işlemini tamamla veya kapat.");
    }
    let mut db = core.db.lock().unwrap();
    let terms = core.terms.lock().unwrap();
    let running = db["sessions"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|session| {
            session["provider"] == provider
                && session["accountId"] == account_id
                && session["id"]
                    .as_str()
                    .and_then(|id| terms.get(id))
                    .is_some_and(|terminal| {
                        !terminal.exited.load(std::sync::atomic::Ordering::SeqCst)
                    })
        });
    if running {
        bail!("Hesabı kaldırmadan önce bu hesaba bağlı çalışan oturumları durdur.");
    }
    drop(terms);
    let mut updated = db.clone();
    change_membership(&mut updated, provider, account_id, removed)?;
    // Commit the metadata only after durable storage succeeds. Account files and sessions stay intact.
    core.save_workspace(&updated)?;
    *db = updated;
    Ok(json!(true))
}

pub fn add(core: &Core, args: &Value) -> Result<Value> {
    let provider = string(args, "provider")?;
    Profile::get(core, provider, &Value::Null)?;
    let name = string(args, "name")?.trim();
    if name.is_empty() || name.chars().count() > 60 || name.chars().any(char::is_control) {
        bail!("Hesap için 1–60 karakterlik bir ad gir.");
    }
    let _guard = core.operations.lock().unwrap();
    let id = id();
    core.db.lock().unwrap()["providers"][provider]["accounts"][&id] = json!({"name":name});
    core.save()?;
    Ok(json!({"id":id,"name":name,"verified":false}))
}

pub fn verify(core: &Core, provider: &str, profile: &Profile, identity: &str) -> Result<()> {
    if profile.directory.is_none() {
        return Ok(());
    }
    let _guard = core.operations.lock().unwrap();
    {
        let mut db = core.db.lock().unwrap();
        let accounts = db["providers"][provider]["accounts"]
            .as_object_mut()
            .context("Hesap bulunamadı")?;
        if accounts
            .iter()
            .any(|(id, entry)| id != &profile.id && entry["identity"] == identity)
        {
            bail!("Bu hesap zaten başka bir profil olarak ekli.");
        }
        let entry = accounts.get_mut(&profile.id).context("Hesap bulunamadı")?;
        if entry["removed"] == true {
            bail!("Bu hesap Workflow’dan kaldırılmış. Hesaplar bölümünden geri ekle.");
        }
        if entry["identity"]
            .as_str()
            .is_some_and(|previous| previous != identity)
        {
            bail!("Bu profil başka bir hesaba bağlı. İlk bağladığın hesapla giriş yap.");
        }
        entry["identity"] = json!(identity);
    }
    core.save()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn removing_and_restoring_accounts_keeps_session_binding_and_identity() {
        let account_id = id();
        let mut db = json!({"providers":{"codex":{"accounts":{}}},
            "sessions":[{"accountId":account_id,"conversationId":id()}],"preferences":{"unchanged":true}});
        db["providers"]["codex"]["accounts"][&account_id] =
            json!({"name":"Synthetic account","identity":id()});
        let original = db.clone();
        change_membership(&mut db, "codex", &account_id, true).unwrap();
        assert_eq!(rows(&db, "codex", false).len(), 1);
        assert_eq!(rows(&db, "codex", true).len(), 1);
        assert_eq!(db["sessions"], original["sessions"]);
        assert_eq!(db["preferences"], original["preferences"]);
        assert_eq!(
            db["providers"]["codex"]["accounts"][&account_id]["identity"],
            original["providers"]["codex"]["accounts"][&account_id]["identity"]
        );
        change_membership(&mut db, "codex", &account_id, false).unwrap();
        assert_eq!(rows(&db, "codex", false).len(), 2);
        assert!(rows(&db, "codex", true).is_empty());
        let before = db.clone();
        assert!(change_membership(&mut db, "codex", DEFAULT, true).is_err());
        assert!(change_membership(&mut db, "claude", &account_id, true).is_err());
        assert_eq!(db, before);
    }
}
