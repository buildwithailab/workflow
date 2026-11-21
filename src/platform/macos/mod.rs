pub(crate) mod process;
pub(crate) mod webview;
pub(crate) mod preview;
mod update;
mod addons;
mod startup;
pub(crate) use startup::{complete_startup, handle_startup_failure, startup_completed, startup_error};
pub(crate) use addons::prepare as prepare_openspec;
pub(crate) use update::install as update_installer;
#[cfg(feature = "desktop-tests")]
pub(crate) mod test_bridge;

pub(crate) fn own_window_state(window: &tauri::WebviewWindow) -> anyhow::Result<()> {
    // Workflow restores its own workspace. AppKit must not recreate old native
    // windows or snapshots from another isolated profile.
    let native=window.clone();
    let apply=move || {
        if let Ok(pointer)=native.ns_window() {
            let window=unsafe {&*pointer.cast::<objc2_app_kit::NSWindow>()};
            window.setRestorable(false);
            window.disableSnapshotRestoration();
        }
    };
    if objc2::MainThreadMarker::new().is_some() {apply();}else{window.run_on_main_thread(apply)?;}
    Ok(())
}

pub(crate) fn private_data_directory(directory: &std::path::Path, default_directory: Option<&std::path::Path>) -> anyhow::Result<()> {
    use anyhow::{bail,Context};
    use std::{fs,io::Read,os::unix::fs::{DirBuilderExt,MetadataExt,PermissionsExt}};
    let absolute=std::path::absolute(directory)?;
    if absolute.parent().is_none() || absolute==crate::core::home() || absolute==std::env::temp_dir() {
        bail!("Workflow verileri için ayrı ve özel bir klasör seç.");
    }
    if fs::symlink_metadata(directory).is_ok_and(|metadata|metadata.file_type().is_symlink()) {
        bail!("Veri klasörü için sembolik bağlantı yerine gerçek klasörü seç.");
    }
    let existed=directory.try_exists()?;
    fs::DirBuilder::new().recursive(true).mode(0o700).create(directory)?;
    let canonical=directory.canonicalize()?;
    if canonical.parent().is_none() || crate::core::home().canonicalize().ok().as_ref()==Some(&canonical)
        || std::env::temp_dir().canonicalize().ok().as_ref()==Some(&canonical) {
        bail!("Workflow verileri için ayrı ve özel bir klasör seç.");
    }
    let metadata=fs::metadata(directory)?;
    if !metadata.is_dir() || metadata.uid()!=unsafe{libc::geteuid()} {bail!("Veri klasörü bu kullanıcıya ait değil.");}
    if metadata.permissions().mode()&0o077!=0 {
        let known_default=default_directory.and_then(|path|path.canonicalize().ok()).is_some_and(|path|directory.canonicalize().is_ok_and(|actual|actual==path));
        let database=directory.join("workspace.json");
        let legacy=known_default && fs::symlink_metadata(&database).is_ok_and(|metadata|metadata.file_type().is_file()) && (||->Option<bool>{
            let mut bytes=Vec::new();fs::File::open(database).ok()?.take(8*1024*1024+1).read_to_end(&mut bytes).ok()?;
            if bytes.len()>8*1024*1024 {return None;}
            let value: serde_json::Value=serde_json::from_slice(&bytes).ok()?;
            Some(value["version"]==1 && ["projects","sessions","links","jobs"].iter().all(|key|value[key].is_array())
                && value["preferences"].is_object() && value["providers"].is_object())
        })()==Some(true);
        // Only the known default location with a real Workflow database can
        // receive a legacy permission migration. A generic lock proves nothing.
        if existed && fs::read_dir(directory)?.next().transpose()?.is_some() && !legacy {
            bail!("Workflow verileri için ayrı ve özel bir klasör seç.");
        }
        fs::set_permissions(directory,fs::Permissions::from_mode(0o700))?;
        if fs::metadata(directory)?.permissions().mode()&0o077!=0 {bail!("Veri klasörü özel erişimi desteklemiyor.");}
    }
    directory.to_str().context("Veri klasörü yolu geçersiz")?;
    Ok(())
}

