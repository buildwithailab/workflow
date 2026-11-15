use crate::{
    accounts::Profile,
    core::{atomic, external_path, hash, safe_path, string, Core},
    terminal::{codex_config_string, uses_windows_script},
};
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::{
    ffi::OsString,
    fs,
    io::Read,
    path::{Path, PathBuf},
};

const BEGIN: &str = "<!-- workflow:begin -->";
const END: &str = "<!-- workflow:end -->";
const CHECKSUM: &str = "<!-- workflow:sha256 ";
const CLAUDE_RULE: &str = ".claude/rules/workflow-project.md";
const COPILOT_RULE: &str = ".github/copilot-instructions.md";
const MAX_SHARED: usize = 16 * 1024;
const MAX_PROVIDER_FILE: usize = 1024 * 1024;
pub(crate) const COPILOT_DIRECTORIES: &str = "COPILOT_CUSTOM_INSTRUCTIONS_DIRS";

fn read_text(path: &Path, limit: usize) -> Result<Option<String>> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if !metadata.is_file() || crate::core::is_link(&metadata) {
        bail!("Talimat dosyası normal bir dosya olmalı.");
    }
    let mut bytes = Vec::new();
    fs::File::open(path)?
        .take((limit + 1) as u64)
        .read_to_end(&mut bytes)?;
    if bytes.len() > limit {
        bail!("Talimat dosyası boyut sınırını aşıyor.");
    }
    let text = String::from_utf8(bytes).context("Talimat dosyası UTF-8 olmalı.")?;
    if text.contains('\0') {
        bail!("Talimat dosyası UTF-8 olmalı.");
    }
    Ok(Some(text))
}

fn project_file(core: &Core, project_id: &str) -> Result<PathBuf> {
    let project = core.project(project_id)?;
    let key = uuid::Uuid::parse_str(string(&project, "id")?)?;
    safe_path(&core.directory, &format!("project-instructions/{key}.md"))
}

pub fn edit(core: &Core, action: &str, args: &Value) -> Result<Value> {
    let _guard = core.operations.lock().unwrap();
    let path = project_file(core, string(args, "projectId")?)?;
    let current = read_text(&path, MAX_SHARED)?.unwrap_or_default();
    let revision = hash(current.as_bytes());
    if action == "project_instructions_read" {
        return Ok(json!({"text":current, "revision":revision, "maxBytes":MAX_SHARED}));
    }
    if args["revision"] != revision {
        bail!("Proje talimatları değişti. Editörü yeniden aç.");
    }
    let text = string(args, "text")?;
    if text.len() > MAX_SHARED {
        bail!("Ortak proje talimatları en fazla 16 KB olabilir.");
    }
    if text
        .chars()
        .any(|c| c.is_control() && !matches!(c, '\r' | '\n' | '\t'))
    {
        bail!("Ortak proje talimatları geçersiz bir karakter içeriyor.");
    }
    if text != current {
        atomic(&path, text.as_bytes())?;
    }
    Ok(json!(true))
}

fn block_range(text: &str) -> Result<Option<std::ops::Range<usize>>> {
    let (mut begin, mut end) = (None, None);
    let mut offset = 0;
    for line in text.split_inclusive('\n') {
        let marker = line.trim_end_matches(['\r', '\n']);
        match marker {
            BEGIN if begin.is_none() && end.is_none() => begin = Some(offset),
            END if begin.is_some() && end.is_none() => end = Some(offset + line.len()),
            BEGIN | END => bail!("Talimat dosyasındaki Workflow işaretleri eşleşmiyor."),
            line if line.starts_with(CHECKSUM) && (begin.is_none() || end.is_some()) => {
                bail!("Talimat dosyasındaki Workflow işaretleri eşleşmiyor.");
            }
            _ => {}
        }
        offset += line.len();
    }
    match (begin, end) {
        (Some(begin), Some(end)) => Ok(Some(begin..end)),
        (None, None) => Ok(None),
        _ => bail!("Talimat dosyasındaki Workflow işaretleri eşleşmiyor."),
    }
}

