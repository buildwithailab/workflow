//! Only linked into an explicitly enabled desktop test build, with an isolated profile.
use crate::core::{atomic, id, Core};
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::{io::{BufRead, BufReader, Read, Write}, net::{TcpListener,TcpStream}, sync::Arc, time::Duration};
use tauri::Manager;

pub(crate) fn record_startup_exit(core: &Core) {
    if std::env::var("WORKFLOW_TEST_MODE").ok().as_deref() != Some("1") || crate::profile_override().is_none() { return; }
    let result = json!({"startupCompleted":super::startup_completed(&core.app),
        "exitInstallationStarted":core.updates.exit_installation_started()});
    let _ = serde_json::to_vec(&result).map(|bytes| atomic(&core.directory.join("test-startup-exit.json"), &bytes));
}

pub(crate) fn start(core: &Arc<Core>) -> Result<()> {
    if std::env::var("WORKFLOW_TEST_MODE").ok().as_deref() != Some("1") || crate::profile_override().is_none() { return Ok(()); }
    let listener = TcpListener::bind(("127.0.0.1",0))?;
    let token = id();
    atomic(&core.directory.join("test-control.json"), &serde_json::to_vec(&json!({"endpoint":listener.local_addr()?.to_string(),"token":token}))?)?;
    let weak = Arc::downgrade(core);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Some(core) = weak.upgrade() else { break; };
            if let Ok(mut stream) = stream {
                let result = connection(&core,&mut stream,&token);
                let response = match result { Ok(value) => json!({"result":value}), Err(error) => json!({"error":error.to_string()}) };
                let _ = writeln!(stream,"{response}");
            }
        }
    });
    Ok(())
}

fn connection(core: &Arc<Core>, stream: &mut TcpStream, token: &str) -> Result<Value> {
    stream.set_read_timeout(Some(Duration::from_secs(20)))?;
    let mut text = String::new();
    BufReader::new(stream.try_clone()?).take(2*1024*1024).read_line(&mut text)?;
    let args: Value = serde_json::from_str(&text)?;
    if args["token"].as_str() != Some(token) { bail!("Test connection rejected"); }
    let label=if args["view"]=="popup" {crate::notification_popup::LABEL}else{"main"};
    let view = core.app.get_webview(label).context("Test view closed")?;
    match args["action"].as_str() {
        Some("evaluate") => tauri::async_runtime::block_on(super::webview::evaluate(&view,args["script"].as_str().context("Test script missing")?)),
        Some("input") => tauri::async_runtime::block_on(super::webview::input(&view,args["method"].as_str().context("Input method missing")?,args["args"].clone())),
        Some("close") => { core.app.get_window("main").context("Test window closed")?.close()?; Ok(json!(true)) },
        Some("quit") => { core.app.exit(0); Ok(json!(true)) },
        Some("minimize") => {core.app.get_window("main").context("Test window closed")?.minimize()?;Ok(json!(true))},
        Some("restore") => {let window=core.app.get_window("main").context("Test window closed")?;window.unminimize()?;window.set_focus()?;Ok(json!(true))},
        Some("prepare_drop") => {
            let window=core.app.get_window("main").context("Test window closed")?;
            window.set_title("Workflow Native Drag Fixture")?;
            window.set_size(tauri::LogicalSize::new(900.,700.))?;
            window.set_position(tauri::LogicalPosition::new(600.,40.))?;
            Ok(json!(true))
        },
        Some("native_state") => {
            let app=core.app.clone();let (send,receive)=std::sync::mpsc::sync_channel(1);
            core.app.run_on_main_thread(move || {
                let result=(||->Result<Value>{
                    let mtm=objc2::MainThreadMarker::new().context("Test main thread required")?;
                    let application=objc2_app_kit::NSApplication::sharedApplication(mtm);
                    let main=app.get_window("main").context("Test window closed")?;
                    let pointer=main.ns_window()?;
                    let window=unsafe {&*pointer.cast::<objc2_app_kit::NSWindow>()};
                    Ok(json!({"badge":application.dockTile().badgeLabel().map(|label|label.to_string()).unwrap_or_default(),
                        "restorable":window.isRestorable(),"minimized":window.isMiniaturized(),"keyWindow":window.isKeyWindow(),
                        "frontPid":objc2_app_kit::NSWorkspace::sharedWorkspace().frontmostApplication().map(|app|app.processIdentifier()),
                        "popupVisible":app.get_window(crate::notification_popup::LABEL).is_some_and(|window|window.is_visible().unwrap_or(false))}))
                })();let _=send.send(result);
            })?;
            receive.recv_timeout(Duration::from_secs(5)).context("Native test state timeout")?
        },
        _ => bail!("Unknown test operation"),
    }
}
