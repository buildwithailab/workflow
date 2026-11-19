use super::*;

const SOURCE_DIRTY: &str = "Birleştirmeden önce bu oturumdaki değişiklikleri commit et.";
const TARGET_DIRTY: &str = "Hedef dalın çalışma klasöründe kaydedilmemiş değişiklikler var. Önce onları commit et veya koruyarak başka yere al.";
const SOURCE_BRANCH: &str = "Birleştirmek için bu oturumda bir yerel dal açık olmalı.";

struct Plan {
    root: PathBuf,
    source: String,
    source_head: String,
    target: String,
    target_head: String,
    destination: Option<PathBuf>,
    stamp: String,
}

fn busy_operation(root: &Path) -> Result<bool> {
    for name in [
        "MERGE_HEAD",
        "CHERRY_PICK_HEAD",
        "REVERT_HEAD",
        "rebase-merge",
        "rebase-apply",
        "sequencer",
    ] {
        let file = PathBuf::from(git(root, &["rev-parse", "--git-path", name])?);
        if (if file.is_absolute() {
            file
        } else {
            root.join(file)
        })
        .exists()
        {
            return Ok(true);
        }
    }
    Ok(false)
}

fn common(root: &Path) -> Result<PathBuf> {
    let path = PathBuf::from(git(root, &["rev-parse", "--git-common-dir"])?);
    Ok(dunce::canonicalize(if path.is_absolute() {
        path
    } else {
        root.join(path)
    })?)
}

fn branch_worktrees(root: &Path, reference: &str) -> Result<Vec<PathBuf>> {
    let raw = git(root, &["worktree", "list", "--porcelain", "-z"])?;
    let mut path = None;
    let mut branch = None;
    let mut result = vec![];
    for part in raw.split('\0').chain(std::iter::once("")) {
        if part.is_empty() {
            if branch.as_deref() == Some(reference) {
                result.push(
                    path.take()
                        .context("Hedef dalın çalışma klasörü bulunamadı.")?,
                );
            }
            path = None;
            branch = None;
        } else if let Some(value) = part.strip_prefix("worktree ") {
            path = Some(PathBuf::from(value));
        } else if let Some(value) = part.strip_prefix("branch ") {
            branch = Some(value.to_owned());
        }
    }
    Ok(result)
}

fn inspect(root: &Path, target: &str) -> Result<Plan> {
    let (source, status, source_stamp) = snapshot(root)?;
    let source_name = source
        .reference
        .as_deref()
        .and_then(|r| r.strip_prefix("refs/heads/"))
        .context(SOURCE_BRANCH)?;
    let source_head = source.head.context(SOURCE_BRANCH)?;
    if !status.is_empty() {
        bail!(SOURCE_DIRTY);
    }
    if busy_operation(root)? {
        bail!("Önce bu oturumdaki devam eden Git işlemini tamamla veya iptal et.");
    }
    git(root, &["check-ref-format", "--branch", target]).context("Hedef dal geçersiz")?;
    if source_name == target {
        bail!("Kaynak ve hedef dal aynı olamaz.");
    }
    let reference = format!("refs/heads/{target}");
    let target_head = checked_ref(root, &reference).context("Hedef yerel dal bulunamadı.")?;
    let paths = branch_worktrees(root, &reference)?;
    if paths.len() > 1 {
        bail!("Hedef dal birden fazla çalışma klasöründe açık. Önce bu durumu düzelt.");
    }
    let destination = paths.first().map(dunce::canonicalize).transpose()?;
    let target_stamp = if let Some(path) = &destination {
        if common(path)? != common(root)? {
            bail!(CHANGED);
        }
        let (current, status, stamp) = snapshot(path)?;
        if current.reference.as_deref() != Some(reference.as_str())
            || current.head.as_deref() != Some(target_head.as_str())
        {
            bail!(CHANGED);
        }
        if !status.is_empty() {
            bail!(TARGET_DIRTY);
        }
        if busy_operation(path)? {
            bail!("Hedef dalda devam eden Git işlemini tamamla veya iptal et.");
        }
        Some(stamp)
    } else {
        None
    };
    let stamp = hash(
        json!([
            root,
            source_name,
            source_head,
            source_stamp,
            reference,
            target_head,
            destination,
            target_stamp
        ])
        .to_string()
        .as_bytes(),
    );
    Ok(Plan {
        root: root.to_path_buf(),
        source: source_name.into(),
        source_head,
        target: target.into(),
        target_head,
        destination,
        stamp,
    })
}

