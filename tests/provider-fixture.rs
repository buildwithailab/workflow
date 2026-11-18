use std::{
    env, fs,
    io::{self, BufRead, Read, Write},
    path::PathBuf,
};
fn main() -> io::Result<()> {
    if env::args().nth(1).as_deref() == Some("--version") {
        println!("2.1.265 (Claude Code)");
        return Ok(());
    }
    let login = env::args().nth(1).as_deref() == Some("--login-fixture");
    let provider = env::args().nth(2).unwrap_or_else(|| "codex".into());
    #[cfg(windows)]
    if env::current_dir()?.join("chat-terminal-fixture").is_file() {
        return chat_terminal(&provider);
    }
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

#[cfg(windows)]
fn chat_terminal(provider: &str) -> io::Result<()> {
    use std::ffi::c_void;
    #[link(name = "kernel32")]
    extern "system" {
        fn GetStdHandle(kind: u32) -> *mut c_void;
        fn GetConsoleMode(handle: *mut c_void, mode: *mut u32) -> i32;
        fn SetConsoleMode(handle: *mut c_void, mode: u32) -> i32;
    }
    let stdin = unsafe { GetStdHandle(-10_i32 as u32) };
    let mut mode = 0;
    if unsafe {
        GetConsoleMode(stdin, &mut mode) == 0 || SetConsoleMode(stdin, (mode & !7) | 0x200) == 0
    } {
        return Err(io::Error::last_os_error());
    }
    let native = provider != "codex";
    let ready = "\x1b[?2004h\x1b[2J\x1b[H› Ask Codex to do anything\x1b[1;3H";
    let footer = if provider == "claude" {
        "? for shortcuts"
    } else {
        "Interactive · Manual Approval"
    };
    let show = |draft: &str, footer: &str| {
        print!("\x1b[?2004h\x1b[2J\x1b[H─────────\r\n❯ {draft}\r\n─────────\r\n{footer}");
    };
    if native {
        show("", footer);
    } else {
        print!("{ready}");
    }
    io::stdout().flush()?;
    let mut input = io::stdin().lock();
    let mut bytes = Vec::new();
    let mut packet = [0; 1024];
    loop {
        let size = input.read(&mut packet)?;
        if size == 0 {
            return Ok(());
        }
        bytes.extend_from_slice(&packet[..size]);
        if native && bytes.starts_with(b"/permissions ") {
            if bytes.last() == Some(&13) {
                show(
                    "",
                    if bytes.starts_with(b"/permissions assisted") {
                        "Interactive · Assisted Approval"
                    } else {
                        footer
                    },
                );
                bytes.clear();
            } else {
                show(&String::from_utf8_lossy(&bytes), footer);
            }
            io::stdout().flush()?;
            continue;
        }
        if let Some(command) = bytes.strip_prefix(b"/model ").map(String::from_utf8_lossy) {
            if let Some(command) = command.strip_suffix('\r') {
                let (model, effort) = command
                    .split_once(" --reasoning=")
                    .unwrap_or((command, "medium"));
                print!("\x1b[2J\x1b[H• Model changed to {model} {effort} for this session only\r\n› Ask Codex to do anything\x1b[2;3H");
                bytes.clear();
            } else {
                print!("\x1b[2J\x1b[H› /model {command}");
            }
            io::stdout().flush()?;
            continue;
        }
        if bytes.ends_with(b"\x1b[201~\r") {
            break;
        }
        if bytes.ends_with(b"\x1b[201~") {
            let text = String::from_utf8_lossy(&bytes);
            let mut lines = text
                .split("\x1b[200~")
                .skip(1)
                .filter_map(|line| line.split_once("\x1b[201~").map(|(body, _)| body));
            if native {
                show(
                    &lines.collect::<Vec<_>>().join("\r\n").replace('\r', "\r\n"),
                    footer,
                );
                io::stdout().flush()?;
            } else if let Some(first) = lines.next() {
                print!("\x1b[2J\x1b[H› {first}");
                for line in lines {
                    print!("\r\n  {line}");
                }
                io::stdout().flush()?;
            }
        }
        if bytes.len() > 64 * 1024 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid fixture input",
            ));
        }
    }
    let folder = PathBuf::from(env::var_os("WORKFLOW_TEST_INPUT_DIR").unwrap());
    fs::create_dir_all(&folder)?;
    let interaction = env::current_dir()?
        .join("chat-interaction-fixture")
        .is_file();
    let output = if interaction { "interaction" } else { "chat" };
    fs::write(folder.join(format!("{provider}-{output}.bin")), bytes)?;
    if interaction {
        let approval = match provider {
            "claude" => "Do you want to proceed?\r\nRead the sample notes\r\n❯ 1. Yes\r\n  2. No\r\nEsc to cancel",
            "copilot" => "Approval required\r\nRead the sample notes\r\n❯ 1. Yes\r\n  2. No, and tell Copilot what to do differently (Esc)",
            _ => "Would you like to run the following command?\r\n$ echo 'Read the sample notes'\r\n› 1. Yes, proceed\r\n  2. No\r\nPress enter to confirm or esc to cancel",
        };
        let (heading, marker, hint) = match provider {
            "claude" => ("", '❯', "Enter to select · ↑/↓ to navigate · Esc to cancel"),
            "copilot" => ("Question 1/1\r\n", '❯', "Enter to select"),
            _ => (
                "Question 1/1 (1 unanswered)\r\n",
                '›',
                "Tab to add notes · Enter to submit answer · Esc to interrupt",
            ),
        };
        print!("\x1b[2J\x1b[H{approval}");
        io::stdout().flush()?;
        loop {
            let size = input.read(&mut packet)?;
            if size == 0 {
                return Ok(());
            }
            if packet[..size].contains(&b'\r') {
                break;
            }
        }
        let titles = [
            "Apply the change where?",
            "Apply the test where?",
            "What should I do instead?",
        ];
        for (index, title) in titles.iter().enumerate() {
            let mut selected: usize = 1;
            let mut keys = Vec::new();
            let custom = index == 2;
            let mut notes = false;
            let mut draft = String::new();
            let descriptions = "\r\n    Leave the remaining files unchanged.";
            loop {
                let first = if selected == 1 { marker } else { ' ' };
                let second = if selected == 2 { marker } else { ' ' };
                let other = if custom {
                    let label = match provider {
                        "claude" => {
                            if draft.is_empty() {
                                "Type something."
                            } else {
                                &draft
                            }
                        }
                        "copilot" => {
                            if draft.is_empty() {
                                "Other (type your answer)"
                            } else {
                                &draft
                            }
                        }
                        _ => "None of the above",
                    };
                    let description = match provider {
                        "claude" => "\r\n  4. Chat about this",
                        "copilot" => "",
                        _ => "\r\n    Optionally, add details in notes (tab)",
                    };
                    format!(
                        "\r\n{} 3. {label}{description}",
                        if selected == 3 { marker } else { ' ' }
                    )
                } else {
                    String::new()
                };
                let note = if notes && provider == "codex" {
                    format!("\r\n────────\r\n› {}\r\n────────\r\nTab or Esc to clear notes · Enter to submit answer", if draft.is_empty() { "\x1b[2mAdd notes\x1b[22m" } else { &draft })
                } else {
                    format!("\r\n{hint}")
                };
                let mut display = format!("{heading}{title}\r\n{first} 1 - Only one{descriptions}\r\n{second} 2 - All of them\r\n    Apply the change to every file.{other}{note}");
                if custom && selected == 3 && provider != "codex" && draft.is_empty() {
                    let placeholder = if provider == "claude" {
                        "Type something."
                    } else {
                        "Other (type your answer)"
                    };
                    display =
                        display.replace(placeholder, &format!("\x1b[2m{placeholder}\x1b[22m"));
                }
                print!("\x1b[2J\x1b[H{display}");
                if custom && selected == 3 && (provider != "codex" || notes) {
                    let row = display
                        .split("\r\n")
                        .enumerate()
                        .filter(|(_, line)| {
                            if provider == "codex" {
                                line.starts_with("› ")
                            } else {
                                line.contains("3. ")
                            }
                        })
                        .map(|(row, _)| row)
                        .last()
                        .unwrap()
                        + 1;
                    let col = (if provider == "codex" { 3 } else { 6 }) + draft.chars().count();
                    print!("\x1b[{row};{col}H");
                }
                io::stdout().flush()?;
                loop {
                    let size = input.read(&mut packet)?;
                    if size == 0 {
                        return Ok(());
                    }
                    keys.extend_from_slice(&packet[..size]);
                    if keys.ends_with(b"\x1b[B")
                        || keys.ends_with(b"\x1b[A")
                        || keys.ends_with(b"\r")
                        || keys.ends_with(b"\t")
                        || keys.ends_with(b"\x1b[201~")
                    {
                        break;
                    }
                }
                if keys.ends_with(b"\r") {
                    fs::write(
                        folder.join(format!("{provider}-choice-{index}.txt")),
                        selected.to_string(),
                    )?;
                    if notes {
                        fs::write(folder.join(format!("{provider}-custom.txt")), &draft)?;
                    }
                    // Keep the answered dialog visible during the provider's redraw delay.
                    std::thread::sleep(std::time::Duration::from_millis(1500));
                    break;
                }
                if keys.ends_with(b"\t") && custom {
                    notes = true;
                } else if notes && keys.starts_with(b"\x1b[200~") && keys.ends_with(b"\x1b[201~") {
                    draft = String::from_utf8_lossy(&keys[6..keys.len() - 6]).to_string();
                } else {
                    selected = if keys.ends_with(b"\x1b[B") {
                        (selected + 1).min(if custom { 3 } else { 2 })
                    } else {
                        selected.saturating_sub(1).max(1)
                    };
                    if custom && selected == 3 && provider != "codex" {
                        notes = true;
                    }
                }
                keys.clear();
                print!("\x1b[2J\x1b[H");
                io::stdout().flush()?;
                std::thread::sleep(std::time::Duration::from_millis(120));
            }
        }
        {
            let screen = "Fixture interaction uses a different layout.";
            print!("\x1b[2J\x1b[H{screen}");
            io::stdout().flush()?;
            loop {
                let size = input.read(&mut packet)?;
                if size == 0 {
                    return Ok(());
                }
                if packet[..size].contains(&b'\r') {
                    break;
                }
            }
        }
        if native {
            show("", footer);
        } else {
            print!("{ready}");
        }
        io::stdout().flush()?;
        while input.read(&mut packet)? > 0 {}
        return Ok(());
    }
    print!("{ready}\x1b[2;1HWorking (test) · esc to interrupt\x1b[1;3H");
    io::stdout().flush()?;
    // Keep the same process available so the renderer can reattach to it.
    while input.read(&mut packet)? > 0 {}
    Ok(())
}
