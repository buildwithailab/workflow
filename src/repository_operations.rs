use anyhow::{bail, Context, Result};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Condvar, Mutex},
};

pub(crate) const CHANGED: &str = "Çalışma alanı işlem sırasında değişti. Yeniden dene.";

// Never wait for this reservation while holding Core::operations, db or terms.
// All linked worktrees share the canonical Git common directory. Git-free folders
// receive a local key without creating a repository or writing anything to disk.
#[derive(Default)]
pub(crate) struct RepositoryOperations {
    active: Mutex<Active>,
    changed: Condvar,
}

pub(crate) struct RepositoryGuard<'a> {
    registry: &'a RepositoryOperations,
    reservation: Arc<Reservation>,
}

#[derive(Default)]
struct Active {
    reservations: Vec<Arc<Reservation>>,
    revision: u64,
}

#[derive(Default)]
struct Reservation {
    // Keep the requested directory -> owning repository mapping, including subdirectories.
    // Extra worktrees can appear during our own operations without changing this identity.
    identity: BTreeMap<PathBuf, PathBuf>,
    keys: BTreeSet<PathBuf>,
    scopes: BTreeSet<PathBuf>,
}

fn canonical_location(path: &Path) -> Result<PathBuf> {
    if path.try_exists()? {
        return Ok(dunce::canonicalize(path)?);
    }
    let parent = path.parent().context("Çalışma klasörü bulunamadı.")?;
    let name = path.file_name().context("Çalışma klasörü bulunamadı.")?;
    Ok(canonical_location(parent)?.join(name))
}

