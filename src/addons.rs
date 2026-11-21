use crate::core::*;
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::{
    fs,
    io::{self, Read},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

const OPENSPEC_VERSION: &str = "2.0.0";
#[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
const OPENSPEC_ASSET: &str =
    "https://github.com/Fission-AI/OpenSpec/releases/download/v2.0.0/openspec-windows-x64.zip";
#[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
const OPENSPEC_ARCHIVE_HASH: &str =
    "4f50ca6765d7b7c214519558bf71635cc837f789155a42b21f73bbb6e11cb36b";
#[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
const OPENSPEC_EXE_HASH: &str = "ba45c5da8a4b0fadfbe6874d5b5fbe41a625f2b9f9b6b15d0490e99237c688ce";
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
const OPENSPEC_ASSET: &str = "https://github.com/Fission-AI/OpenSpec/releases/download/v2.0.0/openspec-darwin-arm64.tar.gz";
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
const OPENSPEC_ARCHIVE_HASH: &str = "cf1583baacb84711c18d58ea143928e4ed1701936e67ebda735200109f084af6";
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
const OPENSPEC_EXE_HASH: &str = "e79f7ecc7525c503b12f83ed587c71e276287fe7e64d8add6f2fa2d7b0896375";
const OPENSPEC_EXE: &str = if cfg!(target_os = "macos") {"openspec"} else {"openspec.exe"};
const PONYTAIL_VERSION: &str = "5.3.3";
const PONYTAIL_COMMIT: &str = "7801d061243d7b753bad0e0b3c057cd250b11755";
const SKILL_LIMIT: u64 = 2 * 1024 * 1024;
const EXE_LIMIT: u64 = 128 * 1024 * 1024;
const OPENSPEC_SKILLS: &[(&str, &str)] = &[
    (
        "openspec-apply-change",
        "71f084a1ea26194f62cb9ed31da8e4aa837fd57abb2b91b9cbb0c6ee8d503b79",
    ),
    (
        "openspec-archive-change",
        "3b52b3a9ff61247af45f7fdaacb7166a224e1c04081ddbc309f353ec01f741b2",
    ),
    (
        "openspec-explore",
        "e776f0d8b5e36d2e5fd8149254a95c54008942bd4336d1c67e568c67e45ddcca",
    ),
    (
        "openspec-propose",
        "5fa202ae4b4f27a67aac3e7e27e5b70fbb733651445988a6f4442bea21ec830e",
    ),
    (
        "openspec-sync-specs",
        "10616a4935e75bbbe5a4bdc81f19805e648076ba106414ffd361a5ae88f01867",
    ),
    (
        "openspec-update-change",
        "51066d453fa4300b0fab4345ed0c4734b57046fa8b170db78556cf52a0ea3916",
    ),
];
const PONYTAIL_SKILLS: &[(&str, &str)] = &[
    (
        "ponytail",
        "9badd22a32e19cb884e46dfa8d3aae82bb81dfa5484105b8b4f0262df823a509",
    ),
    (
        "ponytail-audit",
        "2b3223a02c386d1263e1b08343cb8eb3a53e444cdafb1d79e26c8ec2a8fcad28",
    ),
    (
        "ponytail-debt",
        "b5ec2762f6ba8c4af9ea7173aa6c9f91b88655a25e43f9de0264f6ce8722b25f",
    ),
    (
        "ponytail-gain",
        "231034a896d6762e4b5c8468287083422c56b499119976ec9f76821f7d083090",
    ),
    (
        "ponytail-help",
        "86ab0ce0af70cbeeeef499d5c884873b393f87f86401dcc45553662ce2c1eff9",
    ),
    (
        "ponytail-review",
        "33fe4ebe22a460924e5d4a261e223df22e409cc94ab76182bee9fb91a3ef022e",
    ),
];

struct Addon {
    id: &'static str,
    name: &'static str,
    description: &'static str,
    version: &'static str,
    revision: &'static str,
    source: &'static str,
    skills: &'static [(&'static str, &'static str)],
    command: Option<&'static str>,
}

const ADDONS: &[Addon] = &[
    Addon {
        id: "openspec",
        name: "OpenSpec",
        description: "Plan, gereksinim, tasarım ve görevler",
        version: OPENSPEC_VERSION,
        revision: OPENSPEC_ARCHIVE_HASH,
        source: "https://github.com/Fission-AI/OpenSpec/releases/tag/v2.0.0",
        skills: OPENSPEC_SKILLS,
        command: Some(if cfg!(target_os = "macos") {"openspec init --tools agents --profile core --no-animation"} else {"openspec.exe init --tools agents --profile core --no-animation"}),
    },
    Addon {
        id: "ponytail",
        name: "Ponytail",
        description: "Sade çözüm ve kod inceleme becerileri",
        version: PONYTAIL_VERSION,
        revision: PONYTAIL_COMMIT,
        source: "https://github.com/DietrichGebert/ponytail/tree/7801d061243d7b753bad0e0b3c057cd250b11755/skills",
        skills: PONYTAIL_SKILLS,
        command: None,
    },
];

fn catalog(id: &str) -> Result<&'static Addon> {
    ADDONS
        .iter()
        .find(|a| a.id == id)
        .context("Eklenti desteklenmiyor")
}

