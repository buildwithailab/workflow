use crate::core::{is_link, safe_path};
use anyhow::{bail, Context, Result};
use ignore::gitignore::GitignoreBuilder;
use regex::bytes::RegexSet;
use serde::Serialize;
use std::{
    collections::VecDeque,
    ffi::OsStr,
    fs,
    io::{self, Read},
    path::{Path, PathBuf},
    sync::OnceLock,
};

const FILE_LIMIT: u64 = 5 * 1024 * 1024;
const IGNORE_LIMIT: u64 = 64 * 1024;
const SCAN_BYTES: u64 = 16 * 1024;
const SCAN_LIMIT: usize = 2_000;
const RESULTS_LIMIT: usize = 30;
pub(crate) const BATCH_LIMIT: usize = 20 * 1024 * 1024;

fn sensitive_name(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(OsStr::to_str) else {
        return true;
    };
    let name = name.to_ascii_lowercase();
    let dirs = [
        ".aws",
        ".azure",
        ".ssh",
        ".kube",
        ".docker",
        ".codex",
        "secrets",
        ".secrets",
        "accounts",
        "transcripts",
        "leases",
        "browser-profiles",
        "ui-profile",
        ".workflow",
        ".claude",
        ".copilot",
        "sessions",
        "profiles",
    ];
    path.ancestors()
        .skip(1)
        .filter_map(|p| p.file_name().and_then(OsStr::to_str))
        .any(|part| dirs.iter().any(|dir| part.eq_ignore_ascii_case(dir)))
        || name.starts_with(".env")
        || name.ends_with(".env")
        || name.starts_with("appsettings")
        || name.starts_with("id_rsa")
        || name.starts_with("id_ed25519")
        || name.starts_with("credentials.")
        || name.starts_with("secrets.")
        || name.starts_with("auth-")
        || matches!(
            name.as_str(),
            "web.config"
                | "auth.json"
                | ".credentials.json"
                | ".npmrc"
                | ".netrc"
                | "cookies"
                | "login data"
                | "local state"
                | "workspace.json"
        )
        || (name.contains(".php") && (name.contains("conn") || name.contains("config")))
        || name.contains(".php_")
        || [
            ".pem", ".key", ".pfx", ".p12", ".kdbx", ".bak", ".ansi", "~",
        ]
        .iter()
        .any(|ext| name.ends_with(ext))
}

pub(crate) fn contains_credentials(bytes: &[u8]) -> bool {
    static PATTERNS: OnceLock<RegexSet> = OnceLock::new();
    let patterns = PATTERNS.get_or_init(|| {
        RegexSet::new([
            r"-----BEGIN (?:[A-Z]+ )?PRIVATE KEY-----",
            r"\b(?:AKIA|ASIA)[A-Z0-9]{16}\b",
            r"\b(?:gh[pousr]_[A-Za-z0-9]{20,}|github_pat_[A-Za-z0-9_]{30,}|sk-(?:proj-)?[A-Za-z0-9_-]{20,}|xox[baprs]-[A-Za-z0-9-]{20,}|eyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,})\b",
            r#"(?i)\b(?:postgres(?:ql)?|mysql|mongodb(?:\+srv)?|redis|https?)://[^\s/:"'<>]+:[^\s/@"'<>]{3,}@"#,
            r#"(?im)(?:^|[^\w])[$"']?[A-Z0-9_-]*(?:password|passwd|pwd|api[_-]?key|access[_-]?token|refresh[_-]?token|client[_-]?secret|secret[_-]?key|connection[_-]?string)["']?\s*[:=]\s*(?:"[^"\r\n]{3,}"|'[^'\r\n]{3,}'|[A-Za-z0-9_+/.@-]{8,}(?:\s*(?:[;\r\n,#}]|$)))"#,
        ])
        .expect("valid credential patterns")
    });
    if bytes.starts_with(&[0xff, 0xfe]) || bytes.starts_with(&[0xfe, 0xff]) {
        let le = bytes[0] == 0xff;
        let text = String::from_utf16_lossy(
            &bytes[2..]
                .chunks_exact(2)
                .map(|b| {
                    if le {
                        u16::from_le_bytes([b[0], b[1]])
                    } else {
                        u16::from_be_bytes([b[0], b[1]])
                    }
                })
                .collect::<Vec<_>>(),
        );
        return patterns.is_match(text.as_bytes());
    }
    patterns.is_match(bytes)
}

