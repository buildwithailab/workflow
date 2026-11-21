//! A bounded, application-owned installer. No privileged or persistent service.
use anyhow::{bail, Context, Result};
use std::{collections::HashSet, ffi::CString, fs, io::{Cursor, Read, Write}, os::unix::{ffi::OsStrExt, fs::{MetadataExt, OpenOptionsExt, PermissionsExt}}, path::{Path, PathBuf}, process::{Command, Stdio}, time::Duration};
use serde::{Deserialize, Serialize};
use unicode_normalization::UnicodeNormalization;

fn archive_path(name: &str) -> Result<PathBuf> {
    if !name.starts_with("Workflow.app/") || name.contains(['\\', ':']) || name.chars().any(char::is_control)
        || name.trim_end_matches('/').split('/').any(|part| part.is_empty() || part == "." || part == "..") {
        bail!("Güncelleme arşivinde geçersiz yol");
    }
    for part in name.split('/') {
        let lower = part.to_ascii_lowercase();
        if lower.starts_with(".env") || [".git",".codex",".claude",".copilot","accounts","profiles","sessions","transcripts","leases","ui-profile","browser-profiles","teams-profile","cookies","login data","local state","auth.json","auth-status.json","workspace.json","ready.json"].contains(&lower.as_str())
            || lower.trim_start_matches('.').starts_with("credentials") && lower.ends_with(".json")
            || ["pem","key","pfx","p12","log","jsonl","db","db-wal","db-shm"].contains(&lower.rsplit('.').next().unwrap_or(""))
            || lower.rsplit('.').next().unwrap_or("").starts_with("sqlite") {
            bail!("Güncelleme arşivi uygulama dışı veri içeriyor");
        }
    }
    Ok(PathBuf::from(name))
}

fn extract(bytes: &[u8], directory: &Path) -> Result<PathBuf> {
    let mut archive = zip::ZipArchive::new(Cursor::new(bytes))?;
    if archive.len() > 8192 { bail!("Güncelleme arşivi çok büyük"); }
    let mut names = HashSet::new();
    let mut expanded = 0u64;
    for index in 0..archive.len() {
        let mut entry = archive.by_index(index)?;
        let relative = archive_path(entry.name())?;
        let normalized = entry.name().trim_end_matches('/').nfc().collect::<String>().to_lowercase();
        if !names.insert(normalized) { bail!("Güncelleme arşivinde yinelenen yol"); }
        let mode = entry.unix_mode().unwrap_or(if entry.is_dir() {0o40755} else {0o100644});
        if !matches!(mode & 0o170000, 0 | 0o040000 | 0o100000) || (mode & 0o170000 == 0o040000) != entry.is_dir() {
            bail!("Güncelleme arşivinde bağlantı veya özel dosya");
        }
        expanded = expanded.checked_add(entry.size()).context("Arşiv boyutu geçersiz")?;
        if expanded > 1024*1024*1024 || entry.size() > 128*1024*1024 { bail!("Güncelleme arşivi çok büyük"); }
        let output = directory.join(relative);
        if entry.is_dir() { fs::create_dir_all(&output)?; continue; }
        fs::create_dir_all(output.parent().context("Arşiv yolu geçersiz")?)?;
        let mut file = fs::OpenOptions::new().write(true).create_new(true).open(&output)?;
        let expected = entry.size();
        let written = std::io::copy(&mut (&mut entry).take(expected + 1), &mut file)?;
        if written != expected { bail!("Güncelleme dosyası eksik veya çok büyük"); }
        file.flush()?;
        fs::set_permissions(&output, fs::Permissions::from_mode(if mode & 0o111 != 0 {0o755} else {0o644}))?;
    }
    Ok(directory.join("Workflow.app"))
}

