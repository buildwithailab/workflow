use crate::{
    core::{run, string, Core},
    terminal,
};
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

const WINGET: &str = "winget install --exact --source winget --id ";
const WINGET_FLAGS: [&str; 3] = [
    "--disable-interactivity",
    "--accept-source-agreements",
    "--accept-package-agreements",
];

fn npm_available() -> bool {
    cfg!(windows)
        && terminal::executable("npm", None).is_some()
        && terminal::executable("node", None).is_some_and(|exe| {
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
}

fn winget_option(package: &str, available: bool) -> Value {
    json!({"id":"winget","command":format!("{WINGET}{package} {}", WINGET_FLAGS.join(" ")),"available":available})
}

pub fn options() -> Value {
    let winget = cfg!(windows) && terminal::executable("winget", None).is_some();
    let npm = npm_available();
    json!({
        "codex": [
            {"id":"npm","command":"npm.cmd install -g @openai/codex","available":npm},
            winget_option("OpenAI.Codex", winget)
        ],
        "claude":[winget_option("Anthropic.ClaudeCode", winget)],
        "copilot":[winget_option("GitHub.Copilot", winget)]
    })
}

pub fn install(core: &Core, args: &Value) -> Result<Value> {
    let provider = string(args, "provider")?;
    let method = string(args, "method")?;
    if !cfg!(windows) || args["confirmed"] != true {
        bail!("Kurulum komutunu önce onayla.");
    }
    let _guard = core.operations.lock().unwrap();
    let output = match (provider, method) {
        ("codex", "npm") => {
            if !npm_available() {
                bail!("npm ve Node.js 22+ gerekli.");
            }
            let npm = terminal::executable("npm", None).context("npm bulunamadı.")?;
            let command = format!(
                "& '{}' install -g @openai/codex; exit $LASTEXITCODE",
                npm.to_string_lossy().replace('\'', "''")
            );
            run(
                &core.directory,
                "powershell.exe",
                &[
                    "-NoProfile".into(),
                    "-NonInteractive".into(),
                    "-Command".into(),
                    command,
                ],
            )?
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
