use crate::{browser, core::*, terminal};
use anyhow::{bail, Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    io::{Read, Write},
    net::{Ipv4Addr, TcpListener},
    os::windows::process::CommandExt,
    path::Path,
    process::{Child, Command, Stdio},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use windows::Win32::{
    Foundation::{ERROR_INSUFFICIENT_BUFFER, ERROR_NO_DATA},
    NetworkManagement::IpHelper::{
        GetExtendedTcpTable, MIB_TCP6TABLE_OWNER_PID, MIB_TCPTABLE_OWNER_PID,
        TCP_TABLE_OWNER_PID_LISTENER,
    },
    Networking::WinSock::{AF_INET, AF_INET6},
};

const LOG_LIMIT: usize = 16_384;
const START_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Default)]
pub struct Previews {
    runs: Mutex<HashMap<String, Arc<Mutex<Run>>>>,
}

struct Run {
    id: String,
    address: url::Url,
    phase: &'static str,
    message: String,
    page: Option<String>,
    open_gate: Arc<tokio::sync::Mutex<()>>,
    log: Vec<u8>,
    job: Option<terminal::Job>,
}

impl Run {
    fn public(&self) -> Value {
        json!({"runId":self.id,"state":self.phase,"url":self.address.as_str(),"message":self.message,
            "pageId":self.page,"log":String::from_utf8_lossy(&self.log)})
    }
}

impl Previews {
    fn get(&self, sid: &str) -> Option<Arc<Mutex<Run>>> {
        self.runs.lock().unwrap().get(sid).cloned()
    }

    pub fn running(&self, sid: &str) -> bool {
        self.get(sid)
            .is_some_and(|run| run.lock().unwrap().job.is_some())
    }

    pub fn shutdown(&self) {
        for run in self.runs.lock().unwrap().values() {
            let mut run = run.lock().unwrap();
            run.phase = "stopped";
            run.job.take();
        }
    }
}

fn address(raw: &str) -> Result<url::Url> {
    let mut address = browser::url(raw)?;
    if !matches!(address.host_str(), Some("127.0.0.1" | "localhost"))
        || address.port().is_none()
        || !address.username().is_empty()
        || address.password().is_some()
    {
        bail!("127.0.0.1 ve açık bir port içeren yerel adres gir. Ör. http://127.0.0.1:5080/")
    }
    address.set_host(Some("127.0.0.1"))?;
    Ok(address)
}

fn table(family: u32) -> Result<Vec<u32>> {
    let mut bytes = 0;
    for _ in 0..4 {
        let mut table = vec![0u32; (bytes as usize).div_ceil(4)];
        let pointer = (!table.is_empty()).then(|| table.as_mut_ptr().cast());
        let status = unsafe {
            GetExtendedTcpTable(
                pointer,
                &mut bytes,
                false,
                family,
                TCP_TABLE_OWNER_PID_LISTENER,
                0,
            )
        };
        match status {
            0 => return Ok(table),
            error if error == ERROR_INSUFFICIENT_BUFFER.0 => {}
            error if error == ERROR_NO_DATA.0 => return Ok(vec![]),
            _ => bail!("Windows port sahipliğini doğrulayamadı ({status})."),
        }
    }
    bail!("Windows port listesi okunamadı.")
}

fn rows<T: Copy>(table: &[u32], offset: usize) -> impl Iterator<Item = T> + '_ {
    let count = table.first().copied().unwrap_or(0) as usize;
    let length = std::mem::size_of_val(table).saturating_sub(offset);
    let stride = std::mem::size_of::<T>();
    (0..count.min(length / stride)).map(move |i| unsafe {
        // Windows returns a length followed by fixed-size, initialized rows.
        table
            .as_ptr()
            .cast::<u8>()
            .add(offset + i * stride)
            .cast::<T>()
            .read_unaligned()
    })
}