fn unique_metadata(bytes: &[u8]) -> Result<()> {
    use quick_xml::{events::Event,Reader};
    let mut reader=Reader::from_reader(bytes);
    let mut tags: Vec<String>=Vec::new();let mut dictionaries: Vec<HashSet<String>>=Vec::new();
    loop {
        match reader.read_event()? {
            Event::Start(tag) if tag.name().as_ref()=="key" => {
                if tags.last().map(String::as_str)!=Some("dict") {bail!("Uygulama bilgisi geçersiz");}
                let text=reader.read_text(tag.name())?;
                if text.as_ref().contains(['<','>']) {bail!("Uygulama bilgisi işaretli XML anahtarı içeriyor");}
                let key=quick_xml::escape::unescape(text.as_ref())?.into_owned();
                if !dictionaries.last_mut().context("Uygulama bilgisi geçersiz")?.insert(key) {bail!("Uygulama bilgisinde yinelenen alan");}
            }
            Event::Start(tag) => {
                if tag.name().as_ref()=="dict" {dictionaries.push(HashSet::new());}
                tags.push(tag.name().as_ref().to_owned());
                if tags.len()>64 {bail!("Uygulama bilgisi çok karmaşık");}
            }
            Event::Empty(tag) if tag.name().as_ref()=="key" => {
                if !dictionaries.last_mut().context("Uygulama bilgisi geçersiz")?.insert(String::new()) {bail!("Uygulama bilgisinde yinelenen alan");}
            }
            Event::End(tag) => {
                if tags.pop().as_deref()!=Some(tag.name().as_ref()) {bail!("Uygulama bilgisi geçersiz");}
                if tag.name().as_ref()=="dict" {dictionaries.pop();}
            }
            Event::DocType(text) if text.as_ref().contains('[') => bail!("Uygulama bilgisi özel XML varlıkları içeriyor"),
            Event::Eof => break,
            _=>{}
        }
    }
    if !tags.is_empty() || !dictionaries.is_empty(){bail!("Uygulama bilgisi eksik");}Ok(())
}

fn metadata(bundle: &Path, expected: &str) -> Result<String> {
    let path = bundle.join("Contents/Info.plist");
    if fs::metadata(&path)?.len() > 1024*1024 { bail!("Uygulama bilgisi çok büyük"); }
    let mut bytes=Vec::new();fs::File::open(path)?.take(1024*1024+1).read_to_end(&mut bytes)?;
    if bytes.len()>1024*1024 {bail!("Uygulama bilgisi çok büyük");}
    unique_metadata(&bytes)?;
    let value = plist::Value::from_reader(Cursor::new(bytes))?;
    let fields = value.as_dictionary().context("Uygulama bilgisi geçersiz")?;
    let field = |name: &str| fields.get(name).and_then(plist::Value::as_string);
    if field("CFBundleIdentifier") != Some("local.workflow.desktop") || field("CFBundleShortVersionString") != Some(expected)
        || field("CFBundleVersion") != Some(expected) { bail!("Uygulama sürümü veya kimliği geçersiz"); }
    let executable = field("CFBundleExecutable").context("Uygulama dosyası eksik")?;
    if !["workflow","Workflow"].contains(&executable) { bail!("Uygulama dosyası geçersiz"); }
    Ok(executable.to_owned())
}