fn skill_file(name: &str) -> String {
    format!("{name}/SKILL.md")
}

fn verified_bytes(root: &Path, path: &str, digest: &str, limit: u64) -> Result<Vec<u8>> {
    let path = safe_path(root, path)?;
    if !fs::metadata(&path)?.is_file() {
        bail!("Eklenti bütünlük denetimi başarısız")
    }
    let mut bytes = Vec::new();
    fs::File::open(path)?
        .take(limit + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit || hash(&bytes) != digest {
        bail!("Eklenti bütünlük denetimi başarısız")
    }
    Ok(bytes)
}

fn manifest(root: &Path) -> Result<Option<Value>> {
    match fs::read(safe_path(root, "verified.json")?) {
        Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

fn generation(root: &Path, addon: &Addon, record: &Value) -> Result<PathBuf> {
    if record["revision"] != addon.revision || record["version"] != addon.version {
        bail!("Eklentiyi kütüphaneden doğrulanmış sürüme yükselt")
    }
    let id = string(record, "generation")?;
    uuid::Uuid::parse_str(id)?;
    safe_path(root, id)
}

fn installed_root(root: &Path, addon: &Addon) -> Result<PathBuf> {
    let record = manifest(root)
        .ok()
        .flatten()
        .context("Eklentiyi kütüphaneden doğrulanmış sürüme yükselt")?;
    let path = generation(root, addon, &record)?;
    if !path.is_dir() {
        bail!("Eklentiyi kütüphaneden doğrulanmış sürüme yükselt")
    }
    Ok(path)
}

pub(crate) fn openspec_directory(core: &Core) -> Result<PathBuf> {
    let addon = catalog("openspec")?;
    let root = installed_root(&safe_path(&core.directory, "addons/openspec")?, addon)?;
    verified_bytes(&root, OPENSPEC_EXE, OPENSPEC_EXE_HASH, EXE_LIMIT)?;
    Ok(root)
}

pub fn list(core: &Core) -> Value {
    Value::Array(ADDONS.iter().map(|a| {
        let root = core.directory.join("addons").join(a.id);
        let saved = manifest(&root).ok().flatten().or_else(|| {
            fs::read(root.join("manifest.json")).ok().and_then(|b| serde_json::from_slice(&b).ok())
        });
        let installed = saved.as_ref().is_some_and(|r| generation(&root, a, r).is_ok_and(|p| p.is_dir()));
        let changed_files = saved.as_ref().and_then(|record| {
            let old = record["files"].as_object()?;
            let next = skill_hashes(a);
            let mut changes = Vec::new();
            for (file, digest) in &next {
                match old.get(file) {
                    None => changes.push(json!({"file":file,"status":"added"})),
                    Some(hash) if hash != digest => changes.push(json!({"file":file,"status":"modified"})),
                    _ => {}
                }
            }
            for file in old.keys().filter(|file| !next.contains_key(*file)) {
                changes.push(json!({"file":file,"status":"removed"}));
            }
            Some(changes)
        });
        json!({
            "id":a.id, "name":a.name, "description":a.description, "installed":installed,
            "previousVersion":saved.as_ref().and_then(|r| r["version"].as_str()), "version":a.version,
            "revision":a.revision, "source":a.source, "command":a.command,
            "files":a.skills.iter().map(|(n, _)| skill_file(n)).collect::<Vec<_>>(),
            "changedFiles":changed_files,
        })
    }).collect())
}

fn skill_hashes(addon: &Addon) -> serde_json::Map<String, Value> {
    addon
        .skills
        .iter()
        .map(|(name, digest)| (skill_file(name), json!(digest)))
        .collect()
}

fn download(
    client: &reqwest::blocking::Client,
    url: &str,
    digest: &str,
    limit: u64,
) -> Result<Vec<u8>> {
    let response = client
        .get(url)
        .send()
        .and_then(|r| r.error_for_status())
        .context("Eklenti indirilemedi")?;
    let mut bytes = Vec::new();
    response
        .take(limit + 1)
        .read_to_end(&mut bytes)
        .context("Eklenti indirilemedi")?;
    if bytes.len() as u64 > limit || hash(&bytes) != digest {
        bail!("Eklenti bütünlük denetimi başarısız")
    }
    Ok(bytes)
}

#[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
fn capture(mut command: std::process::Command, cwd: &Path) -> Result<()> {
    use std::process::Stdio;
    command
        .current_dir(external_path(cwd))
        .env("DO_NOT_TRACK", "1")
        .env("OPENSPEC_TELEMETRY", "0")
        .env("CI", "true")
        .stdin(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000);
    }
    if !command.output()?.status.success() {
        bail!("Eklenti hazırlığı tamamlanamadı")
    }
    Ok(())
}

fn prepare_distribution(root: &Path, addon: &Addon) -> Result<()> {
    let client = reqwest::blocking::Client::builder()
        .https_only(true)
        .timeout(Duration::from_secs(60))
        .build()?;
    if addon.id == "openspec" {
        #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
        {
            let archive = root.join("openspec.tar.gz");
            fs::write(&archive,download(&client,OPENSPEC_ASSET,OPENSPEC_ARCHIVE_HASH,64*1024*1024)?)?;
            crate::platform::macos::prepare_openspec(root,&archive,OPENSPEC_EXE_HASH)?;
            fs::remove_file(archive)?;
        }
        #[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
        {
        if !cfg!(all(windows, target_arch = "x86_64")) {
            bail!("OpenSpec paketi Windows x64 gerektirir")
        }
        let archive = root.join("openspec.zip");
        fs::write(
            &archive,
            download(
                &client,
                OPENSPEC_ASSET,
                OPENSPEC_ARCHIVE_HASH,
                64 * 1024 * 1024,
            )?,
        )?;
        let system = PathBuf::from(
            std::env::var_os("SystemRoot").context("Windows sistem klasörü bulunamadı")?,
        );
        let mut tar = std::process::Command::new(system.join("System32/tar.exe"));
        tar.args(["-xf", "openspec.zip", "openspec.exe"]);
        capture(tar, root)?;
        verified_bytes(root, "openspec.exe", OPENSPEC_EXE_HASH, EXE_LIMIT)?;
        fs::remove_file(archive)?;
        let template = root.join("template");
        fs::create_dir(&template)?;
        let mut exe = std::process::Command::new(external_path(&root.join("openspec.exe")));
        exe.args([
            "init",
            "--tools",
            "agents",
            "--profile",
            "core",
            "--no-animation",
        ]);
        let config = template.join("config");
        fs::create_dir(&config)?;
        exe.env_clear()
            .env("SystemRoot", &system)
            .env("USERPROFILE", &config)
            .env("TEMP", &config)
            .env("TMP", &config)
            .env("XDG_CONFIG_HOME", &config)
            .env("APPDATA", &config)
            .env("LOCALAPPDATA", &config);
        capture(exe, &template)?;
        }
    }
    for (name, digest) in addon.skills {
        let relative = skill_file(name);
        let bytes = if addon.id == "openspec" {
            verified_bytes(
                root,
                &format!("template/.agents/skills/{relative}"),
                digest,
                SKILL_LIMIT,
            )?
        } else {
            let url = format!("https://raw.githubusercontent.com/DietrichGebert/ponytail/{PONYTAIL_COMMIT}/skills/{relative}");
            download(&client, &url, digest, SKILL_LIMIT)?
        };
        atomic(&root.join("skills").join(relative), &bytes)?;
    }
    if addon.id == "openspec" {
        fs::remove_dir_all(root.join("template"))?;
    }
    Ok(())
}

fn install(core: &Core, args: &Value) -> Result<Value> {
    let addon = catalog(string(args, "id")?)?;
    if args["revision"] != addon.revision {
        bail!("Eklenti sürümünü ve dosyalarını kütüphaneden onayla")
    }
    let _guard = core.operations.lock().unwrap();
    let root = safe_path(&core.directory, &format!("addons/{}", addon.id))?;
    fs::create_dir_all(&root)?;
    let generation = id();
    let candidate = safe_path(&root, &generation)?;
    fs::create_dir(&candidate)?;
    let result = prepare_distribution(&candidate, addon).and_then(|()| {
        atomic(&root.join("verified.json"), &serde_json::to_vec(
            &json!({"version":addon.version,"revision":addon.revision,"generation":generation,"files":skill_hashes(addon)})
        )?)
    });
    if result.is_err() {
        let _ = fs::remove_dir_all(candidate);
    }
    result?;
    Ok(list(core))
}
pub fn dispatch(core: &Arc<Core>, action: &str, args: &Value) -> Result<Value> {
    match action {
        "list" => Ok(list(core)),
        "install" => install(core, args),
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
        let addon = catalog(addon)?;
        let addon_root = safe_path(&core.directory, &format!("addons/{}", addon.id))?;
        let installed = installed_root(&addon_root, addon)?;
        for (name, digest) in addon.skills {
            let relative = skill_file(name);
            let bytes = verified_bytes(
                &installed,
                &format!("skills/{relative}"),
                digest,
                SKILL_LIMIT,
            )?;
            let key = format!("{prefix}/{relative}");
            if desired.insert(key, bytes).is_some() {
                bail!("Eklentiler aynı beceri dosyasını sağlıyor")
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
        if current.as_deref() != Some(digest.as_str()) {
            changes.push((path.clone(), Some(bytes)));
        }
        next.insert(path, json!(digest));
    }
    for (path, bytes) in changes {
        crate::files::apply_bytes(&safe_path(&root, &path)?, bytes.as_deref())?;
    }
    atomic(&manifest_file, &serde_json::to_vec_pretty(&next)?)?;
    if ids.iter().any(|id| id == "openspec") {
        fs::create_dir_all(safe_path(&root, "openspec/specs")?)?;
        fs::create_dir_all(safe_path(&root, "openspec/changes")?)?;
        let config = safe_path(&root, "openspec/config.yaml")?;
        if !config.exists() {
            atomic(&config, b"schema: spec-driven\n")?;
        }
    }
    Ok(())
}