fn discover_repository(directory: &Path) -> Result<Option<PathBuf>> {
    let mut probe = directory;
    while !probe.is_dir() {
        probe = probe.parent().context("Çalışma klasörü bulunamadı.")?;
    }
    let common = match crate::core::git(probe, &["rev-parse", "--git-common-dir"]) {
        Ok(common) => Some(common),
        Err(error) => {
            let not_found = |error: &anyhow::Error| {
                error.chain().any(|cause| {
                    cause
                        .downcast_ref::<std::io::Error>()
                        .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound)
                })
            };
            // Distinguish an unavailable Git executable from a corrupt repository
            // or a working directory that disappeared during discovery.
            if not_found(&error) && probe.is_dir() {
                let stable = std::env::temp_dir();
                if stable.is_dir()
                    && crate::core::run(&stable, "git", &["--version".into()])
                        .is_err_and(|error| not_found(&error))
                {
                    return Ok(None);
                }
            }
            None
        }
    };
    if let Some(common) = common {
        let common = PathBuf::from(common);
        return Ok(Some(dunce::canonicalize(if common.is_absolute() {
            common
        } else {
            probe.join(common)
        })?));
    }
    // A broken Git marker must not silently become an independent lock.
    for ancestor in directory.ancestors() {
        match fs::symlink_metadata(ancestor.join(".git")) {
            Ok(_) => bail!("Git çalışma alanının kilidi doğrulanamadı."),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(None)
}

fn normalize(path: PathBuf) -> PathBuf {
    if cfg!(windows) {
        PathBuf::from(path.to_string_lossy().to_lowercase())
    } else {
        path
    }
}

const INVALID_WORKTREES: &str = "Git çalışma alanının kilidi doğrulanamadı.";

fn worktree_paths(bytes: &[u8]) -> Result<Vec<PathBuf>> {
    let records = bytes.strip_suffix(b"\0\0").context(INVALID_WORKTREES)?;
    let mut paths = vec![];
    let mut needs_path = true;
    let mut has_head = false;
    for field in records.split(|byte| *byte == 0) {
        if needs_path {
            let raw = field
                .strip_prefix(b"worktree ")
                .context(INVALID_WORKTREES)?;
            #[cfg(unix)]
            let path = {
                use std::{ffi::OsStr, os::unix::ffi::OsStrExt};
                Path::new(OsStr::from_bytes(raw))
            };
            #[cfg(not(unix))]
            let path = Path::new(std::str::from_utf8(raw).context(INVALID_WORKTREES)?);
            if !path.is_absolute() {
                bail!(INVALID_WORKTREES);
            }
            paths.push(normalize(canonical_location(path)?));
            needs_path = false;
            has_head = false;
        } else if field.is_empty() {
            if !has_head {
                bail!(INVALID_WORKTREES);
            }
            needs_path = true;
        } else if let Some(oid) = field.strip_prefix(b"HEAD ") {
            if has_head || !matches!(oid.len(), 40 | 64) || !oid.iter().all(u8::is_ascii_hexdigit) {
                bail!(INVALID_WORKTREES);
            }
            has_head = true;
        } else if field == b"bare" {
            if has_head {
                bail!(INVALID_WORKTREES);
            }
            has_head = true;
        } else if field.starts_with(b"worktree ") {
            bail!(INVALID_WORKTREES);
        }
    }
    if needs_path || !has_head {
        bail!(INVALID_WORKTREES);
    }
    Ok(paths)
}

impl Reservation {
    fn discover(paths: &[PathBuf], for_clone: bool, list_worktrees: bool) -> Result<Self> {
        let mut result = Self::default();
        for path in paths {
            let directory = canonical_location(path)?;
            let location = normalize(directory.clone());
            if result.identity.contains_key(&location) {
                continue;
            }
            let common = discover_repository(&directory)?;
            let key = normalize(common.clone().unwrap_or_else(|| directory.join(".git")));
            result.identity.insert(location.clone(), key.clone());
            result.scopes.insert(location);
            if result.keys.insert(key) && list_worktrees {
                if let Some(common) = common {
                    let raw = crate::core::git_bytes(
                        &common,
                        &["worktree", "list", "--porcelain", "-z"],
                    )?;
                    result.scopes.extend(worktree_paths(&raw)?);
                }
            }
            if for_clone {
                result.keys.insert(normalize(directory.join(".git")));
            }
        }
        Ok(result)
    }

    fn conflicts(&self, other: &Self) -> bool {
        !self.keys.is_disjoint(&other.keys)
            || self.scopes.iter().any(|left| {
                other
                    .scopes
                    .iter()
                    .any(|right| left.starts_with(right) || right.starts_with(left))
            })
    }
}

impl RepositoryOperations {
    pub(crate) fn lock(&self, paths: &[PathBuf]) -> Result<RepositoryGuard<'_>> {
        self.reserve(paths, false, true)
    }

    pub(crate) fn lock_clone(&self, destination: &Path) -> Result<RepositoryGuard<'_>> {
        self.reserve(&[destination.to_path_buf()], true, true)
    }

    fn reserve(
        &self,
        paths: &[PathBuf],
        for_clone: bool,
        wait: bool,
    ) -> Result<RepositoryGuard<'_>> {
        let mut initial = None;
        loop {
            let revision = self.active.lock().unwrap().revision;
            let requested = Reservation::discover(paths, for_clone, true)?;
            if initial.get_or_insert_with(|| requested.identity.clone()) != &requested.identity {
                bail!(CHANGED);
            }
            let mut active = self.active.lock().unwrap();
            // Another operation may have moved or added a worktree during discovery.
            if revision != active.revision {
                continue;
            }
            if active
                .reservations
                .iter()
                .any(|other| requested.conflicts(other))
            {
                if !wait {
                    bail!("Bu depoda başka bir işlem sürüyor. Tamamlanınca yeniden dene.");
                }
                drop(self.changed.wait(active).unwrap());
                continue;
            }
            let reservation = Arc::new(requested);
            active.reservations.push(reservation.clone());
            return Ok(RepositoryGuard {
                registry: self,
                reservation,
            });
        }
    }

    #[cfg(test)]
    fn try_lock(&self, paths: &[PathBuf]) -> Result<RepositoryGuard<'_>> {
        self.reserve(paths, false, false)
    }
}