fn verify_bundle(bundle: &Path, expected: &str) -> Result<()> {
    let executable = bundle.join("Contents/MacOS").join(metadata(bundle, expected)?);
    if fs::metadata(&executable)?.len() > 128*1024*1024 || fs::metadata(&executable)?.permissions().mode() & 0o111 == 0 { bail!("Uygulama dosyası geçersiz"); }
    let bytes = fs::read(executable)?;
    if bytes.len() < 32 || bytes[..4] != [0xcf,0xfa,0xed,0xfe] || u32::from_le_bytes(bytes[4..8].try_into()?) != 0x0100000c
        || u32::from_le_bytes(bytes[12..16].try_into()?) != 2
        || !bytes.windows(include_str!("../../update-public-key.txt").trim().len()).any(|part| part == include_str!("../../update-public-key.txt").trim().as_bytes()) {
        bail!("Uygulama mimarisi veya doğrulama anahtarı geçersiz");
    }
    if !Command::new("/usr/bin/codesign").args(["--verify","--deep","--strict","--all-architectures"]).arg(bundle).output()?.status.success() {
        bail!("Uygulama kod imzası doğrulanamadı");
    }
    Ok(())
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Plan {
    parent: u32,
    destination: PathBuf,
    receipt: PathBuf,
    source: PathBuf,
    restart: bool,
    profile: Option<PathBuf>,
    profile_environment: Vec<(String, PathBuf)>,
    installed_version: String,
    installed_hash: String,
    version: String,
    executable_hash: String,
}

const PROFILE_ENVIRONMENT: [&str; 5] = ["HOME", "CODEX_HOME", "CLAUDE_CONFIG_DIR", "COPILOT_HOME", "GH_CONFIG_DIR"];

fn bundle_hash(bundle: &Path, expected: &str) -> Result<String> {
    verify_bundle(bundle, expected)?;
    Ok(crate::core::hash(&fs::read(bundle.join("Contents/MacOS").join(metadata(bundle, expected)?))?))
}

fn rename_exclusive(source: &Path, destination: &Path) -> Result<()> {
    let source = CString::new(source.as_os_str().as_bytes())?;
    let destination = CString::new(destination.as_os_str().as_bytes())?;
    let result = unsafe { libc::renameatx_np(libc::AT_FDCWD, source.as_ptr(), libc::AT_FDCWD, destination.as_ptr(), libc::RENAME_EXCL) };
    if result != 0 { return Err(std::io::Error::last_os_error().into()); }
    Ok(())
}

fn same_directory(path: &Path, identity: (u64, u64)) -> bool {
    fs::symlink_metadata(path).is_ok_and(|m| m.is_dir() && (m.dev(), m.ino()) == identity)
}

fn replace_bundle(plan: &Plan, staging: &Path) -> Result<&'static str> {
    let new = staging.join("Workflow.app");
    let backup = staging.join("previous.app");
    if bundle_hash(&plan.destination, &plan.installed_version)? != plan.installed_hash
        || bundle_hash(&new, &plan.version)? != plan.executable_hash {
        bail!("Güncelleme hazırlanırken uygulama değişti");
    }
    let m = fs::symlink_metadata(&new)?;
    let identity = (m.dev(), m.ino());
    rename_exclusive(&plan.destination, &backup)?;
    if rename_exclusive(&new, &plan.destination).is_err() {
        return Ok(if rename_exclusive(&backup, &plan.destination).is_ok() { "failed" } else { "rollback-failed" });
    }
    if bundle_hash(&plan.destination, &plan.version).ok().as_ref() != Some(&plan.executable_hash) {
        if !same_directory(&plan.destination, identity)
            || rename_exclusive(&plan.destination, &staging.join("rejected.app")).is_err()
            || rename_exclusive(&backup, &plan.destination).is_err() {
            return Ok("rollback-failed");
        }
        return Ok("failed");
    }
    // A cleanup failure must retain the recovery copy rather than hide it.
    let _ = fs::remove_dir_all(backup);
    Ok("installed")
}

fn parent_exited(parent: u32) -> Result<bool> {
    if parent <= 1 || parent > i32::MAX as u32 || parent == std::process::id() { bail!("Kurulum işlemi geçersiz"); }
    let result = unsafe { libc::kill(parent as i32, 0) };
    if result == 0 { return Ok(false); }
    if std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) { return Ok(true); }
    bail!("Uygulamanın kapanması doğrulanamadı")
}