fn listeners(port: u16) -> Result<Vec<u32>> {
    use windows::Win32::NetworkManagement::IpHelper::{
        MIB_TCP6ROW_OWNER_PID, MIB_TCPROW_OWNER_PID,
    };
    let matches_port = |value| u16::from_be(value as u16) == port;
    let v4 = table(AF_INET.0 as u32)?;
    let v6 = table(AF_INET6.0 as u32)?;
    let owners =
        rows::<MIB_TCPROW_OWNER_PID>(&v4, std::mem::offset_of!(MIB_TCPTABLE_OWNER_PID, table))
            .filter(|row| {
                matches_port(row.dwLocalPort)
                    && matches!(
                        Ipv4Addr::from(row.dwLocalAddr.to_ne_bytes()),
                        Ipv4Addr::UNSPECIFIED | Ipv4Addr::LOCALHOST
                    )
            })
            .map(|row| row.dwOwningPid)
            .chain(
                rows::<MIB_TCP6ROW_OWNER_PID>(
                    &v6,
                    std::mem::offset_of!(MIB_TCP6TABLE_OWNER_PID, table),
                )
                .filter(|row| matches_port(row.dwLocalPort) && row.ucLocalAddr == [0; 16])
                .map(|row| row.dwOwningPid),
            );
    Ok(owners.collect())
}

fn owns_listener(run: &Run) -> Result<bool> {
    let Some(job) = &run.job else {
        return Ok(false);
    };
    let port = run.address.port().unwrap();
    let pids = listeners(port)?;
    if pids.iter().any(|&pid| !job.owns(pid)) {
        bail!("Port {port} başka bir süreç tarafından kullanılıyor.")
    }
    Ok(!pids.is_empty())
}

fn shell(command: &str, directory: &Path, address: &url::Url) -> Result<(Child, terminal::Job)> {
    let powershell = std::env::var_os("SystemRoot").context("Windows sistem klasörü bulunamadı")?;
    let powershell = Path::new(&powershell).join("System32/WindowsPowerShell/v1.0/powershell.exe");
    let mut cmd = Command::new(powershell);
    cmd.current_dir(external_path(directory))
        .args(["-NoLogo", "-NoProfile", "-NonInteractive", "-Command",
            "$ErrorActionPreference='Stop'; $code=[Text.Encoding]::UTF8.GetString([Convert]::FromBase64String([Console]::In.ReadToEnd())); & ([scriptblock]::Create($code)); if($null -ne $LASTEXITCODE){exit $LASTEXITCODE}"])
        .env("WORKFLOW_PREVIEW_URL", address.as_str())
        .env("WORKFLOW_PREVIEW_PORT", address.port().unwrap().to_string())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .creation_flags(0x08000000); // CREATE_NO_WINDOW
    for (key, _) in std::env::vars_os() {
        if terminal::internal_variable(&key.to_string_lossy()) {
            cmd.env_remove(key);
        }
    }
    let mut child = cmd.spawn().context("Önizleme komutu başlatılamadı")?;
    let job = match terminal::Job::new(child.id()) {
        Ok(job) => job,
        Err(error) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(error.context("Önizleme süreç grubu oluşturulamadı"));
        }
    };
    // The shell receives the command only after it belongs to the job.
    if let Err(error) = child
        .stdin
        .take()
        .unwrap()
        .write_all(STANDARD.encode(command).as_bytes())
    {
        drop(job);
        let _ = child.wait();
        return Err(error.into());
    }
    Ok((child, job))
}

fn capture(mut reader: impl Read + Send + 'static, run: Arc<Mutex<Run>>) {
    std::thread::spawn(move || {
        let mut bytes = [0u8; 2048];
        while let Ok(n) = reader.read(&mut bytes) {
            if n == 0 {
                break;
            }
            let mut run = run.lock().unwrap();
            run.log.extend_from_slice(&bytes[..n]);
            let excess = run.log.len().saturating_sub(LOG_LIMIT);
            run.log.drain(..excess);
        }
    });
}

fn notify(core: &Core, sid: &str, run: &Run) {
    core.emit(
        "preview",
        json!({"sessionId":sid,"runId":run.id,"state":run.phase,"pageId":run.page}),
    );
}