impl RepositoryGuard<'_> {
    pub(crate) fn validate(&self, paths: &[PathBuf]) -> Result<()> {
        if Reservation::discover(paths, false, false)?.identity != self.reservation.identity {
            bail!(CHANGED);
        }
        Ok(())
    }
}

impl Drop for RepositoryGuard<'_> {
    fn drop(&mut self) {
        let mut active = self.registry.active.lock().unwrap();
        active
            .reservations
            .retain(|r| !Arc::ptr_eq(r, &self.reservation));
        active.revision = active.revision.wrapping_add(1);
        self.registry.changed.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        sync::{mpsc, Arc},
        time::Duration,
    };

    #[test]
    fn unrelated_folders_proceed_while_same_folder_waits_and_drop_releases() {
        let base = std::env::temp_dir().join(format!("workflow-lock-{}", crate::core::id()));
        let a = base.join("a");
        let b = base.join("b");
        fs::create_dir_all(&a).unwrap();
        fs::create_dir_all(&b).unwrap();
        let registry = Arc::new(RepositoryOperations::default());
        let held = registry.lock(&[a.clone(), a.clone()]).unwrap();
        assert!(registry.try_lock(&[a.clone()]).is_err());
        let (tx, rx) = mpsc::channel();
        let worker = {
            let registry = registry.clone();
            let a = a.clone();
            std::thread::spawn(move || {
                let _held = registry.lock(&[a]).unwrap();
                tx.send(()).unwrap();
            })
        };
        let independent = registry.lock(&[b]).unwrap();
        assert!(rx.recv_timeout(Duration::from_millis(50)).is_err());
        drop(held);
        rx.recv_timeout(Duration::from_secs(5)).unwrap();
        worker.join().unwrap();
        drop(independent);
        let _held = registry.lock(&[a]).unwrap();
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn linked_worktrees_share_a_key_and_broken_git_is_rejected() {
        let base = std::env::temp_dir().join(format!("workflow-lock-{}", crate::core::id()));
        let main = base.join("main");
        let parent = base.join("parent");
        let linked = parent.join("linked");
        let sibling = base.join("parent-other");
        fs::create_dir_all(&parent).unwrap();
        fs::create_dir_all(&sibling).unwrap();
        fs::create_dir_all(&main).unwrap();
        crate::core::git(&main, &["init", "-b", "main"]).unwrap();
        let hooks = base.join("hooks");
        fs::create_dir(&hooks).unwrap();
        crate::core::git(
            &main,
            &["config", "core.hooksPath", hooks.to_str().unwrap()],
        )
        .unwrap();
        crate::core::git(
            &main,
            &[
                "-c",
                "commit.gpgsign=false",
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@localhost",
                "commit",
                "--allow-empty",
                "-m",
                "base",
            ],
        )
        .unwrap();
        crate::core::git(
            &main,
            &["worktree", "add", "-b", "topic", linked.to_str().unwrap()],
        )
        .unwrap();
        let registry = RepositoryOperations::default();
        let held = registry.lock(&[main.clone()]).unwrap();
        assert!(registry.try_lock(&[linked.clone()]).is_err());
        // The request was for main in a separate directory. Its linked checkout lies
        // inside this independent project, from which the same physical files can be edited.
        assert!(registry.try_lock(&[parent.clone()]).is_err());
        assert!(registry.try_lock(&[sibling]).is_ok());
        assert!(held.validate(&[linked.clone()]).is_err());
        assert!(held.validate(&[main.clone()]).is_ok());
        drop(held);
        let outer = registry.lock(&[parent.clone()]).unwrap();
        assert!(registry.try_lock(&[main.clone()]).is_err());
        drop(outer);
        fs::write(linked.join(".git"), "gitdir: missing\n").unwrap();
        assert!(registry.lock(&[linked]).is_err());
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn malformed_worktree_inventory_fails_closed() {
        for bytes in [
            &b""[..],
            b"worktree relative\0HEAD invalid\0\0",
            b"HEAD invalid\0\0",
            b"worktree missing-terminator",
        ] {
            assert!(worktree_paths(bytes).is_err());
        }
    }
}
