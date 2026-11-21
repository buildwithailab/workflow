use anyhow::{bail,Context,Result};
use std::{fs,io::Read,os::unix::fs::PermissionsExt,path::Path,process::{Command,Stdio},sync::mpsc,time::{Duration,Instant}};

fn run(mut command: Command, capture: bool) -> Result<Vec<u8>> {
    command.stdin(Stdio::null()).stdout(if capture {Stdio::piped()} else {Stdio::null()}).stderr(Stdio::null());
    super::process::configure(&mut command);
    let mut child=command.spawn()?;
    let group=match super::process::Group::new(child.id(),false) {
        Ok(group)=>group,
        Err(error)=>{let _=child.kill();let _=child.wait();return Err(error);}
    };
    let receiver=child.stdout.take().map(|stdout| {
        let (send,receive)=mpsc::channel();
        std::thread::spawn(move || {
            let mut bytes=Vec::new();
            let result=stdout.take(128*1024*1024+1).read_to_end(&mut bytes).map(|_|bytes);
            let _=send.send(result);
        });
        receive
    });
    let result=(|| {
        let deadline=Instant::now()+Duration::from_secs(60);
        loop {
            if let Some(status)=child.try_wait()? {
                if !status.success(){bail!("Eklenti komutu tamamlanamadı");}
                return if let Some(receiver)=receiver {
                    let bytes=receiver.recv_timeout(deadline.saturating_duration_since(Instant::now())).context("Eklenti çıktısı zaman aşımına uğradı")??;
                    if bytes.len()>128*1024*1024 {bail!("Eklenti çok büyük");}
                    Ok(bytes)
                }else{Ok(Vec::new())};
            }
            if Instant::now()>=deadline {bail!("Eklenti komutu zaman aşımına uğradı");}
            std::thread::sleep(Duration::from_millis(20));
        }
    })();
    group.stop();
    let _=child.wait();
    result
}

pub(crate) fn prepare(root: &Path, archive: &Path, expected: &str) -> Result<()> {
    // Read a single pinned archive member to stdout; never extract archive paths.
    let mut command=Command::new("/usr/bin/tar");
    command.arg("-xOzf").arg(archive).args(["--","openspec"]).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null());
    let bytes=run(command,true)?;
    if crate::core::hash(&bytes)!=expected {bail!("Eklenti bütünlük denetimi başarısız");}
    let executable=root.join("openspec");crate::core::atomic(&executable,&bytes)?;
    fs::set_permissions(&executable,fs::Permissions::from_mode(0o755))?;
    let template=root.join("template");fs::create_dir(&template)?;
    let config=template.join("config");fs::create_dir(&config)?;
    let mut command=Command::new(executable);
    command.args(["init","--tools","agents","--profile","core","--no-animation"]).current_dir(&template)
        .env_clear().env("HOME",&config).env("TMPDIR",&config).env("XDG_CONFIG_HOME",&config).env("PATH","/usr/bin:/bin")
        .env("DO_NOT_TRACK","1").env("OPENSPEC_TELEMETRY","0").env("CI","true").stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    run(command,false)?;
    Ok(())
}