fn target_idle(core: &Core, plan: &Plan) -> Result<()> {
    let _guard = core.operations.lock().unwrap();
    if let Some(target) = &plan.destination {
        let db = core.db.lock().unwrap();
        let terms = core.terms.lock().unwrap();
        let running = db["sessions"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|session| {
                let live = session["id"]
                    .as_str()
                    .and_then(|id| terms.get(id))
                    .is_some_and(|term| !term.exited.load(std::sync::atomic::Ordering::SeqCst));
                live && session["directory"]
                    .as_str()
                    .and_then(|directory| dunce::canonicalize(directory).ok())
                    .is_some_and(|directory| {
                        directory.starts_with(target) || target.starts_with(&directory)
                    })
            });
        if running {
            bail!("Hedef dalı kullanan çalışan oturumları önce durdur.");
        }
    }
    Ok(())
}

fn ancestor(root: &Path, left: &str, right: &str) -> Result<bool> {
    let result = run(
        root,
        "git",
        &[
            "merge-base".into(),
            "--is-ancestor".into(),
            left.into(),
            right.into(),
        ],
    )?;
    match result.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => bail!("Dal ilişkisi doğrulanamadı."),
    }
}

enum Candidate {
    Ready(String),
    Conflicts(Vec<String>),
}

fn conflicts(root: &Path) -> Result<Vec<String>> {
    Ok(git(
        root,
        &["diff", "--name-only", "--diff-filter=U", "-z", "--"],
    )?
    .split('\0')
    .filter(|path| !path.is_empty())
    .map(str::to_owned)
    .collect())
}

fn discard(root: &Path, base: &Path, slot: &Path, files: &Path) -> Result<()> {
    if !files.try_exists()? {
        fs::remove_dir(slot)?;
        return Ok(());
    }
    let resolved = dunce::canonicalize(files)?;
    if !resolved.starts_with(base)
        || resolved.parent() != Some(slot)
        || crate::core::is_link(&fs::symlink_metadata(slot)?)
        || crate::core::is_link(&fs::symlink_metadata(files)?)
        || worktree_root(files)? != resolved
        || common(files)? != common(root)?
    {
        bail!("Geçici birleştirme klasörünün konumu doğrulanamadı.");
    }
    git(
        root,
        &[
            "worktree",
            "remove",
            "--force",
            "--",
            &files.to_string_lossy(),
        ],
    )
    .context("Geçici birleştirme klasörü temizlenemedi. Hedef dal değiştirilmedi.")?;
    fs::remove_dir(slot)?;
    Ok(())
}

fn candidate(plan: &Plan, base: &Path) -> Result<Candidate> {
    fs::create_dir_all(base)?;
    let base = dunce::canonicalize(base)?;
    let slot = base.join(id());
    fs::create_dir(&slot)?;
    let files = slot.join("files");
    if let Err(error) = git(
        &plan.root,
        &[
            "worktree",
            "add",
            "--detach",
            "--",
            &files.to_string_lossy(),
            &plan.target_head,
        ],
    ) {
        discard(&plan.root, &base, &slot, &files)?;
        return Err(error);
    }
    let result = (|| {
        let message = format!("Merge branch '{}' into {}", plan.source, plan.target);
        let merged = git(
            &files,
            &[
                "merge",
                "--no-edit",
                "--no-squash",
                "--no-overwrite-ignore",
                "-m",
                &message,
                &plan.source_head,
            ],
        );
        if merged.is_err() {
            let paths = conflicts(&files)?;
            if !paths.is_empty() {
                return Ok(Candidate::Conflicts(paths));
            }
            merged?;
        }
        let head = git(&files, &["rev-parse", "--verify", "HEAD"])?;
        if !ancestor(&files, &plan.source_head, &head)?
            || !ancestor(&files, &plan.target_head, &head)?
        {
            bail!("Birleştirme sonucu kaynak ve hedef commit’lerini içermiyor.");
        }
        Ok(Candidate::Ready(head))
    })();
    // Hooks may fail after checkout; clean up the same verified private slot on every path.
    discard(&plan.root, &base, &slot, &files)?;
    result
}