/// Check rules only along a requested path. A nested rule cannot reopen an
/// ignored ancestor unless the rules outside that ancestor already reopened it.
fn explicitly_ignored(root: &Path, relative: &Path) -> Result<bool> {
    let mut directories = Vec::new();
    let mut current = PathBuf::new();
    let mut components = relative.components().peekable();
    while let Some(component) = components.next() {
        let full = root.join(&current);
        let mut builder = GitignoreBuilder::new(&full);
        builder.case_insensitive(cfg!(windows))?;
        for name in [".agentloomignore", ".workflowignore"] {
            let path = full.join(name);
            match fs::symlink_metadata(&path) {
                Ok(m) if !is_link(&m) && m.is_file() && m.len() <= IGNORE_LIMIT => {
                    let mut text = String::new();
                    fs::File::open(path)?
                        .take(IGNORE_LIMIT + 1)
                        .read_to_string(&mut text)?;
                    if text.len() as u64 > IGNORE_LIMIT {
                        bail!("Dosya paylaşım kuralları okunamadı")
                    }
                    for line in text.trim_start_matches('\u{feff}').lines() {
                        builder.add_line(None, line)?;
                    }
                }
                Ok(_) => bail!("Dosya paylaşım kuralları okunamadı"),
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
        }
        directories.push(builder.build()?);
        current.push(component);
        let candidate = root.join(&current);
        if directories
            .iter()
            .rev()
            .map(|rules| rules.matched(&candidate, components.peek().is_some()))
            .find(|m| !m.is_none())
            .is_some_and(|m| m.is_ignore())
        {
            return Ok(true);
        }
    }
    Ok(false)
}

fn read_limited(path: &Path, limit: u64) -> Result<Vec<u8>> {
    if !fs::metadata(path)?.is_file() {
        bail!("Yalnızca normal dosyalar paylaşılabilir")
    }
    let mut bytes = Vec::new();
    fs::File::open(path)?.take(limit).read_to_end(&mut bytes)?;
    Ok(bytes)
}

fn path_blocked(root: &Path, relative: &Path) -> Result<bool> {
    Ok(sensitive_name(relative) || explicitly_ignored(root, relative)?)
}

pub(crate) fn project_path_blocked(root: &Path, project: &Path, relative: &Path) -> Result<bool> {
    let check = || {
        if path_blocked(root, relative)? {
            return Ok::<_, anyhow::Error>(true);
        }
        if root == project {
            return Ok(false);
        }
        // Stop if a project component is a link; never read its ignore files from outside the project.
        safe_path(project, &relative.to_string_lossy())?;
        path_blocked(project, relative)
    };
    check().context("Dosya paylaşım kuralları okunamadı")
}

pub(crate) fn read_shared(root: &Path, project: &Path, relative: &str) -> Result<Vec<u8>> {
    let root = root.canonicalize()?;
    let path = safe_path(&root, relative)?;
    let relative = path.strip_prefix(&root)?;
    if project_path_blocked(&root, project, relative)? {
        bail!("Dosya paylaşım kurallarına göre engellendi")
    }
    let bytes = read_limited(&path, FILE_LIMIT + 1)?;
    if bytes.len() as u64 > FILE_LIMIT {
        bail!("Önizleme sınırı 5 MB")
    }
    if contains_credentials(&bytes) {
        bail!("Dosya olası kimlik bilgileri içerdiği için paylaşılmadı")
    }
    Ok(bytes)
}

pub(crate) fn safe_baseline(core: &crate::core::Core, digest: &str) -> Result<bool> {
    if digest.len() != 64 || !digest.bytes().all(|b| b.is_ascii_hexdigit()) {
        bail!("Yedek kimliği geçersiz")
    }
    let path = safe_path(&core.directory, &format!("objects/{digest}"))?;
    let bytes = read_limited(&path, FILE_LIMIT + 1)?;
    Ok(bytes.len() as u64 <= FILE_LIMIT
        && crate::core::hash(&bytes) == digest
        && !contains_credentials(&bytes))
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct WarningScan {
    files: Vec<String>,
    truncated: bool,
}

pub(crate) fn warnings(root: &Path) -> Result<WarningScan> {
    let root = root.canonicalize()?;
    let mut remaining = VecDeque::from([root.clone()]);
    let mut files = Vec::new();
    let mut count = 0;
    let mut truncated = false;
    'scan: while let Some(dir) = remaining.pop_front() {
        let entries = match fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(_) => {
                truncated = true;
                continue;
            }
        };
        for entry in entries {
            if count >= SCAN_LIMIT || files.len() >= RESULTS_LIMIT {
                truncated = true;
                break 'scan;
            }
            count += 1;
            let Ok(entry) = entry else {
                truncated = true;
                continue;
            };
            let entry_path = entry.path();
            let relative = entry_path.strip_prefix(&root)?;
            let Ok(kind) = entry.file_type() else {
                truncated = true;
                continue;
            };
            if kind.is_dir() {
                let name = entry.file_name().to_string_lossy().to_ascii_lowercase();
                if matches!(
                    name.as_str(),
                    ".git"
                        | "node_modules"
                        | "target"
                        | "bin"
                        | "obj"
                        | "dist"
                        | ".workflow"
                        | ".agentloom"
                ) {
                    continue;
                }
                if let Some(relative) = relative.to_str() {
                    if let Ok(path) = safe_path(&root, relative) {
                        remaining.push_back(path);
                    }
                }
            } else if kind.is_file() {
                let Some(name) = relative.to_str() else {
                    truncated = true;
                    continue;
                };
                if sensitive_name(relative) {
                    files.push(name.replace('\\', "/"));
                } else if let Ok(path) = safe_path(&root, name) {
                    match read_limited(&path, SCAN_BYTES + 1) {
                        Ok(mut bytes) => {
                            if bytes.len() as u64 > SCAN_BYTES {
                                bytes.truncate(SCAN_BYTES as usize);
                                truncated = true;
                            }
                            if contains_credentials(&bytes) {
                                files.push(name.replace('\\', "/"));
                            }
                        }
                        Err(_) => truncated = true,
                    }
                }
            }
        }
    }
    files.sort();
    Ok(WarningScan { files, truncated })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with_project(test: impl FnOnce(&Path)) {
        let root = std::env::temp_dir().join(crate::core::id());
        fs::create_dir(&root).unwrap();
        test(&root.canonicalize().unwrap());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn sharing_honors_root_rules_nested_rules_and_denied_ancestors() {
        with_project(|root| {
            fs::create_dir_all(root.join("nested/hidden")).unwrap();
            fs::write(
                root.join(".workflowignore"),
                "*.internal\n/nested/hidden/\n",
            )
            .unwrap();
            fs::write(root.join("nested/.workflowignore"), "!allowed.internal\n").unwrap();
            fs::write(root.join("nested/hidden/.workflowignore"), "!private.txt\n").unwrap();
            fs::write(root.join("nested/allowed.internal"), "ordinary fixture").unwrap();
            fs::write(root.join("nested/denied.internal"), "ordinary fixture").unwrap();
            fs::write(root.join("nested/hidden/private.txt"), "ordinary fixture").unwrap();
            assert!(read_shared(root, root, "nested/allowed.internal").is_ok());
            assert!(read_shared(root, root, "nested/denied.internal").is_err());
            assert!(read_shared(root, root, "nested/hidden/private.txt").is_err());
        });
    }

    #[test]
    fn content_is_checked_beyond_the_warning_scan_prefix() {
        with_project(|root| {
            let token = crate::core::id();
            fs::write(root.join("conn.php___"), "backup fixture").unwrap();
            let mut bytes = vec![b' '; SCAN_BYTES as usize + 1];
            bytes.extend_from_slice(format!("\n$password = '{token}';\n").as_bytes());
            fs::write(root.join("plain.txt"), bytes).unwrap();
            let warnings = warnings(root).unwrap();
            assert_eq!(warnings.files, ["conn.php___"]);
            assert!(warnings.truncated);
            assert!(read_shared(root, root, "conn.php___").is_err());
            let error = read_shared(root, root, "plain.txt")
                .unwrap_err()
                .to_string();
            assert!(error.contains("kimlik bilgileri"));
            assert!(!error.contains(&token));
        });
    }

    #[test]
    fn detects_prefixed_credentials_in_regular_files() {
        with_project(|root| {
            let value = crate::core::id();
            for field in ["DB_PASSWORD", "dbPassword", "service-passwd", "DB_PWD"] {
                fs::write(root.join("settings.txt"), format!("{field} = '{value}'\n")).unwrap();
                assert!(read_shared(root, root, "settings.txt").is_err(), "{field}");
            }
        });
    }
}
