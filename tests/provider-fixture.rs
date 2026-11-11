use std::{
    env, fs,
    io::{self, BufRead, Write},
    path::PathBuf,
};
fn main() -> io::Result<()> {
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