fn apply(
    root: &Path,
    target: &str,
    stamp: &str,
    base: &Path,
    validate: impl Fn(&Plan) -> Result<()>,
) -> Result<Value> {
    let plan = inspect(root, target)?;
    if plan.stamp != stamp {
        bail!(CHANGED);
    }
    validate(&plan)?;
    if ancestor(root, &plan.source_head, &plan.target_head)? {
        return Ok(json!({"merged":true,"unchanged":true,"target":target,"head":plan.target_head}));
    }
    let result = candidate(&plan, base)?;
    let current = inspect(root, target)?;
    if current.stamp != plan.stamp {
        bail!(CHANGED);
    }
    validate(&current)?;
    let commit = match result {
        Candidate::Conflicts(paths) => {
            return Ok(json!({"merged":false,"conflicts":paths,"target":target,"stamp":plan.stamp}))
        }
        Candidate::Ready(commit) => commit,
    };
    if let Some(destination) = &plan.destination {
        git(
            destination,
            &[
                "merge",
                "--ff-only",
                "--no-squash",
                "--no-edit",
                "--no-overwrite-ignore",
                &commit,
            ],
        )?;
    } else {
        git(
            root,
            &[
                "update-ref",
                "-m",
                "Workflow target branch merge",
                &format!("refs/heads/{target}"),
                &commit,
                &plan.target_head,
            ],
        )?;
    }
    if checked_ref(root, &format!("refs/heads/{target}"))? != commit {
        bail!("Birleştirme sonrasında hedef dal beklenen commit'te değil. Git durumunu yeniden kontrol et.");
    }
    Ok(json!({"merged":true,"unchanged":false,"source":plan.source,"target":target,"head":commit}))
}

// Include ignored files. A conflicted three-way merge can overwrite an ignored file
// even with --no-overwrite-ignore. Directory/file replacements collide in both directions.
fn protect_local_files(plan: &Plan) -> Result<()> {
    let local = git(&plan.root, &["ls-files", "--others", "-z", "--"])?;
    if local.is_empty() {
        return Ok(());
    }
    let ignore_case = cfg!(windows)
        || git(
            &plan.root,
            &[
                "config",
                "--bool",
                "--default",
                "false",
                "--get",
                "core.ignorecase",
            ],
        )? == "true";
    let key = |path: &str| {
        let path = path.trim_end_matches('/');
        if ignore_case {
            path.to_lowercase()
        } else {
            path.to_owned()
        }
    };
    let mut paths = BTreeSet::new();
    let mut add_tree = |tree: &str| -> Result<()> {
        let entries = git(
            &plan.root,
            &["ls-tree", "-r", "--name-only", "-z", tree, "--"],
        )?;
        paths.extend(entries.split('\0').filter(|path| !path.is_empty()).map(key));
        Ok(())
    };
    add_tree(&plan.target_head)?;
    // A directory rename can relocate an incoming file outside its path in the target.
    // merge-tree writes Git objects only. Status 1 still supplies the conflicted tree.
    let projection = run(
        &plan.root,
        "git",
        &[
            "merge-tree".into(),
            "--write-tree".into(),
            "-z".into(),
            "HEAD".into(),
            plan.target_head.clone(),
        ],
    )?;
    if !matches!(projection.status.code(), Some(0 | 1)) {
        bail!("Yerel dosyaları korumak için birleştirme yolları doğrulanamadı.");
    }
    let tree = projection
        .stdout
        .split(|b| *b == 0)
        .next()
        .unwrap_or_default();
    if !matches!(tree.len(), 40 | 64) || !tree.iter().all(u8::is_ascii_hexdigit) {
        bail!("Yerel dosyaları korumak için birleştirme yolları doğrulanamadı.");
    }
    add_tree(std::str::from_utf8(tree)?)?;
    for path in local.split('\0').filter(|path| !path.is_empty()).map(key) {
        let directory = format!("{path}/");
        if paths.contains(&path)
            || path
                .match_indices('/')
                .any(|(i, _)| paths.contains(&path[..i]))
            || paths
                .range(directory.clone()..)
                .next()
                .is_some_and(|next| next.starts_with(&directory))
        {
            bail!("Birleştirme bu oturumdaki izlenmeyen veya Git tarafından yok sayılan bir dosyanın üzerine yazabilir. Önce dosyayı koruyarak başka yere al.");
        }
    }
    Ok(())
}

