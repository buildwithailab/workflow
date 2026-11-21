//! Ownership is established at spawn, never inferred from a provider's name.
use anyhow::{bail, Context, Result};
use std::{process::Command, sync::atomic::{AtomicBool, Ordering}, time::{Duration, Instant}};

#[derive(Clone, Copy, PartialEq, Eq)]
struct Identity { pid: i32, born: (u64, u64) }

fn info(pid: i32) -> Option<libc::proc_bsdinfo> {
    let mut value = std::mem::MaybeUninit::<libc::proc_bsdinfo>::uninit();
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as i32;
    let count = unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDTBSDINFO, 0, value.as_mut_ptr().cast(), size) };
    (count == size).then(|| unsafe { value.assume_init() })
}

fn identity(value: &libc::proc_bsdinfo) -> Identity {
    Identity { pid: value.pbi_pid as i32, born: (value.pbi_start_tvsec, value.pbi_start_tvusec) }
}

pub(crate) fn configure(command: &mut Command) {
    use std::os::unix::process::CommandExt;
    command.process_group(0);
}

pub(crate) struct Group { root: Identity, session: bool, stopped: AtomicBool }

impl Group {
    pub(crate) fn new(pid: u32, session: bool) -> Result<Self> {
        let pid = i32::try_from(pid)?;
        let value = info(pid).context("Süreç sahipliği doğrulanamadı")?;
        if value.pbi_pgid != pid as u32 || (session && unsafe { libc::getsid(pid) } != pid) {
            bail!("Süreç grubu uygulamaya ait değil");
        }
        Ok(Self { root: identity(&value), session, stopped: AtomicBool::new(false) })
    }

    fn contains(&self, value: &libc::proc_bsdinfo) -> bool {
        let pid = value.pbi_pid as i32;
        let owned = if self.session { (unsafe { libc::getsid(pid) }) == self.root.pid }
            else { value.pbi_pgid == self.root.pid as u32 };
        owned && value.pbi_uid == unsafe { libc::geteuid() } && identity(value).born >= self.root.born && value.pbi_status != 5
    }

    fn members(&self) -> Vec<Identity> {
        if info(self.root.pid).is_some_and(|value| identity(&value) != self.root) { return vec![]; }
        // A PTY may create foreground process groups within its own session.
        // A pipe-based probe owns only its new process group.
        let count = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
        if count <= 0 { return vec![]; }
        let mut pids = vec![0i32; count as usize + 128];
        let count = unsafe { libc::proc_listallpids(pids.as_mut_ptr().cast(), std::mem::size_of_val(&*pids) as i32) };
        pids.into_iter().take(count.max(0) as usize).filter_map(|pid| {
            let value = info(pid)?;
            let id = identity(&value);
            self.contains(&value).then_some(id)
        }).collect()
    }

    pub(crate) fn owns(&self, pid: u32) -> bool {
        self.members().iter().any(|p| p.pid == pid as i32)
    }

    pub(crate) fn stop(&self) {
        if self.stopped.swap(true, Ordering::SeqCst) { return; }
        let signal = |members: Vec<Identity>, signal| {
            for member in members {
                // Recheck birth time immediately before signalling to avoid PID reuse.
                if info(member.pid).is_some_and(|value| identity(&value) == member && self.contains(&value)) {
                    unsafe { libc::kill(member.pid, signal); }
                }
            }
        };
        signal(self.members(), libc::SIGTERM);
        let deadline = Instant::now() + Duration::from_millis(250);
        while Instant::now() < deadline {
            if self.members().is_empty() { return; }
            std::thread::sleep(Duration::from_millis(15));
        }
        // Include children created while termination was in progress.
        signal(self.members(), libc::SIGKILL);
    }
}

impl Drop for Group { fn drop(&mut self) { self.stop(); } }

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn a_changed_root_birth_identity_never_signals_a_reused_pid() {
        let mut command=Command::new("/bin/sleep");command.arg("30");configure(&mut command);
        let mut child=command.spawn().unwrap();let owner=Group::new(child.id(),false).unwrap();
        let stale=Group {root:Identity {pid:owner.root.pid,born:(0,0)},session:false,stopped:AtomicBool::new(false)};
        stale.stop();assert!(child.try_wait().unwrap().is_none());
        owner.stop();child.wait().unwrap();
    }
    #[test]
    fn pty_session_stops_term_resistant_descendants() {
        let system=portable_pty::native_pty_system();
        let pair=system.openpty(portable_pty::PtySize {rows:24,cols:80,pixel_width:0,pixel_height:0}).unwrap();
        let mut command=portable_pty::CommandBuilder::new("/bin/sh");command.args(["-c","trap '' TERM; sleep 30 & wait"]);
        let mut child=pair.slave.spawn_command(command).unwrap();
        let group=Group::new(child.process_id().unwrap(),true).unwrap();
        let deadline=Instant::now()+Duration::from_secs(2);
        while group.members().len()<2 && Instant::now()<deadline {std::thread::sleep(Duration::from_millis(10));}
        assert!(group.members().len()>=2);group.stop();child.wait().unwrap();
        let deadline=Instant::now()+Duration::from_secs(2);
        while !group.members().is_empty() && Instant::now()<deadline {std::thread::sleep(Duration::from_millis(10));}
        assert!(group.members().is_empty());
    }
    #[test]
    fn owned_group_stops_parent_and_child_without_touching_another_group() {
        let mut other = Command::new("/bin/sleep").arg("30").spawn().unwrap();
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "sleep 30 & wait"]);
        configure(&mut command);
        let mut child = command.spawn().unwrap();
        let group = Group::new(child.id(), false).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while group.members().len() < 2 && Instant::now() < deadline { std::thread::sleep(Duration::from_millis(10)); }
        assert!(group.members().len() >= 2);
        assert!(!group.owns(other.id()));
        group.stop();
        child.wait().unwrap();
        assert!(group.members().is_empty());
        assert!(other.try_wait().unwrap().is_none());
        other.kill().unwrap(); other.wait().unwrap();
    }
}