fn failure(core: &Core, sid: &str, run: &Mutex<Run>, message: String) {
    let mut run = run.lock().unwrap();
    if run.job.take().is_some() {
        run.phase = "error";
        run.message = message;
        notify(core, sid, &run);
    }
}

async fn show(core: &Arc<Core>, sid: &str, run: &Arc<Mutex<Run>>, activate: bool) -> Result<Value> {
    let gate = run.lock().unwrap().open_gate.clone();
    let _opening = gate.lock().await;
    let (url, page) = {
        let run = run.lock().unwrap();
        if run.phase != "ready" || !owns_listener(&run)? {
            bail!("Önizleme henüz hazır değil.")
        }
        (run.address.to_string(), run.page.clone())
    };
    if activate {
        if let Some(id) = &page {
            if let Ok(p) =
                browser::dispatch(core, "activate", &json!({"sessionId":sid,"pageId":id})).await
            {
                if p["url"] == url {
                    return Ok(p);
                }
            }
        }
    }
    let result = browser::dispatch(
        core,
        "open",
        &json!({
            "sessionId":sid,"url":url,"mode":"embedded","newTab":page.is_none(),"pageId":page,
        }),
    )
    .await;
    {
        let mut run = run.lock().unwrap();
        if run.phase == "ready" {
            if let Ok(p) = &result {
                run.page = p["id"].as_str().map(str::to_owned);
            } else {
                run.message = "Sunucu hazır, ancak tarayıcı sekmesi açılamadı.".into();
            }
            notify(core, sid, &run);
            return result;
        }
    }
    if let Ok(p) = result {
        let _ = browser::dispatch(core, "close", &json!({"sessionId":sid,"pageId":p["id"]})).await;
    }
    bail!("Önizleme durduruldu.")
}

pub async fn open(core: &Arc<Core>, args: &Value) -> Result<Value> {
    let sid = string(args, "sessionId")?;
    let run = core.previews.get(sid).context("Önizleme başlatılmadı.")?;
    show(core, sid, &run, true).await
}

fn monitor(core: Arc<Core>, sid: String, run: Arc<Mutex<Run>>, mut child: Child) {
    let address = run.lock().unwrap().address.clone();
    let client = reqwest::blocking::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_millis(650))
        .build();
    let mut started = Instant::now();
    loop {
        if run.lock().unwrap().job.is_none() {
            break;
        }
        match child.try_wait() {
            Ok(None) => {}
            Ok(Some(exit)) => {
                failure(
                    &core,
                    &sid,
                    &run,
                    format!("Önizleme komutu sonlandı ({exit})."),
                );
                break;
            }
            Err(error) => {
                failure(
                    &core,
                    &sid,
                    &run,
                    format!("Önizleme süreci okunamadı ({error})."),
                );
                break;
            }
        }
        let (state, ready) = {
            let run = run.lock().unwrap();
            (owns_listener(&run), run.phase == "ready")
        };
        if ready {
            match state {
                Ok(true) => {}
                Ok(false) => {
                    let mut current = run.lock().unwrap();
                    if current.job.is_none() {
                        break;
                    }
                    current.phase = "starting";
                    current.message = "Sunucunun yeniden açılması bekleniyor…".into();
                    started = Instant::now();
                    notify(&core, &sid, &current);
                }
                Err(error) => {
                    failure(&core, &sid, &run, error.to_string());
                    break;
                }
            }
        } else {
            match state {
                Ok(true)
                    if client
                        .as_ref()
                        .is_ok_and(|c| c.get(address.clone()).send().is_ok()) =>
                {
                    let mut current = run.lock().unwrap();
                    if current.job.is_none() {
                        break;
                    }
                    match owns_listener(&current) {
                        Ok(true) => {}
                        Ok(false) => continue,
                        Err(error) => {
                            drop(current);
                            failure(&core, &sid, &run, error.to_string());
                            break;
                        }
                    }
                    current.phase = "ready";
                    current.message = "Önizleme hazır.".into();
                    drop(current);
                    let (core, sid, run) = (core.clone(), sid.clone(), run.clone());
                    tauri::async_runtime::spawn(async move {
                        let _ = show(&core, &sid, &run, false).await;
                    });
                }
                Err(error) => {
                    failure(&core, &sid, &run, error.to_string());
                    break;
                }
                _ if started.elapsed() >= START_TIMEOUT => {
                    failure(&core, &sid, &run, format!("Sunucu 60 saniyede {address} adresinde hazır olmadı. Komutu, portu ve çıktıyı kontrol et."));
                    break;
                }
                _ => {}
            }
        }
        std::thread::sleep(Duration::from_millis(if ready { 750 } else { 250 }));
    }
    let _ = child.wait();
}