fn resolve(
    root: &Path,
    target: &str,
    stamp: &str,
    validate: impl Fn(&Plan) -> Result<()>,
) -> Result<Value> {
    let plan = inspect(root, target)?;
    if plan.stamp != stamp {
        bail!(CHANGED);
    }
    validate(&plan)?;
    protect_local_files(&plan)?;
    let current = inspect(root, target)?;
    if current.stamp != stamp {
        bail!(CHANGED);
    }
    validate(&current)?;
    let result = git(
        root,
        &[
            "merge",
            "--no-edit",
            "--no-squash",
            "--no-overwrite-ignore",
            &plan.target_head,
        ],
    );
    let paths = conflicts(root)?;
    if let Err(error) = result {
        if paths.is_empty()
            || checked_ref(root, "MERGE_HEAD").ok().as_ref() != Some(&plan.target_head)
        {
            return Err(error);
        }
    }
    Ok(json!({"started":true,"source":plan.source,"target":target,"conflicts":paths}))
}

pub(super) fn dispatch(core: &Arc<Core>, root: &Path, action: &str, args: &Value) -> Result<Value> {
    if action == "merge_targets" {
        let current = checkout(root);
        let source = current
            .reference
            .as_deref()
            .and_then(|reference| reference.strip_prefix("refs/heads/"))
            .context(SOURCE_BRANCH)?;
        let targets = git(
            root,
            &["for-each-ref", "--format=%(refname:lstrip=2)", "refs/heads"],
        )?
        .lines()
        .filter(|name| *name != source)
        .map(str::to_owned)
        .collect::<Vec<_>>();
        let project = core.project(string(args, "projectId")?)?;
        let original = checkout(Path::new(string(&project, "directory")?));
        let preferred = original
            .reference
            .as_deref()
            .and_then(|r| r.strip_prefix("refs/heads/"))
            .filter(|name| targets.iter().any(|target| target == name));
        let selected = preferred
            .map(str::to_owned)
            .or_else(|| targets.iter().find(|name| name.as_str() == "main").cloned())
            .or_else(|| {
                targets
                    .iter()
                    .find(|name| name.as_str() == "master")
                    .cloned()
            })
            .or_else(|| targets.first().cloned());
        return Ok(json!({"source":source,"targets":targets,"selected":selected}));
    }
    let target = string(args, "target")?;
    if action == "merge_target_resolve" {
        return resolve(root, target, string(args, "stamp")?, |plan| {
            target_idle(core, plan)
        });
    }
    if action == "merge_target_apply" {
        return apply(
            root,
            target,
            string(args, "stamp")?,
            &core.directory.join("merge-workspaces"),
            |plan| target_idle(core, plan),
        );
    }
    let plan = inspect(root, target)?;
    target_idle(core, &plan)?;
    let count = git(
        root,
        &[
            "rev-list",
            "--count",
            &format!("{}..{}", plan.target_head, plan.source_head),
        ],
    )?
    .parse::<usize>()?;
    let files = git(
        root,
        &[
            "diff",
            "--name-only",
            "-z",
            &format!("{}...{}", plan.target_head, plan.source_head),
            "--",
        ],
    )?
    .split('\0')
    .filter(|path| !path.is_empty())
    .map(str::to_owned)
    .collect::<Vec<_>>();
    if inspect(root, target)?.stamp != plan.stamp {
        bail!(CHANGED);
    }
    Ok(
        json!({"source":plan.source,"target":plan.target,"stamp":plan.stamp,"commits":count,"files":files,
        "directory":plan.destination,"unchanged":count==0}),
    )
}
#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture {
        base: PathBuf,
        main: PathBuf,
        source: PathBuf,
        temporary: PathBuf,
    }
    impl Fixture {
        fn new() -> Self {
            let base = std::env::temp_dir().join(format!("workflow-merge-{}", id()));
            let main = base.join("project");
            let source = base.join("source with spaces");
            fs::create_dir_all(&main).unwrap();
            git(&main, &["init", "-b", "main"]).unwrap();
            git(&main, &["config", "user.name", "Workflow Test"]).unwrap();
            git(&main, &["config", "user.email", "test@localhost"]).unwrap();
            git(&main, &["config", "commit.gpgsign", "false"]).unwrap();
            git(&main, &["config", "core.autocrlf", "false"]).unwrap();
            let hooks = base.join("hooks");
            fs::create_dir(&hooks).unwrap();
            git(
                &main,
                &["config", "core.hooksPath", &hooks.to_string_lossy()],
            )
            .unwrap();
            Self::commit(&main, "a.txt", "base\n");
            git(
                &main,
                &[
                    "worktree",
                    "add",
                    "-b",
                    "work/source",
                    "--",
                    &source.to_string_lossy(),
                    "HEAD",
                ],
            )
            .unwrap();
            Self {
                temporary: base.join("private-merges"),
                base,
                main,
                source,
            }
        }
        fn commit(root: &Path, file: &str, text: &str) {
            fs::write(root.join(file), text).unwrap();
            git(root, &["add", "--", file]).unwrap();
            git(root, &["commit", "-m", "Synthetic merge fixture"]).unwrap();
        }
        fn head(root: &Path) -> String {
            git(root, &["rev-parse", "HEAD"]).unwrap()
        }
        fn merge(&self, target: &str) -> Result<Value> {
            let plan = inspect(&self.source, target)?;
            apply(&self.source, target, &plan.stamp, &self.temporary, |_| {
                Ok(())
            })
        }
        fn conflict(&self) -> String {
            Self::commit(&self.main, "a.txt", "target\n");
            Self::commit(&self.source, "a.txt", "source\n");
            let result = self.merge("main").unwrap();
            assert_eq!(result["conflicts"], json!(["a.txt"]));
            result["stamp"].as_str().unwrap().to_owned()
        }
        fn clean_helpers(&self) {
            assert_eq!(
                git(&self.main, &["worktree", "list", "--porcelain"])
                    .unwrap()
                    .matches("worktree ")
                    .count(),
                2
            );
            if self.temporary.exists() {
                assert_eq!(fs::read_dir(&self.temporary).unwrap().count(), 0);
            }
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            if let (Ok(base), Ok(parent)) = (
                dunce::canonicalize(&self.base),
                dunce::canonicalize(std::env::temp_dir()),
            ) {
                if base.parent() == Some(parent.as_path())
                    && base
                        .file_name()
                        .is_some_and(|name| name.to_string_lossy().starts_with("workflow-merge-"))
                {
                    let _ = fs::remove_dir_all(base);
                }
            }
        }
    }

    #[test]
    fn checked_out_target_fast_forwards_without_switching_source() {
        let f = Fixture::new();
        Fixture::commit(&f.source, "a.txt", "source change\n");
        let source = Fixture::head(&f.source);
        assert_eq!(f.merge("main").unwrap()["merged"], true);
        assert_eq!(Fixture::head(&f.main), source);
        assert_eq!(Fixture::head(&f.source), source);
        assert_eq!(
            git(&f.source, &["branch", "--show-current"]).unwrap(),
            "work/source"
        );
        assert_eq!(
            fs::read_to_string(f.main.join("a.txt")).unwrap(),
            "source change\n"
        );
        assert_eq!(f.merge("main").unwrap()["unchanged"], true);
        f.clean_helpers();
    }

    #[test]
    fn checked_out_target_moves_even_with_branch_squash_config() {
        let f = Fixture::new();
        git(&f.main, &["config", "branch.main.mergeOptions", "--squash"]).unwrap();
        Fixture::commit(&f.source, "a.txt", "source\n");
        let result = f.merge("main").unwrap();
        assert_eq!(result["head"], Fixture::head(&f.main));
        assert_eq!(Fixture::head(&f.source), Fixture::head(&f.main));
        assert!(git(&f.main, &["status", "--porcelain"]).unwrap().is_empty());
    }

    #[test]
    fn recovery_checks_original_source_and_target_even_when_new_checkout_is_clean() {
        let f = Fixture::new();
        let stamp = f.conflict();
        let source = Fixture::head(&f.source);
        git(&f.source, &["switch", "-c", "different-source"]).unwrap();
        assert!(resolve(&f.source, "main", &stamp, |_| Ok(())).is_err());
        assert_eq!(Fixture::head(&f.source), source);
        assert!(!busy_operation(&f.source).unwrap());
        git(&f.source, &["switch", "work/source"]).unwrap();
        Fixture::commit(&f.main, "b.txt", "target moved\n");
        assert!(resolve(&f.source, "main", &stamp, |_| Ok(())).is_err());
        assert!(!busy_operation(&f.source).unwrap());
        assert_eq!(Fixture::head(&f.source), source);
    }

    #[test]
    fn recovery_merges_exact_target_even_if_a_tag_has_the_same_name() {
        let f = Fixture::new();
        git(&f.main, &["tag", "main", "HEAD"]).unwrap();
        let stamp = f.conflict();
        let target = Fixture::head(&f.main);
        let result = resolve(&f.source, "main", &stamp, |_| Ok(())).unwrap();
        assert_eq!(result["conflicts"], json!(["a.txt"]));
        assert_eq!(checked_ref(&f.source, "MERGE_HEAD").unwrap(), target);
        assert_eq!(Fixture::head(&f.main), target);
        assert_eq!(
            git(&f.source, &["branch", "--show-current"]).unwrap(),
            "work/source"
        );
    }

    #[test]
    fn recovery_preserves_ignored_files_and_file_directory_collisions() {
        for (local, incoming) in [
            ("hidden", "hidden"),
            ("hidden", "hidden/entry"),
            ("hidden/entry", "hidden"),
        ] {
            let f = Fixture::new();
            Fixture::commit(&f.source, ".gitignore", "hidden\n");
            let tracked = f.main.join(incoming);
            fs::create_dir_all(tracked.parent().unwrap()).unwrap();
            Fixture::commit(&f.main, incoming, "incoming\n");
            let local = f.source.join(local);
            fs::create_dir_all(local.parent().unwrap()).unwrap();
            fs::write(&local, "local draft\n").unwrap();
            let stamp = f.conflict();
            let source = Fixture::head(&f.source);
            let target = Fixture::head(&f.main);
            let error = resolve(&f.source, "main", &stamp, |_| Ok(())).unwrap_err();
            assert!(error.to_string().contains("üzerine yazabilir"), "{error}");
            assert_eq!(fs::read_to_string(local).unwrap(), "local draft\n");
            assert!(!busy_operation(&f.source).unwrap());
            assert_eq!(Fixture::head(&f.source), source);
            assert_eq!(Fixture::head(&f.main), target);
        }
    }

    #[test]
    fn divergent_target_merges_both_changes_and_keeps_source_history() {
        let f = Fixture::new();
        Fixture::commit(&f.main, "b.txt", "target change\n");
        Fixture::commit(&f.source, "a.txt", "source change\n");
        let source = Fixture::head(&f.source);
        let target = Fixture::head(&f.main);
        let result = f.merge("main").unwrap();
        assert_eq!(result["merged"], true);
        let parents = git(&f.main, &["show", "-s", "--format=%P", "HEAD"]).unwrap();
        assert_eq!(
            parents.split_whitespace().collect::<Vec<_>>(),
            vec![target.as_str(), source.as_str()]
        );
        assert_eq!(Fixture::head(&f.source), source);
        assert_eq!(
            fs::read_to_string(f.main.join("a.txt")).unwrap(),
            "source change\n"
        );
        assert_eq!(
            fs::read_to_string(f.main.join("b.txt")).unwrap(),
            "target change\n"
        );
        f.clean_helpers();
    }

    #[test]
    fn unopened_target_updates_only_its_reference() {
        let f = Fixture::new();
        git(&f.main, &["branch", "release/test"]).unwrap();
        let main = Fixture::head(&f.main);
        Fixture::commit(&f.source, "a.txt", "new\n");
        assert!(inspect(&f.source, "release/test")
            .unwrap()
            .destination
            .is_none());
        assert_eq!(f.merge("release/test").unwrap()["merged"], true);
        assert_eq!(
            git(&f.main, &["rev-parse", "release/test"]).unwrap(),
            Fixture::head(&f.source)
        );
        assert_eq!(Fixture::head(&f.main), main);
        assert_eq!(fs::read_to_string(f.main.join("a.txt")).unwrap(), "base\n");
        f.clean_helpers();
    }

    #[test]
    fn conflicts_leave_both_real_worktrees_and_refs_unchanged() {
        let f = Fixture::new();
        Fixture::commit(&f.main, "a.txt", "target\n");
        Fixture::commit(&f.source, "a.txt", "source\n");
        let source = Fixture::head(&f.source);
        let target = Fixture::head(&f.main);
        let result = f.merge("main").unwrap();
        assert_eq!(result["merged"], false);
        assert_eq!(result["conflicts"], json!(["a.txt"]));
        assert_eq!(Fixture::head(&f.source), source);
        assert_eq!(Fixture::head(&f.main), target);
        assert!(git(&f.main, &["status", "--porcelain"]).unwrap().is_empty());
        assert!(git(&f.source, &["status", "--porcelain"])
            .unwrap()
            .is_empty());
        assert!(!busy_operation(&f.main).unwrap());
        assert!(!busy_operation(&f.source).unwrap());
        f.clean_helpers();
    }

    #[test]
    fn dirty_source_target_and_stale_preview_are_rejected() {
        let f = Fixture::new();
        Fixture::commit(&f.source, "a.txt", "source\n");
        let plan = inspect(&f.source, "main").unwrap();
        fs::write(f.main.join("local.txt"), "preserve").unwrap();
        assert!(inspect(&f.source, "main")
            .err()
            .unwrap()
            .to_string()
            .contains("Hedef dalın"));
        fs::remove_file(f.main.join("local.txt")).unwrap();
        fs::write(f.source.join("a.txt"), "draft").unwrap();
        assert!(inspect(&f.source, "main")
            .err()
            .unwrap()
            .to_string()
            .contains("bu oturumdaki"));
        fs::write(f.source.join("a.txt"), "source\n").unwrap();
        Fixture::commit(&f.source, "a.txt", "second source change\n");
        assert!(
            apply(&f.source, "main", &plan.stamp, &f.temporary, |_| Ok(()))
                .unwrap_err()
                .to_string()
                .contains("Git durumu değişti")
        );
        assert_eq!(fs::read_to_string(f.main.join("a.txt")).unwrap(), "base\n");
        f.clean_helpers();
    }

    #[test]
    fn rejected_merge_hook_is_honored_and_target_is_preserved() {
        let f = Fixture::new();
        Fixture::commit(&f.main, "b.txt", "target\n");
        Fixture::commit(&f.source, "a.txt", "source\n");
        fs::write(f.base.join("hooks/pre-merge-commit"), "#!/bin/sh\nexit 1\n").unwrap();
        let source = Fixture::head(&f.source);
        let target = Fixture::head(&f.main);
        assert!(f.merge("main").is_err());
        assert_eq!(Fixture::head(&f.source), source);
        assert_eq!(Fixture::head(&f.main), target);
        f.clean_helpers();
    }

    #[test]
    fn ignored_target_files_are_not_overwritten() {
        let f = Fixture::new();
        Fixture::commit(&f.main, ".gitignore", "local.txt\n");
        git(&f.source, &["merge", "--ff-only", "main"]).unwrap();
        fs::write(f.source.join("local.txt"), "tracked incoming").unwrap();
        git(&f.source, &["add", "-f", "--", "local.txt"]).unwrap();
        git(&f.source, &["commit", "-m", "Synthetic incoming file"]).unwrap();
        fs::write(f.main.join("local.txt"), "preserve local file").unwrap();
        let target = Fixture::head(&f.main);
        assert!(f.merge("main").is_err());
        assert_eq!(Fixture::head(&f.main), target);
        assert_eq!(
            fs::read_to_string(f.main.join("local.txt")).unwrap(),
            "preserve local file"
        );
        f.clean_helpers();
    }
    #[test]
    fn rejected_checkout_hook_does_not_leave_a_temporary_worktree() {
        let f = Fixture::new();
        Fixture::commit(&f.source, "a.txt", "source change\n");
        fs::write(f.base.join("hooks/post-checkout"), "#!/bin/sh\nexit 1\n").unwrap();
        let target = Fixture::head(&f.main);
        let source = Fixture::head(&f.source);
        assert!(f.merge("main").is_err());
        assert_eq!(Fixture::head(&f.main), target);
        assert_eq!(Fixture::head(&f.source), source);
        f.clean_helpers();
    }

    #[test]
    fn moving_target_after_preview_is_rejected_without_switching_source() {
        let f = Fixture::new();
        Fixture::commit(&f.source, "a.txt", "source change\n");
        let plan = inspect(&f.source, "main").unwrap();
        Fixture::commit(&f.main, "b.txt", "new target commit\n");
        let target = Fixture::head(&f.main);
        let source = Fixture::head(&f.source);
        assert!(apply(&f.source, "main", &plan.stamp, &f.temporary, |_| Ok(())).is_err());
        assert_eq!(Fixture::head(&f.main), target);
        assert_eq!(Fixture::head(&f.source), source);
        f.clean_helpers();
    }
}
