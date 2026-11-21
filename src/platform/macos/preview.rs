use crate::{core::external_path, terminal};
use anyhow::{bail, Context, Result};
use std::{path::Path, process::{Child, Command, Stdio}};
use super::process::{self, Group};

pub(crate) fn listeners(port: u16) -> Result<Vec<u32>> {
    let output = Command::new("/usr/sbin/lsof").args(["-nP", &format!("-iTCP:{port}"), "-sTCP:LISTEN", "-Fp"])
        .stdin(Stdio::null()).output().context("Port sahipliği okunamadı")?;
    if output.status.code() == Some(1) && output.stdout.is_empty() && output.stderr.is_empty() { return Ok(vec![]); }
    if !output.status.success() { bail!("Port sahipliği doğrulanamadı"); }
    let text = std::str::from_utf8(&output.stdout)?;
    let mut pids = std::collections::BTreeSet::new();
    for line in text.lines() {
        // lsof emits file-descriptor fields even with -Fp. They are not PIDs.
        if let Some(pid) = line.strip_prefix('p') { pids.insert(pid.parse::<u32>()?); }
        else if let Some(fd) = line.strip_prefix('f') { fd.parse::<u32>()?; }
        else { bail!("Port yanıtı geçersiz"); }
    }
    Ok(pids.into_iter().collect())
}

#[cfg(test)]
mod tests {
    #[test]
    fn real_listener_inventory_handles_lsof_descriptor_fields() {
        let listener=std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST,0)).unwrap();
        let pids=super::listeners(listener.local_addr().unwrap().port()).unwrap();
        assert!(pids.contains(&std::process::id()));
    }
}

pub(crate) fn shell(command: &str, directory: &Path, address: &url::Url) -> Result<(Child, Group)> {
    let mut cmd = Command::new("/bin/zsh");
    cmd.args(["-f", "-c", command]).current_dir(external_path(directory))
        .env("PATH", super::child_path(Path::new("/bin/zsh"))?)
        .env("WORKFLOW_PREVIEW_URL", address.as_str())
        .env("WORKFLOW_PREVIEW_PORT", address.port().context("Port gerekli")?.to_string())
        .stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    for key in std::env::vars_os().map(|(key, _)| key) {
        if terminal::internal_variable(&key.to_string_lossy()) { cmd.env_remove(key); }
    }
    process::configure(&mut cmd);
    let mut child = cmd.spawn().context("Önizleme komutu başlatılamadı")?;
    match Group::new(child.id(),false) {
        Ok(group) => Ok((child,group)),
        Err(error) => { let _ = child.kill(); let _ = child.wait(); Err(error) }
    }
}