pub fn dispatch(core: &Arc<Core>, action: &str, args: &Value) -> Result<Value> {
    if action == "list" {
        return Ok(json!(core
            .previews
            .runs
            .lock()
            .unwrap()
            .iter()
            .map(|(sid, run)| { (sid.clone(), run.lock().unwrap().public()) })
            .collect::<HashMap<_, _>>()));
    }
    let sid = string(args, "sessionId")?;
    let _operation = core.operations.lock().unwrap();
    let session = core.session(sid)?;
    if action == "stop" {
        let run = core.previews.get(sid).context("Önizleme başlatılmadı.")?;
        let mut run = run.lock().unwrap();
        if args["runId"].as_str() == Some(&run.id) && run.job.take().is_some() {
            run.phase = "stopped";
            run.message = "Önizleme durduruldu.".into();
            notify(core, sid, &run);
        }
        return Ok(run.public());
    }
    if action == "status" {
        return Ok(core
            .previews
            .get(sid)
            .map_or(Value::Null, |run| run.lock().unwrap().public()));
    }
    if action != "start" {
        bail!("Önizleme işlemi geçersiz.")
    }
    if session["archived"] == true
        || session["login"] == true
        || core.project(string(&session, "projectId")?)?["removed"] == true
    {
        bail!("Bu oturumda önizleme başlatılamaz.")
    }
    if core.previews.running(sid) {
        bail!("Bu oturumun önizlemesi zaten çalışıyor.")
    }
    let settings = args.get("settings").unwrap_or(&session["preview"]);
    let command = string(settings, "command")?.trim();
    if command.is_empty() || command.len() > 4096 {
        bail!("Geçerli bir PowerShell komutu gir.")
    }
    let address = address(string(settings, "url")?)?;
    let port = address.port().unwrap();
    let directory = Path::new(string(&session, "directory")?);
    core.update_session(
        sid,
        json!({"preview":{"command":command,"url":address.as_str()}}),
    )?;
    let reserved = core.previews.runs.lock().unwrap().values().any(|run| {
        let run = run.lock().unwrap();
        run.job.is_some() && run.address.port() == Some(port)
    });
    if reserved
        || !listeners(port)?.is_empty()
        || TcpListener::bind((Ipv4Addr::LOCALHOST, port)).is_err()
    {
        bail!("Port {port} dolu veya kullanılamıyor. Komutu ve adresi başka bir porta ayarla.")
    }
    let (mut child, job) = shell(command, directory, &address)?;
    let (page, open_gate) = core.previews.get(sid).map_or_else(
        || (None, Arc::new(tokio::sync::Mutex::new(()))),
        |run| {
            let run = run.lock().unwrap();
            (run.page.clone(), run.open_gate.clone())
        },
    );
    let run = Arc::new(Mutex::new(Run {
        id: id(),
        address,
        phase: "starting",
        message: "Sunucu bekleniyor…".into(),
        page,
        open_gate,
        log: vec![],
        job: Some(job),
    }));
    capture(child.stdout.take().unwrap(), run.clone());
    capture(child.stderr.take().unwrap(), run.clone());
    core.previews
        .runs
        .lock()
        .unwrap()
        .insert(sid.to_owned(), run.clone());
    let status = run.lock().unwrap().public();
    let (core, sid) = (core.clone(), sid.to_owned());
    std::thread::spawn(move || monitor(core, sid, run, child));
    Ok(status)
}