fn read_plan(staging: &Path, helper: &Path) -> Result<Plan> {
    let m = fs::symlink_metadata(staging)?;
    let suffix = staging.file_name().and_then(|s|s.to_str()).and_then(|s|s.strip_prefix(".workflow-update-")).context("Kurulum klasörü geçersiz")?;
    uuid::Uuid::parse_str(suffix)?;
    if !m.is_dir() || m.uid() != unsafe {libc::geteuid()} || m.permissions().mode() & 0o077 != 0 { bail!("Kurulum klasörü özel değil"); }
    let mut file = fs::OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK).open(staging.join("install.json"))?;
    if !file.metadata()?.is_file() || file.metadata()?.len() > 64*1024 {bail!("Kurulum bilgisi geçersiz");}
    let mut bytes = Vec::new();Read::by_ref(&mut file).take(64*1024+1).read_to_end(&mut bytes)?;
    if bytes.len()>64*1024 {bail!("Kurulum bilgisi çok büyük");}
    let plan: Plan = serde_json::from_slice(&bytes)?;
    if plan.installed_version != env!("CARGO_PKG_VERSION") || crate::update_manifest::version(&plan.version).is_none()
        || plan.destination.extension().and_then(|s|s.to_str()) != Some("app")
        || plan.destination.parent() != staging.parent()
        || plan.destination.symlink_metadata().is_ok_and(|m|m.file_type().is_symlink())
        || plan.receipt.file_name().and_then(|s|s.to_str()) != Some("install-result")
        || plan.source.parent() != plan.receipt.parent()
        || plan.source.extension().and_then(|s|s.to_str()) != Some("download")
        || plan.receipt.parent().context("Kurulum sonucu klasörü eksik")?.canonicalize()?.join("install-result") != plan.receipt
        || fs::metadata(plan.receipt.parent().unwrap())?.uid() != unsafe {libc::geteuid()}
        || crate::core::hash(&fs::read(helper)?) != plan.installed_hash {
        bail!("Kurulum bilgisi doğrulanamadı");
    }
    if plan.profile.as_ref().is_some_and(|p|!p.is_absolute()) {bail!("Veri klasörü geçersiz");}
    let mut names=HashSet::new();
    if plan.profile_environment.iter().any(|(name,path)|plan.profile.is_none() || !PROFILE_ENVIRONMENT.contains(&name.as_str())
        || !path.is_absolute() || !names.insert(name)) {bail!("Profil klasörleri geçersiz");}
    Ok(plan)
}

fn finish_install() -> Result<()> {
    let helper = std::env::current_exe()?.canonicalize()?;
    if helper.file_name().and_then(|s|s.to_str()) != Some("installer") {bail!("Kurulum yardımcısı geçersiz");}
    let staging = helper.parent().context("Kurulum klasörü eksik")?;
    let plan = read_plan(staging, &helper)?;
    let result = (|| -> Result<&'static str> {
        let mut exited = false;
        for _ in 0..30 {
            if parent_exited(plan.parent)? {exited=true;break;}
            std::thread::sleep(Duration::from_secs(1));
        }
        if !exited && !parent_exited(plan.parent)? {bail!("Uygulama kapanmadı");}
        replace_bundle(&plan, staging)
    })().unwrap_or("failed");
    let receipt = crate::core::atomic(&plan.receipt, result.as_bytes());
    let _ = fs::remove_file(&plan.source);
    let restarted = if result == "installed" && plan.restart {
        let mut command = Command::new("/usr/bin/open");
        command.arg("-n").arg("-a").arg(&plan.destination);
        if let Some(profile) = &plan.profile {command.arg("--env").arg(format!("WORKFLOW_DATA_DIR={}",profile.to_str().context("Veri klasörü geçersiz")?));}
        for (name,path) in &plan.profile_environment {command.arg("--env").arg(format!("{name}={}",path.to_str().context("Profil klasörü geçersiz")?));}
        command.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok_and(|status|status.success())
    } else {true};
    if !restarted {let _ = crate::core::atomic(&plan.receipt,b"restart-failed");}
    // Keep the only recovery copy if rollback or backup cleanup could not finish.
    if !staging.join("previous.app").exists() {let _=fs::remove_dir_all(staging);}
    receipt?;
    if result != "installed" || !restarted {bail!("Güncelleme tamamlanamadı");}
    Ok(())
}

pub(crate) fn helper(args: &[String]) -> Option<i32> {
    args.get(1).filter(|arg|arg.as_str()=="--apply-update").map(|_| {
        if args.len()==2 && finish_install().is_ok() {0} else {1}
    })
}

