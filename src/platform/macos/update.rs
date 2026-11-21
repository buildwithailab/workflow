//! A bounded, application-owned installer. No privileged or persistent service.
use anyhow::{bail, Context, Result};
use std::{collections::HashSet, fs, io::{Cursor, Read, Write}, os::unix::fs::PermissionsExt, path::{Path, PathBuf}, process::{Command, Stdio}};
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

pub(crate) fn install(source: &Path, receipt: &Path, restart: bool, expected_hash: &str, expected_version: &str) -> Result<()> {
    let executable = std::env::current_exe()?.canonicalize()?;
    let destination = executable.parent().and_then(Path::parent).and_then(Path::parent).context("Uygulama paketi bulunamadı")?;
    if destination.extension().and_then(|s| s.to_str()) != Some("app") || executable.parent().and_then(Path::file_name).and_then(|s|s.to_str()) != Some("MacOS") { bail!("Güncelleme bir uygulama paketi gerektirir"); }
    metadata(destination, env!("CARGO_PKG_VERSION"))?;
    let mut bytes=Vec::new();fs::File::open(source)?.take(crate::update_manifest::MAX_ASSET+1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > crate::update_manifest::MAX_ASSET || crate::core::hash(&bytes) != expected_hash { bail!("Güncelleme dosyası doğrulanamadı"); }
    let parent = destination.parent().context("Uygulama klasörü bulunamadı")?;
    // Same-volume staging allows atomic renames; read-only/translocated apps fail here.
    let staging = parent.join(format!(".workflow-update-{}", uuid::Uuid::new_v4()));
    fs::create_dir(&staging).context("Uygulamayı yazılabilir bir Applications klasörüne taşı")?;
    fs::set_permissions(&staging, fs::Permissions::from_mode(0o700))?;
    let prepared = (|| -> Result<()> {
        let bundle = extract(&bytes, &staging)?;
        verify_bundle(&bundle, expected_version)?;
        let script = staging.join("install.sh");
        crate::core::atomic(&script, include_bytes!("apply_update.sh"))?;
        Command::new("/bin/sh").arg(&script).arg(std::process::id().to_string()).arg(destination).arg(&staging).arg(receipt)
            .arg(if restart {"1"} else {"0"}).arg(env!("CARGO_PKG_VERSION"))
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
        for tamper in [false,true] {
            let directory=std::env::temp_dir().join(format!("workflow-install-{}",uuid::Uuid::new_v4()));fs::create_dir(&directory).unwrap();
            let old=extract(&bytes,&directory).unwrap();verify_bundle(&old,env!("CARGO_PKG_VERSION")).unwrap();
            let staging=directory.join("staging");fs::create_dir(&staging).unwrap();
            let new=extract(&bytes,&staging).unwrap();verify_bundle(&new,env!("CARGO_PKG_VERSION")).unwrap();
            if tamper { fs::write(new.join("Contents/Resources/LICENSE.txt"),b"synthetic tamper").unwrap(); }
            let receipt=directory.join("receipt");
            // An already exited, freshly reaped child is used instead of any user process.
            let mut exited=Command::new("/usr/bin/true").spawn().unwrap();let pid=exited.id();exited.wait().unwrap();
            assert!(Command::new("/bin/sh").arg("-c").arg(include_str!("apply_update.sh")).arg("workflow-update-test")
                .arg(pid.to_string()).arg(&old).arg(&staging).arg(&receipt).arg("0").arg(env!("CARGO_PKG_VERSION")).status().unwrap().success());
            assert_eq!(fs::read_to_string(receipt).unwrap(),if tamper {"failed"} else {"installed"});
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
