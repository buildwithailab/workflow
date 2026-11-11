use crate::{core::*, terminal};
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::Arc,
};
pub fn list(core: &Core) -> Value {
    json!([{"id":"openspec","name":"OpenSpec","description":"Plan, gereksinim, tasarım ve görevler","installed":core.directory.join("addons/openspec/manifest.json").is_file()},{"id":"ponytail","name":"Ponytail","description":"Sade çözüm ve kod inceleme becerileri","installed":core.directory.join("addons/ponytail/manifest.json").is_file()}])
}
fn capture(exe: &Path, args: &[String], cwd: &Path) -> Result<String> {
    use std::process::{Command, Stdio};
    let extension = exe.extension().unwrap_or_default().to_string_lossy();
    let mut c = if cfg!(windows) && ["cmd", "ps1", "bat"].contains(&extension.as_ref()) {
        let q = |v: &str| format!("'{}'", v.replace('\'', "''"));
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
    c.current_dir(cwd)
        .env("DO_NOT_TRACK", "1")
        .env("OPENSPEC_TELEMETRY", "0")
        .env("CI", "true")
        .stdin(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        c.creation_flags(0x08000000);
    }
    let o = c.output()?;
    if !o.status.success() {
        bail!(
            "Eklenti kurulumu tamamlanamadı: {}",
            String::from_utf8_lossy(&o.stderr)
        )
    }
    Ok(String::from_utf8_lossy(&o.stdout).to_string())
}
pub fn dispatch(core: &Arc<Core>, action: &str, args: &Value) -> Result<Value> {
    match action {
        "list" => Ok(list(core)),
        "install" => {
            let _guard = core.operations.lock().unwrap();
            let addon = string(args, "id")?;
            if !["openspec", "ponytail"].contains(&addon) {
                bail!("Eklenti desteklenmiyor")
            };
            let root = core.directory.join("addons").join(addon);
            fs::create_dir_all(&root)?;
            if addon == "openspec" {
                let npm = terminal::executable("npm", None)
                    .context("OpenSpec kurulumu için Node.js/npm gerekir")?;
                capture(
                    &npm,
                    &[
                        "install".into(),
                        "--prefix".into(),
                        root.to_string_lossy().into(),
                        "--ignore-scripts".into(),
                        "--no-audit".into(),
                        "--no-fund".into(),
                        "--".into(),
                        "@fission-ai/openspec".into(),
                    ],
                    &root,
                )?;
                let pkg = root.join("node_modules/@fission-ai/openspec");
                let meta: Value = serde_json::from_slice(&fs::read(pkg.join("package.json"))?)?;
                let bin = meta["bin"]["openspec"]
                    .as_str()
                    .or_else(|| meta["bin"].as_str())
                    .context("OpenSpec giriş dosyası bulunamadı")?;
                let entry = safe_path(&pkg, bin.trim_start_matches("./"))?;
                let template = root.join("template");
                fs::create_dir_all(&template)?;
                let node = terminal::executable("node", None).context("Node.js bulunamadı")?;
                capture(
                    &node,
                    &[
                        entry.to_string_lossy().into(),
                        "init".into(),
                        "--tools".into(),
                        "agents".into(),
                        "--profile".into(),
                        "core".into(),
                        "--no-animation".into(),
                    ],
                    &template,
                )?;
                let skills = template.join(".agents/skills");
                if !skills.is_dir() {
                    bail!("OpenSpec becerileri oluşturulamadı")
                };
                let manifest = json!({"version":meta["version"],"skills":skills,"entry":entry});
                atomic(&root.join("manifest.json"), &serde_json::to_vec(&manifest)?)?;
            } else {
                let repo = root.join("source");
                if !repo.exists() {
                    git(
                        &root,
                        &[
                            "clone",
                            "--depth",
                            "1",
                            "--",
                            "https://github.com/DietrichGebert/ponytail.git",
                            &repo.to_string_lossy(),
                        ],
                    )?;
                }
                let shared_skills = repo.join("skills");
                let skills_root = if shared_skills.is_dir() {
                    &shared_skills
                } else {
                    &repo
                };
                let mut skills = vec![];
                for e in ignore::WalkBuilder::new(skills_root)
                    .hidden(false)
                    .git_ignore(false)
                    .filter_entry(|e| e.file_name() != ".git")
                    .build()
                    .flatten()
                {
                    if e.file_name() == "SKILL.md" {
                        skills.push(e.path().parent().unwrap().to_path_buf())
                    }
                }
                if skills.is_empty() {
                    bail!("Depoda standart SKILL.md becerisi bulunamadı")
                };
                atomic(
                    &root.join("manifest.json"),
                    &serde_json::to_vec(
                        &json!({"version":git(&repo,&["rev-parse","HEAD"])?,"skillDirectories":skills}),
                    )?,
                )?;
            }
            Ok(list(core))
        }
        "select" => {
            let sid = string(args, "id")?;
            if core.session(sid)?["workspaceKind"] == "direct" {
                bail!("Oturuma özel eklentiler için ayrı çalışma alanı seç.")
            }
            if core
                .terms
                .lock()
                .unwrap()
                .get(sid)
                .is_some_and(|t| !t.exited.load(std::sync::atomic::Ordering::SeqCst))
            {
                bail!("Eklenti değişikliği için önce bu oturumu durdur")
            };
            core.update_session(sid, json!({"addons":args["addons"]}))
        }
        _ => bail!("Eklenti işlemi geçersiz"),
    }
}
pub fn prepare(core: &Core, session: &Value) -> Result<()> {
    if session["workspaceKind"] == "direct" {
        return Ok(());
    }
    let ids = session["addons"].as_array().cloned().unwrap_or_default();
    let sid = string(session, "id")?;
    let root = PathBuf::from(string(session, "directory")?);
    let provider = string(session, "provider")?;
    let manifest_file = core
        .directory
        .join("workspaces")
        .join(sid)
        .join("addons-applied.json");
    let previous: Value = if manifest_file.exists() {
        serde_json::from_slice(&fs::read(&manifest_file)?)?
    } else {
        json!({})
    };
    let prefix = match provider {
        "claude" => ".claude/skills",
        "copilot" => ".github/skills",
        _ => ".agents/skills",
    };
    let mut desired = std::collections::HashMap::<String, Vec<u8>>::new();
    for addon in &ids {
        let addon = addon.as_str().context("Eklenti adı geçersiz")?;
        if !["openspec", "ponytail"].contains(&addon) {
            bail!("Eklenti desteklenmiyor")
        };
        let addon_root = core.directory.join("addons").join(addon);
        let m: Value = serde_json::from_slice(
            &fs::read(addon_root.join("manifest.json"))
                .context("Önce eklentiyi kütüphaneden kur")?,
        )?;
        let dirs = if let Some(dir) = m["skills"].as_str() {
            fs::read_dir(dir)?
                .filter_map(|e| e.ok())
                .filter(|e| e.path().is_dir())
                .map(|e| e.path())
                .collect::<Vec<_>>()
        } else {
            m["skillDirectories"]
                .as_array()
                .context("Eklenti becerileri eksik")?
                .iter()
                .filter_map(Value::as_str)
                .map(PathBuf::from)
                .collect()
        };
        // Older Ponytail manifests also listed provider-specific copies.
        let shared_skills = addon_root.join("source/skills");
        let use_shared_skills = addon == "ponytail" && shared_skills.is_dir();
        for dir in dirs {
            if use_shared_skills && !dir.starts_with(&shared_skills) {
                continue;
            }
            let name = dir.file_name().context("Beceri adı yok")?.to_string_lossy();
            for entry in ignore::WalkBuilder::new(&dir)
                .hidden(false)
                .git_ignore(false)
                .build()
            {
                let entry = entry?;
                if entry.file_type().is_some_and(|t| t.is_symlink()) {
                    bail!("Eklentide bağlantılı dosya var")
                };
                if !entry.file_type().is_some_and(|t| t.is_file()) {
                    continue;
                }
                let relative = entry
                    .path()
                    .strip_prefix(&dir)?
                    .to_string_lossy()
                    .replace('\\', "/");
                let key = format!("{prefix}/{name}/{relative}");
                let bytes = fs::read(entry.path())?;
                if bytes.len() > 2 * 1024 * 1024 {
                    bail!("Eklenti dosyası çok büyük")
                };
                if desired.insert(key, bytes).is_some() {
                    bail!("Eklentiler aynı beceri dosyasını sağlıyor")
                }
            }
        }
    }
    let mut changes = vec![];
    for (path, old) in previous.as_object().context("Eklenti kaydı geçersiz")? {
        let current = file_hash(&safe_path(&root, path)?)?;
        if current.as_deref() != old.as_str() {
            bail!("Eklenti dosyasında kullanıcı değişikliği var; korundu: {path}")
        };
        if !desired.contains_key(path) {
            changes.push((path.clone(), None));
        }
    }
    let mut next = serde_json::Map::new();
    for (path, bytes) in desired {
        let dest = safe_path(&root, &path)?;
        let current = file_hash(&dest)?;
        let digest = hash(&bytes);
        if current.is_some() && previous[&path].is_null() && current.as_deref() != Some(&digest) {
            bail!("Beceri dosyası zaten var; üzerine yazılmadı: {path}")
        };
        next.insert(path.clone(), json!(digest));
        changes.push((path, Some(bytes)));
    }
    for (path, bytes) in changes {
        crate::files::apply_bytes(&safe_path(&root, &path)?, bytes.as_deref())?;
    }
    atomic(&manifest_file, &serde_json::to_vec_pretty(&next)?)?;
    if ids.iter().any(|id| id == "openspec") {
        fs::create_dir_all(root.join("openspec/specs"))?;
        fs::create_dir_all(root.join("openspec/changes"))?;
        if !root.join("openspec/config.yaml").exists() {
            atomic(&root.join("openspec/config.yaml"), b"schema: spec-driven\n")?;
        }
    }
    Ok(())
}