fn block_digest(body: &str) -> String {
    hash(body.replace("\r\n", "\n").as_bytes())
}

fn without_legacy_block(original: &str) -> Result<Option<String>> {
    let Some(range) = block_range(original)? else {
        return Ok(Some(original.to_owned()));
    };
    let block = &original[range.clone()];
    let prefix_digest = (|| {
        let (_, rest) = block.split_once('\n')?;
        let (stamp, rest) = rest.split_once('\n')?;
        let (checksum, prefix_digest) = stamp
            .trim_end_matches('\r')
            .strip_prefix(CHECKSUM)?
            .strip_suffix(" -->")?
            .split_once(' ')?;
        let (body, _) = rest.rsplit_once(END)?;
        (checksum == block_digest(body)).then_some(prefix_digest)
    })()
    .context("Workflow talimat bölümü elle değişmiş. Dosyayı kontrol et.")?;
    if range.start == 0 && range.end == original.len() {
        return Ok(None);
    }
    let prefix = &original[..range.start];
    let suffix = &original[range.end..];
    if !suffix.is_empty() || prefix_digest != block_digest(prefix) {
        return Ok(Some(format!("{prefix}{suffix}")));
    }
    Ok(Some(
        prefix
            .strip_suffix("\r\n")
            .or_else(|| prefix.strip_suffix('\n'))
            .unwrap_or(prefix)
            .to_owned(),
    ))
}

fn remove_legacy_blocks(root: &Path) -> Result<()> {
    let mut pending = Vec::new();
    for name in ["AGENTS.md", "AGENTS.override.md", CLAUDE_RULE, COPILOT_RULE] {
        let Ok(path) = safe_path(root, name) else {
            continue;
        };
        let Ok(Some(text)) = read_text(&path, MAX_PROVIDER_FILE) else {
            continue;
        };
        let next = without_legacy_block(&text)?;
        let old = Some(text);
        if old != next {
            pending.push((name, path, old, next));
        }
    }
    for (name, path, old, next) in pending {
        if safe_path(root, name)? != path || read_text(&path, MAX_PROVIDER_FILE)? != old {
            bail!("Talimat dosyası değişti. Oturumu yeniden başlatmayı dene.");
        }
        match next {
            Some(next) => atomic(&path, next.as_bytes())?,
            None => fs::remove_file(path)?,
        }
    }
    Ok(())
}

#[derive(Default)]
pub(crate) struct LaunchInstructions {
    pub arguments: Vec<String>,
    pub copilot_directories: Option<OsString>,
}

fn session_file(core: &Core, session: &Value, name: &str, text: &str) -> Result<PathBuf> {
    let id = uuid::Uuid::parse_str(string(session, "id")?)?;
    let path = safe_path(
        &core.directory,
        &format!("workspaces/{id}/instructions/{name}"),
    )?;
    if crate::core::file_hash(&path)?.as_deref() != Some(hash(text.as_bytes()).as_str()) {
        atomic(&path, text.as_bytes())?;
    }
    Ok(path)
}