#[cfg(test)]
mod data_tests {
    use super::*;
    use std::{fs,os::unix::fs::PermissionsExt};
    #[test]
    fn profiles_are_private_and_unrelated_populated_folders_are_not_chmodded() {
        let root=std::env::temp_dir().join(format!("workflow-private-{}",uuid::Uuid::new_v4()));fs::create_dir(&root).unwrap();
        let profile=root.join("profile");private_data_directory(&profile,None).unwrap();assert_eq!(fs::metadata(&profile).unwrap().permissions().mode()&0o077,0);
        let unrelated=root.join("unrelated");fs::create_dir(&unrelated).unwrap();fs::write(unrelated.join("user-file"),b"synthetic user data").unwrap();fs::set_permissions(&unrelated,fs::Permissions::from_mode(0o755)).unwrap();
        assert!(private_data_directory(&unrelated,None).is_err());
        fs::write(unrelated.join("instance.lock"),b"").unwrap();
        assert!(private_data_directory(&unrelated,None).is_err());assert_eq!(fs::metadata(&unrelated).unwrap().permissions().mode()&0o777,0o755);assert_eq!(fs::read(unrelated.join("user-file")).unwrap(),b"synthetic user data");
        let link=root.join("linked-profile");std::os::unix::fs::symlink(&unrelated,&link).unwrap();assert!(private_data_directory(&link,None).is_err());assert_eq!(fs::metadata(&unrelated).unwrap().permissions().mode()&0o777,0o755);
        fs::write(profile.join("workspace.json"),serde_json::to_vec(&serde_json::json!({"version":1,"projects":[],"sessions":[],"links":[],"jobs":[],"preferences":{},"providers":{}})).unwrap()).unwrap();
        fs::set_permissions(&profile,fs::Permissions::from_mode(0o755)).unwrap();
        assert!(private_data_directory(&profile,None).is_err());private_data_directory(&profile,Some(&profile)).unwrap();assert_eq!(fs::metadata(&profile).unwrap().permissions().mode()&0o077,0);
        fs::remove_dir_all(root).unwrap();
    }
}

pub(crate) fn data_store(directory: &std::path::Path, scope: &str) -> [u8; 16] {
    use sha2::{Digest, Sha256};
    let canonical = directory.canonicalize().unwrap_or_else(|_| directory.to_path_buf());
    let digest = Sha256::digest(format!("local.workflow.desktop\0{}\0{scope}", canonical.display()).as_bytes());
    let mut bytes = [0; 16];
    bytes.copy_from_slice(&digest[..16]);
    bytes
}

pub(crate) fn cli_directories() -> Vec<std::path::PathBuf> {
    ["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin", "/bin", "/usr/sbin", "/sbin"]
        .into_iter().map(std::path::PathBuf::from).collect()
}

pub(crate) fn git_executable() -> Option<std::path::PathBuf> {
    use std::{os::unix::fs::PermissionsExt, path::Path, process::{Command,Stdio}, sync::OnceLock};
    static DEVELOPER_TOOLS: OnceLock<bool> = OnceLock::new();
    let search=child_path(Path::new("/usr/bin/git")).ok()?;
    std::env::split_paths(&search).map(|directory|directory.join("git")).find(|path| {
        if !path.metadata().is_ok_and(|metadata|metadata.is_file()&&metadata.permissions().mode()&0o111!=0){return false;}
        // The system Git shim opens the CLT installation dialog when tools are absent.
        if path.canonicalize().ok().as_deref()==Some(Path::new("/usr/bin/git")) {
            return *DEVELOPER_TOOLS.get_or_init(||Command::new("/usr/bin/xcode-select").arg("-p").stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok_and(|status|status.success()));
        }
        true
    })
}

pub(crate) fn child_path(executable: &std::path::Path) -> anyhow::Result<std::ffi::OsString> {
    let mut paths = std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()).collect::<Vec<_>>();
    if let Some(parent) = executable.parent().filter(|p| p.is_absolute()) { paths.push(parent.to_path_buf()); }
    paths.push(crate::core::home().join(".local/bin"));
    paths.extend(cli_directories());
    let mut unique = std::collections::HashSet::new();
    paths.retain(|path| path.is_absolute() && unique.insert(path.clone()));
    Ok(std::env::join_paths(paths)?)
}

pub(crate) fn install_provider(npm: &std::path::Path, package: &str) -> anyhow::Result<()> {
    use anyhow::bail;
    use std::{process::{Command,Stdio},time::{Instant,Duration}};
    let mut command = Command::new(npm);
    command.args(["install","--global","--prefix"]).arg(crate::core::home().join(".local")).arg(package)
        .current_dir(std::env::temp_dir()).env("PATH",child_path(npm)?).env_remove("NODE_OPTIONS")
        .stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    process::configure(&mut command);
    let mut child = command.spawn()?;
    let group = match process::Group::new(child.id(),false) { Ok(group)=>group,Err(error)=>{let _=child.kill();let _=child.wait();return Err(error);} };
    let deadline = Instant::now()+Duration::from_secs(180);
    loop {
        if let Some(status) = child.try_wait()? { if status.success(){return Ok(());} bail!("Kurulum tamamlanamadı ({status})."); }
        if Instant::now()>=deadline { group.stop();let _=child.wait();bail!("Kurulum zaman aşımına uğradı."); }
        std::thread::sleep(Duration::from_millis(50));
    }
}
