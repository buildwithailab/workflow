use std::{
    env, fs,
    io::{self, BufRead, Write},
    path::PathBuf,
};
fn main() -> io::Result<()> {
    if env::args().nth(1).as_deref() == Some("--version") {
        println!("2.1.265 (Claude Code)");
        return Ok(());
    }
    let login = env::args().nth(1).as_deref() == Some("--login-fixture");
    let provider = env::args().nth(2).unwrap_or_else(|| "codex".into());
    let variable = match provider.as_str() {
        "codex" => "CODEX_HOME",
        "claude" => "CLAUDE_CONFIG_DIR",
        _ => "COPILOT_HOME",
    };
    let profile = env::var_os(variable).map(PathBuf::from).filter(|p| {
        p.starts_with(PathBuf::from(env::var_os("WORKFLOW_DATA_DIR").unwrap()).join("accounts"))
    });
    let mut updating =
        provider == "codex" && env::current_dir()?.join("model-startup-fixture").is_file();
    println!(
        "Workflow terminal test · {} · PID {}",
        provider,
        std::process::id()
    );
    if let Some(profile) = &profile {
        println!(
            "Account: {}",
            profile.file_name().unwrap().to_string_lossy()
        );
    }
    if !login {
        let instructions = match provider.as_str() {
            "claude" => {
                let snapshot = env::args()
                    .skip_while(|arg| arg != "--system-prompt-snapshot")
                    .nth(1);
                if snapshot.as_deref() == Some("off") {
                    println!("Claude reloads the current instructions.");
                    env::args()
                        .skip_while(|arg| arg != "--append-system-prompt-file")
                        .nth(1)
                        .and_then(|path| fs::read_to_string(path).ok())
                } else {
                    None
                }
            }
            "copilot" => env::var("COPILOT_CUSTOM_INSTRUCTIONS_DIRS")
                .ok()
                .and_then(|paths| {
                    let directory = PathBuf::from(paths.rsplit(',').next()?);
                    fs::read_to_string(directory.join("AGENTS.md")).ok()
                }),
            _ => env::args().find_map(|arg| {
                arg.strip_prefix("developer_instructions=")
                    .map(str::to_owned)
            }),
        };
        if let Some(instructions) = instructions {
            if instructions.contains("Run the project checks.")
                || instructions.contains("Verify changes before committing.")
            {
                println!("Workflow shared instructions are present at process startup.");
            }
            if instructions.contains("Run the updated project checks.") {
                println!("Updated Workflow instructions are present at process startup.");
            }
            if provider == "codex" && instructions.contains("Existing Codex test instruction.") {
                println!("Codex custom instructions were preserved.");
            }
        }
    }
    if updating {
        print!("\r\nCheck execution permissions\r\nUpdate available\r\n1. Update now\r\n2. Skip\r\n3. Skip until next version\r\nenter continue · esc skip");
    } else {
        print!("\r\nReady > ");
    }
    io::stdout().flush()?;
    for line in io::stdin().lock().lines() {
        let line = line?;
        if login {
            if line.trim() == "connect" {
                if let Some(profile) = &profile {
                    let id = profile.file_name().unwrap().to_string_lossy();
                    fs::write(
                        profile.join("auth-status.json"),
                        format!(
                            r#"{{"login":"{id}","email":"{id}@localhost","loggedIn":true,"authMethod":"claude.ai","orgId":"fixture"}}"#
                        ),
                    )?;
                    if provider == "claude" {
                        let token = env::var("WORKFLOW_TEST_MANAGED_CREDENTIAL")
                            .expect("Missing generated test credential");
                        fs::write(
                            profile.join(".credentials.json"),
                            format!(
                                r#"{{"claudeAiOauth":{{"accessToken":"{token}","expiresAt":2200000000000}}}}"#
                            ),
                        )?;
                    }
                    break;
                }
            }
            if line.trim() == "connect" && provider == "claude" {
                let folder = PathBuf::from(env::var_os("WORKFLOW_TEST_USAGE_DIR").unwrap());
                fs::write(
                    folder.join("auth-status.json"),
                    br#"{"loggedIn":true,"authMethod":"claude.ai"}"#,
                )?;
                break;
            }
            continue;
        }
        if let Some(folder) = env::var_os("WORKFLOW_TEST_INPUT_DIR") {
            let folder = PathBuf::from(folder);
            fs::create_dir_all(&folder)?;
            let mut file = fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(folder.join(format!("{provider}.txt")))?;
            writeln!(file, "{line}")?;
        }
        if line.trim() == "exit" {
            break;
        }
        if line.trim() == "write-workspace-marker" {
            fs::write("direct-session-marker.txt", "direct workspace\n")?;
        }
        if updating && line.trim().trim_start_matches('\x1b') == "skip-update" {
            updating = false;
            print!(
                "\x1b[2J\x1b[H/permissions - choose permissions\r\nAsk Codex to do anything\r\n"
            );
            io::stdout().flush()?;
            continue;
        }
        println!("\r\nReceived: {line}\r\nReady > ");
        io::stdout().flush()?;
    }
    Ok(())
}