pub(crate) fn prepare(
    core: &Core,
    session: &Value,
    root: &Path,
    exe: &Path,
    profile: &Profile,
) -> Result<LaunchInstructions> {
    let mut launch = LaunchInstructions::default();
    if session["login"] == true {
        return Ok(launch);
    }
    let source = project_file(core, string(session, "projectId")?)?;
    let Some(shared) = read_text(&source, MAX_SHARED)? else {
        return Ok(launch);
    };
    remove_legacy_blocks(root)?;
    let provider = string(session, "provider")?;
    if provider == "claude"
        && crate::usage::claude_supports_prompt_snapshot(exe, profile)
            .context("Claude Code sürümü okunamadı.")?
    {
        // Rebuild the prompt when continuing a conversation with edited or cleared rules.
        launch.arguments.extend(["--system-prompt-snapshot".into(), "off".into()]);
    }
    if shared.trim().is_empty() {
        return Ok(launch);
    }
    match provider {
        "codex" => {
            let configured = crate::usage::codex_instructions(core, root, profile)
                .context("Codex talimat ayarları okunamadı.")?;
            let content = format!("{configured}\n\nProject instructions from Workflow:\n{shared}");
            let script = uses_windows_script(exe);
            let mut argument = codex_config_string(&content, script);
            // Windows batch shims have a lower limit than CreateProcess.
            let limit = if script { 3000 } else { 16000 };
            if argument.encode_utf16().count() > limit {
                let path = session_file(core, session, "project.md", &content)?;
                let reference = format!("Read the complete local instructions file at {:?} before doing any project work. Follow the instructions in that file.", external_path(&path));
                argument = codex_config_string(&reference, script);
            }
            launch.arguments = vec!["-c".into(), format!("developer_instructions={argument}")];
        }
        "claude" => {
            let path = session_file(core, session, "project.md", &shared)?;
            launch.arguments.extend([
                "--append-system-prompt-file".into(),
                external_path(&path).to_string_lossy().into_owned(),
            ]);
        }
        "copilot" => {
            let path = session_file(core, session, "AGENTS.md", &shared)?;
            let directory = external_path(path.parent().unwrap());
            if directory.to_string_lossy().contains(',') {
                bail!("Copilot talimat klasörü yolunda virgül kullanılamıyor.");
            }
            let mut directories = std::env::var_os(COPILOT_DIRECTORIES).unwrap_or_default();
            if !directories.is_empty() {
                directories.push(",");
            }
            directories.push(directory);
            launch.copilot_directories = Some(directories);
        }
        _ => bail!("Sağlayıcı geçersiz"),
    }
    Ok(launch)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn legacy_block(manual: Option<&str>, contents: &str) -> String {
        let newline = if manual.is_some_and(|text| text.contains("\r\n")) {
            "\r\n"
        } else {
            "\n"
        };
        let prefix = manual.map_or_else(String::new, |text| format!("{text}{newline}"));
        let body = format!("{}{newline}", contents.replace('\n', newline));
        let body_digest = block_digest(&body);
        let prefix_digest = block_digest(&prefix);
        format!("{prefix}{BEGIN}{newline}{CHECKSUM}{body_digest} {prefix_digest} -->{newline}{body}{END}{newline}")
    }

    #[test]
    fn legacy_cleanup_preserves_manual_bytes_and_line_endings() {
        for manual in ["", "manual", "manual\n", "\u{feff}manual\r\n\r\n"] {
            let legacy = legacy_block(Some(manual), "First\nSecond");
            assert_eq!(
                without_legacy_block(&legacy).unwrap().as_deref(),
                Some(manual)
            );
            let with_suffix = format!("{legacy}Manual suffix");
            let newline = if manual.contains("\r\n") {
                "\r\n"
            } else {
                "\n"
            };
            assert_eq!(
                without_legacy_block(&with_suffix).unwrap().unwrap(),
                format!("{manual}{newline}Manual suffix")
            );
        }
    }

    #[test]
    fn removes_only_app_created_file_and_rejects_malformed_blocks() {
        let generated = legacy_block(None, "Rules");
        assert_eq!(without_legacy_block(&generated).unwrap(), None);
        assert!(without_legacy_block(BEGIN).is_err());
        assert!(without_legacy_block(&format!("{generated}{generated}")).is_err());
        assert!(without_legacy_block(&generated.replace("Rules", "Manual edit")).is_err());
        let with_manual = legacy_block(Some("manual\n"), "Rules");
        let modified_prefix = with_manual.replacen("manual\n\n", "manual\n", 1);
        assert_eq!(
            without_legacy_block(&modified_prefix).unwrap().as_deref(),
            Some("manual\n")
        );
        let manual = "A manually written instruction";
        assert_eq!(
            without_legacy_block(manual).unwrap().as_deref(),
            Some(manual)
        );
    }
}