pub(crate) fn install(source: &Path, receipt: &Path, restart: bool, expected_hash: &str, expected_version: &str) -> Result<()> {
    let executable = std::env::current_exe()?.canonicalize()?;
    let destination = executable.parent().and_then(Path::parent).and_then(Path::parent).context("Uygulama paketi bulunamadı")?;
    if destination.extension().and_then(|s| s.to_str()) != Some("app") || executable.parent().and_then(Path::file_name).and_then(|s|s.to_str()) != Some("MacOS") { bail!("Güncelleme bir uygulama paketi gerektirir"); }
    let installed_hash=bundle_hash(destination, env!("CARGO_PKG_VERSION"))?;
    let mut bytes=Vec::new();fs::File::open(source)?.take(crate::update_manifest::MAX_ASSET+1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > crate::update_manifest::MAX_ASSET || crate::core::hash(&bytes) != expected_hash { bail!("Güncelleme dosyası doğrulanamadı"); }
    let parent = destination.parent().context("Uygulama klasörü bulunamadı")?;
    // Same-volume staging allows atomic renames; read-only/translocated apps fail here.
    let staging = parent.join(format!(".workflow-update-{}", uuid::Uuid::new_v4()));
    fs::create_dir(&staging).context("Uygulamayı yazılabilir bir Applications klasörüne taşı")?;
    fs::set_permissions(&staging, fs::Permissions::from_mode(0o700))?;
    let prepared = (|| -> Result<()> {
        let bundle = extract(&bytes, &staging)?;
        let profile=crate::profile_override().map(PathBuf::from).map(std::path::absolute).transpose()?;
        // Explicit custom profiles must reopen in the same homes. Only location
        // variables are forwarded; credentials and arbitrary environment are not.
        let profile_environment=if profile.is_some() {PROFILE_ENVIRONMENT.iter().filter_map(|name|
            std::env::var_os(name).map(PathBuf::from).filter(|path|path.is_absolute()).map(|path|(name.to_string(),path))).collect()} else {Vec::new()};
        let plan=Plan {parent:std::process::id(),destination:destination.to_owned(),
            receipt:receipt.parent().context("Kurulum sonucu klasörü eksik")?.canonicalize()?.join("install-result"),
            source:source.canonicalize()?,restart,profile,profile_environment,
            installed_version:env!("CARGO_PKG_VERSION").to_owned(),installed_hash,
            version:expected_version.to_owned(),executable_hash:bundle_hash(&bundle, expected_version)?};
        let helper=staging.join("installer");
        fs::copy(&executable,&helper)?;
        fs::set_permissions(&helper,fs::Permissions::from_mode(0o700))?;
        crate::core::atomic(&staging.join("install.json"),&serde_json::to_vec(&plan)?)?;
        // Validate the same plan before committing the application to exit.
        read_plan(&staging,&helper)?;
        Command::new(&helper).arg("--apply-update")
            .stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn()?;
        Ok(())
    })();
    if prepared.is_err() { let _ = fs::remove_dir_all(&staging); }
    prepared
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn exclusive_rename_never_nests_or_overwrites_an_existing_directory() {
        let root=std::env::temp_dir().join(format!("workflow-rename-{}",uuid::Uuid::new_v4()));
        fs::create_dir(&root).unwrap();
        let source=root.join("Workflow.app");let destination=root.join("existing.app");
        fs::create_dir(&source).unwrap();fs::create_dir(&destination).unwrap();
        fs::write(source.join("new"),b"synthetic new bundle").unwrap();
        fs::write(destination.join("old"),b"synthetic old bundle").unwrap();
        assert!(rename_exclusive(&source,&destination).is_err());
        assert_eq!(fs::read(destination.join("old")).unwrap(),b"synthetic old bundle");
        assert!(!destination.join("Workflow.app").exists());assert!(source.join("new").is_file());
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn helper_rejects_invalid_parents_and_observes_a_reaped_child() {
        assert!(parent_exited(0).is_err());assert!(parent_exited(1).is_err());assert!(parent_exited(std::process::id()).is_err());
        let mut child=Command::new("/usr/bin/true").spawn().unwrap();let pid=child.id();child.wait().unwrap();
        assert!(parent_exited(pid).unwrap());
    }
    #[test]
    fn duplicate_metadata_is_rejected_even_if_the_first_value_is_a_collection() {
        let duplicate=br#"<plist version="1.0"><dict><key>CFBundleVersion</key><array/><key>CFBundleVersion</key><string>1.2.3</string></dict></plist>"#;
        assert!(unique_metadata(duplicate).is_err());
        assert!(unique_metadata(br#"<plist version="1.0"><dict><key><![CDATA[CFBundleVersion]]></key><string>1.2.3</string><key>CFBundleVersion</key><string>1.2.3</string></dict></plist>"#).is_err());
        assert!(unique_metadata(br#"<plist version="1.0"><dict><key>Items</key><array><dict><key>Label</key><string>synthetic</string></dict></array></dict></plist>"#).is_ok());
    }
    #[test]
    #[ignore = "Requires a locally built, ad-hoc signed Mac package in ignored storage"]
    fn local_package_extracts_verifies_and_installs_only_in_a_disposable_directory() {
        let path=std::env::var_os("WORKFLOW_MAC_PACKAGE").expect("Local candidate required");
        let bytes=fs::read(path).unwrap();
        for scenario in ["install","tamper","rollback","occupied"] {
            let directory=std::env::temp_dir().join(format!("workflow-install-{}",uuid::Uuid::new_v4()));fs::create_dir(&directory).unwrap();
            let old=extract(&bytes,&directory).unwrap();verify_bundle(&old,env!("CARGO_PKG_VERSION")).unwrap();
            let staging=directory.join("staging");fs::create_dir(&staging).unwrap();
            let new=extract(&bytes,&staging).unwrap();verify_bundle(&new,env!("CARGO_PKG_VERSION")).unwrap();
            let original=bundle_hash(&old,env!("CARGO_PKG_VERSION")).unwrap();
            let plan=Plan {parent:2,destination:old.clone(),receipt:directory.join("install-result"),source:directory.join("synthetic.download"),restart:false,profile:None,profile_environment:Vec::new(),
                installed_version:env!("CARGO_PKG_VERSION").into(),installed_hash:original.clone(),version:env!("CARGO_PKG_VERSION").into(),executable_hash:bundle_hash(&new,env!("CARGO_PKG_VERSION")).unwrap()};
            if scenario=="tamper" {fs::write(new.join("Contents/Resources/LICENSE.txt"),b"synthetic tamper").unwrap();}
            if scenario=="rollback" {
                let path=CString::new(new.as_os_str().as_bytes()).unwrap();
                assert_eq!(unsafe{libc::chflags(path.as_ptr(),libc::UF_IMMUTABLE)},0);
            }
            if scenario=="occupied" {fs::create_dir(staging.join("previous.app")).unwrap();}
            let result=replace_bundle(&plan,&staging).unwrap_or("failed");
            if scenario=="rollback" {
                let path=CString::new(new.as_os_str().as_bytes()).unwrap();
                assert_eq!(unsafe{libc::chflags(path.as_ptr(),0)},0);
            }
            assert_eq!(result,if scenario=="install" {"installed"} else {"failed"});
            assert_eq!(bundle_hash(&old,env!("CARGO_PKG_VERSION")).unwrap(),original);
            verify_bundle(&old,env!("CARGO_PKG_VERSION")).unwrap();
            fs::remove_dir_all(directory).unwrap();
        }
    }
    #[test]
    fn rejects_unsafe_archive_paths() {
        for path in ["../Workflow.app/a", "Other.app/a", "Workflow.app/../a", "Workflow.app//a", "Workflow.app/a\\b", "Workflow.app/Contents/.env", "Workflow.app/sessions/a", "Workflow.app/a.key"] { assert!(archive_path(path).is_err()); }
        assert!(archive_path("Workflow.app/Contents/MacOS/workflow").is_ok());
    }
    #[test]
    fn rejects_duplicate_case_links_and_traversal_before_writing_outside_staging() {
        for (names, mode) in [(vec!["Workflow.app/a","workflow.app/A"],0o644),(vec!["Workflow.app/link"],0o120777),(vec!["Workflow.app/../outside"],0o644)] {
            let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
            for name in names {
                if mode == 0o120777 { writer.add_symlink(name,"outside",zip::write::SimpleFileOptions::default()).unwrap(); }
                else { writer.start_file(name,zip::write::SimpleFileOptions::default().unix_permissions(mode)).unwrap(); writer.write_all(b"synthetic").unwrap(); }
            }
            let bytes = writer.finish().unwrap().into_inner();
            let directory = std::env::temp_dir().join(format!("workflow-update-{}",uuid::Uuid::new_v4()));fs::create_dir(&directory).unwrap();
            assert!(extract(&bytes,&directory).is_err());fs::remove_dir_all(directory).unwrap();
        }
    }
}
