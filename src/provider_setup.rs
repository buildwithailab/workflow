use crate::{
    core::{run, string, Core},
    terminal,
};
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::{
    path::PathBuf,
};
#[cfg(not(target_os = "macos"))]
use std::{path::Path, process::Command};

#[cfg(not(target_os = "macos"))]
const WINGET: &str = "winget install --exact --source winget --id ";
#[cfg(not(target_os = "macos"))]
const WINGET_FLAGS: [&str; 3] = [
    "--disable-interactivity",
    "--accept-source-agreements",
    "--accept-package-agreements",
];

fn npm_for_install() -> Option<PathBuf> {
    if !cfg!(any(windows, target_os = "macos")) {
        return None;
    }
    let npm = terminal::executable("npm", None).filter(|path| path.is_absolute())?;
    terminal::executable("node", None)
        .is_some_and(|exe| {
            run(
                &std::env::temp_dir(),
                &exe.to_string_lossy(),
                &["--version".into()],
            )
            .ok()
            .filter(|out| out.status.success())
            .and_then(|out| {
                String::from_utf8_lossy(&out.stdout)
                    .trim()
                    .trim_start_matches('v')
                    .split('.')
                    .next()?
                    .parse::<u32>()
                    .ok()
            })
            .is_some_and(|major| major >= 22)
        })
        .then_some(npm)
}

#[cfg(not(target_os = "macos"))]
fn npm_command(npm: &Path, arguments: &str) -> Option<Command> {
    let powershell = PathBuf::from(std::env::var_os("SystemRoot")?)
        .join("System32/WindowsPowerShell/v1.0/powershell.exe");
    let mut command = Command::new(powershell);
    command
        .env("WORKFLOW_NPM_PROGRAM", npm)
        .args(["-NoProfile", "-NonInteractive", "-Command"])
        .arg(format!(
            "$ErrorActionPreference = 'Stop'; [Console]::OutputEncoding = [Text.UTF8Encoding]::new($false); & $env:WORKFLOW_NPM_PROGRAM {arguments}; exit $LASTEXITCODE"
        ));
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000);
    }
    Some(command)
}

#[cfg(windows)]
pub(crate) fn npm_prefix() -> Option<PathBuf> {
    use std::{
        io::Read,
        process::Stdio,
        sync::mpsc,
        thread,
        time::{Duration, Instant},
    };

    let npm = terminal::executable("npm", None).filter(|path| path.is_absolute())?;
    let mut child = npm_command(&npm, "prefix -g")?
        .current_dir(std::env::temp_dir())
        .env_remove("NODE_OPTIONS")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let _job = terminal::Job::new(child.id()).ok();
    let stdout = child.stdout.take()?;
    let (send, receive) = mpsc::channel();
    const MAX_PREFIX_BYTES: usize = 32 * 1024;
    thread::spawn(move || {
        let mut bytes = Vec::new();
        let result = stdout
            .take(MAX_PREFIX_BYTES as u64 + 1)
            .read_to_end(&mut bytes)
            .map(|_| bytes);
        let _ = send.send(result);
    });
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => break,
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(20)),
            Ok(Some(_)) => return None,
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
    let bytes = receive
        .recv_timeout(deadline.saturating_duration_since(Instant::now()))
        .ok()?
        .ok()?;
    if bytes.len() > MAX_PREFIX_BYTES {
        return None;
    }
    let mut lines = std::str::from_utf8(&bytes).ok()?.trim().lines();
    let prefix = PathBuf::from(lines.next()?);
    (lines.next().is_none() && prefix.is_absolute()).then_some(prefix)
}

#[cfg(not(target_os = "macos"))]
fn winget_option(package: &str, available: bool) -> Value {
    json!({"id":"winget","command":format!("{WINGET}{package} {}", WINGET_FLAGS.join(" ")),"available":available})
}

pub fn options() -> Value {
    #[cfg(not(target_os = "macos"))]
    let winget = cfg!(windows) && terminal::executable("winget", None).is_some();
    let npm = npm_for_install().is_some();
    #[cfg(target_os = "macos")]
    return json!({
        "codex":[{"id":"npm","command":"npm install --global --prefix ~/.local @openai/codex","available":npm}],
        "claude":[{"id":"npm","command":"npm install --global --prefix ~/.local @anthropic-ai/claude-code","available":npm}],
        "copilot":[{"id":"npm","command":"npm install --global --prefix ~/.local @github/copilot","available":npm}]
    });
    #[cfg(not(target_os = "macos"))]
    json!({
        "codex": [
            {"id":"npm","command":"npm.cmd install -g @openai/codex","available":npm},
            winget_option("OpenAI.Codex", winget)
        ],
        "claude":[winget_option("Anthropic.ClaudeCode", winget)],
        "copilot":[
            {"id":"npm","command":"npm.cmd install -g @github/copilot","available":npm},
            winget_option("GitHub.Copilot", winget)
        ]
    })
}

pub fn install(core: &Core, args: &Value) -> Result<Value> {
    let provider = string(args, "provider")?;
    let method = string(args, "method")?;
    if !cfg!(any(windows, target_os = "macos")) || args["confirmed"] != true {
        bail!("Kurulum komutunu önce onayla.");
    }
    #[cfg(target_os = "macos")]
    {
        if method != "npm" { bail!("Kurulum yöntemi geçersiz."); }
        let package = match provider {
            "codex" => "@openai/codex", "claude" => "@anthropic-ai/claude-code", "copilot" => "@github/copilot",
            _ => bail!("Sağlayıcı desteklenmiyor."),
        };
        let _guard = core.operations.lock().unwrap();
        crate::platform::macos::install_provider(&npm_for_install().context("npm ve Node.js 22+ gerekli.")?,package)?;
        return Ok(json!(true));
    }
    #[cfg(not(target_os = "macos"))]
    {
    let _guard = core.operations.lock().unwrap();
    let output = match (provider, method) {
        ("codex" | "copilot", "npm") => {
            let npm = npm_for_install().context("npm ve Node.js 22+ gerekli.")?;
            let arguments = if provider == "codex" {
                "install -g '@openai/codex'"
            } else {
                "install -g '@github/copilot'"
            };
            npm_command(&npm, arguments)
                .context("PowerShell bulunamadı.")?
                .current_dir(crate::core::external_path(&core.directory))
                .output()
                .context("npm çalıştırılamadı.")?
        }
        ("codex" | "claude" | "copilot", "winget") => {
            let exe = terminal::executable("winget", None).context("WinGet bulunamadı.")?;
            let package = match provider {
                "codex" => "OpenAI.Codex",
                "claude" => "Anthropic.ClaudeCode",
                _ => "GitHub.Copilot",
            };
            let arguments: Vec<_> = ["install", "--exact", "--source", "winget", "--id", package]
                .into_iter()
                .chain(WINGET_FLAGS)
                .map(str::to_owned)
                .collect();
            run(&core.directory, &exe.to_string_lossy(), &arguments)?
        }
        _ => bail!("Kurulum yöntemi geçersiz."),
    };
    if !output.status.success() {
        bail!("Kurulum tamamlanamadı ({0}).", output.status);
    }
    Ok(json!(true))
    }
}
