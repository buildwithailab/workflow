use super::checkout;
use crate::core::{hash, id, safe_path, string, Core};
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
};

const MAX_BYTES: usize = 5 * 1024 * 1024;
const CONFLICT: &str =
    "Dosya veya seçili Git dalı değişti. Taslağını koruyup dosyayı yeniden açabilirsin.";

fn editable_path(root: &Path, name: &str) -> Result<PathBuf> {
    let allowed = |path: &Path| {
        path.iter().all(|part| {
            let part = part.to_string_lossy();
            !part.eq_ignore_ascii_case(".git")
                && !(cfg!(windows) && (part.contains(':') || part.ends_with(['.', ' '])))
        })
    };
    if name.is_empty() || !allowed(Path::new(name)) {
        bail!("Bu dosya düzenlenemiyor.");
    }
    let path = safe_path(root, name)?.canonicalize()?;
    if !allowed(
        path.strip_prefix(root)
            .context("Dosya çalışma alanı dışında.")?,
    ) {
        bail!("Bu dosya düzenlenemiyor.");
    }
    Ok(path)
}

fn contents(path: &Path) -> Result<(Vec<u8>, fs::Permissions)> {
    let metadata = fs::metadata(path)?;
    if !metadata.is_file() {
        bail!("Yalnızca normal dosyalar düzenlenebilir.");
    }
    if metadata.permissions().readonly() {
        bail!("Dosya salt okunur.");
    }
    let mut bytes = Vec::new();
    File::open(path)?
        .take(MAX_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_BYTES {
        bail!("Dosya düzenleme sınırı 5 MB.");
    }
    Ok((bytes, metadata.permissions()))
}

fn text_format(bytes: &[u8]) -> Result<(&str, bool, bool)> {
    let text =
        std::str::from_utf8(bytes).context("Yalnızca UTF-8 metin dosyaları düzenlenebilir.")?;
    let plain = text.strip_prefix('\u{feff}').unwrap_or(text);
    if plain
        .chars()
        .any(|c| c.is_ascii_control() && !matches!(c, '\t' | '\r' | '\n'))
    {
        bail!("Dosya düzenlenebilir metin içermiyor.");
    }
    let crlf = plain.contains("\r\n");
    let normalized = plain.replace("\r\n", "");
    if normalized.contains('\r') || (crlf && normalized.contains('\n')) {
        bail!("Dosyada birden fazla satır sonu biçimi var.");
    }
    Ok((plain, plain.len() != text.len(), crlf))
}

fn revision(root: &Path, path: &Path, bytes: &[u8]) -> String {
    hash(
        json!([root, path, checkout(root), hash(bytes)])
            .to_string()
            .as_bytes(),
    )
}

pub(super) fn read(core: &Core, args: &Value) -> Result<Value> {
    let root = core.root(args)?.canonicalize()?;
    let path = editable_path(&root, string(args, "path")?)?;
    let (bytes, _) = contents(&path)?;
    let (text, _, _) = text_format(&bytes)?;
    Ok(json!({"text":text.replace("\r\n", "\n"),"revision":revision(&root, &path, &bytes)}))
}

pub(super) fn save(core: &Core, args: &Value) -> Result<Value> {
    let _guard = core.operations.lock().unwrap();
    let root = core.root(args)?.canonicalize()?;
    let name = string(args, "path")?;
    let path = editable_path(&root, name).context(CONFLICT)?;
    let (before, permissions) = contents(&path).context(CONFLICT)?;
    let expected = string(args, "revision")?;
    if revision(&root, &path, &before) != expected {
        bail!(CONFLICT);
    }
    let (_, bom, crlf) = text_format(&before)?;
    let text = string(args, "text")?;
    if text.contains('\r') || text.len() > MAX_BYTES {
        bail!("Metin geçersiz veya 5 MB düzenleme sınırını aşıyor.");
    }
    let mut bytes = Vec::new();
    if bom {
        bytes.extend_from_slice("\u{feff}".as_bytes());
    }
    if crlf {
        bytes.extend_from_slice(text.replace('\n', "\r\n").as_bytes());
    } else {
        bytes.extend_from_slice(text.as_bytes());
    }
    if bytes.len() > MAX_BYTES {
        bail!("Dosya düzenleme sınırı 5 MB.");
    }
    text_format(&bytes)?;
    if bytes != before {
        let temp = path.with_file_name(format!(".workflow-edit-{}.tmp", id()));
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temp)?;
        let result = (|| {
            file.write_all(&bytes)?;
            file.set_permissions(permissions)?;
            file.sync_all()?;
            drop(file);
            let current = editable_path(&root, name).context(CONFLICT)?;
            if current != path
                || revision(&root, &current, &contents(&current).context(CONFLICT)?.0) != expected
            {
                bail!(CONFLICT);
            }
            fs::rename(&temp, &path)
                .context("Dosya kaydedilemedi. Kullanan başka bir uygulama olabilir.")?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temp);
        }
        result?;
    }
    Ok(json!(true))
}
