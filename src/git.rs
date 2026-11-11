use crate::core::*;
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::{
    collections::{BTreeSet, HashMap},
    fs,
    path::{Path, PathBuf},
    sync::Arc,
};

const CHANGED: &str = "Git durumu değişti; yenileyip yeniden dene";

pub(crate) fn worktree_root(directory: &Path) -> Result<PathBuf> {
    Ok(dunce::canonicalize(git(
        directory,
        &["rev-parse", "--show-toplevel"],
    )?)?)
}

#[derive(PartialEq)]
struct Checkout {
    reference: Option<String>,
    head: Option<String>,
}

fn checkout(root: &Path) -> Checkout {
    Checkout {
        reference: git(root, &["symbolic-ref", "--quiet", "HEAD"]).ok(),
        head: git(root, &["rev-parse", "--verify", "HEAD"]).ok(),
    }
}

fn snapshot(root: &Path) -> Result<(Checkout, String, String)> {
    let current = checkout(root);
    let status = git(root, &["status", "--porcelain=v1", "-z"])?;
    let staged = git(root, &["diff", "--cached", "--binary"])?;
    if checkout(root) != current {
        bail!(CHANGED)
    }
    let stamp = hash(
        json!([current.reference, current.head, status, staged])
            .to_string()
            .as_bytes(),
    );
    Ok((current, status, stamp))
}

fn status_entries(raw: &str) -> impl Iterator<Item = (&str, &str, Option<&str>)> {
    let mut parts = raw.split('\0');
    std::iter::from_fn(move || {
        let entry = parts.find(|entry| entry.len() >= 4)?;
        let xy = &entry[..2];
        let name = &entry[3..];
        let old = if xy.contains('R') || xy.contains('C') {
            parts.next()
        } else {
            None
        };
        Some((xy, name, old))
    })
}

fn state(root: &Path) -> Result<Value> {
    let (current, raw, stamp) = snapshot(root)?;
    let files = status_entries(&raw)
        .map(|(xy, name, old)| json!({"path":name,"oldPath":old,"index":&xy[..1],"work":&xy[1..],"untracked":xy=="??","conflict":(["DD","AU","UD","UA","DU","AA","UU"].contains(&xy))}))
        .collect::<Vec<_>>();
    let branch = current
        .reference
        .as_deref()
        .and_then(|r| r.strip_prefix("refs/heads/"))
        .unwrap_or("HEAD");
    let remotes = git(root, &["remote"])
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect::<Vec<_>>();
    let upstream = git(
        root,
        &[
            "rev-parse",
            "--abbrev-ref",
            "--symbolic-full-name",
            "@{upstream}",
        ],
    )
    .ok();
    let branches = git(
        root,
        &[
            "for-each-ref",
            "--format=%(refname:short)%09%(objectname)",
            "refs/heads",
            "refs/remotes",
        ],
    )
    .unwrap_or_default()
    .lines()
    .map(|l| {
        let (n, h) = l.split_once('\t').unwrap_or((l, ""));
        json!({"name":n,"hash":h})
    })
    .collect::<Vec<_>>();
    Ok(
        json!({"available":true,"branch":branch,"head":current.head,"files":files,"remotes":remotes,"upstream":upstream,"branches":branches,"stamp":stamp}),
    )
}
pub fn dispatch(core: &Arc<Core>, action: &str, args: &Value) -> Result<Value> {
    let directory = core.root(args)?;
    let root = match worktree_root(&directory) {
        Ok(root) => root,
        Err(_) if action == "state" => return Ok(json!({"available":false})),
        Err(error) => return Err(error),
    };
    if action == "state" {
        return state(&root);
    }
    if action == "history" {
        let selector = args["branch"].as_str().unwrap_or("--all");
        if selector.starts_with('-') && selector != "--all" {
            bail!("Dal geçersiz")
        };
        let out = git(
            &root,
            &[
                "log",
                selector,
                "--topo-order",
                "--date=iso-strict",
                "--format=%H%x1f%P%x1f%D%x1f%s%x1f%an%x1f%aI",
                "-150",
            ],
        )?;
        return Ok(json!(out.lines().map(|l|{let p=l.split('\u{1f}').collect::<Vec<_>>();json!({"hash":p.first(),"parents":p.get(1).unwrap_or(&"").split_whitespace().collect::<Vec<_>>(),"refs":p.get(2),"subject":p.get(3),"author":p.get(4),"date":p.get(5)})}).collect::<Vec<_>>()));
    }
    if action == "commit_files" || action == "commit_diff" {
        let commit = checked_ref(&root, string(args, "commit")?)?;
        let from = if let Some(from) = args["from"].as_str() {
            Some(checked_ref(&root, from)?)
        } else {
            let parents = git(&root, &["rev-list", "--parents", "-n", "1", &commit])?;
            parents.split_whitespace().nth(1).map(str::to_owned)
        };
        let mut cmd = vec!["--literal-pathspecs"];
        if from.is_some() {
            cmd.push("diff");
        } else {
            cmd.extend(["diff-tree", "--root", "--no-commit-id", "-r"]);
        }
        cmd.extend([
            "--no-renames",
            "--no-ext-diff",
            "--no-textconv",
            "--no-color",
        ]);
        if action == "commit_files" {
            cmd.extend(["--name-status", "-z"]);
        } else {
            cmd.push("--patch");
        }
        cmd.extend(from.as_deref());
        cmd.extend([&commit, "--"]);
        if action == "commit_files" {
            let out = git(&root, &cmd)?;
            let p = out.split('\0').collect::<Vec<_>>();
            return Ok(json!(p
                .chunks(2)
                .filter(|p| p.len() == 2)
                .map(|p| json!({"status":p[0],"path":p[1]}))
                .collect::<Vec<_>>()));
        }
        cmd.push(string(args, "path")?);
        return Ok(json!({"patch":git(&root,&cmd)?}));
    }
    if action == "diff" {
        let file = string(args, "path")?;
        crate::core::safe_path(&root, file)?;
        let staged = args["staged"] == true;
        let mut cmd = vec![
            "--literal-pathspecs",
            "diff",
            "--no-ext-diff",
            "--no-textconv",
            "--no-color",
        ];
        if staged {
            cmd.push("--cached")
        }
        cmd.extend(["--", file]);
        let mut patch = git(&root, &cmd)?;
        if patch.is_empty() && !staged {
            let p = safe_path(&root, file)?;
            if p.is_file() {
                patch = String::from_utf8(fs::read(p)?).unwrap_or_else(|_| "İkili dosya".into())
            }
        }
        return Ok(json!({"patch":patch}));
    }
    if action == "conflict_details" {
        let bytes = read_optional(&safe_path(&root, string(args, "path")?)?)?;
        let digest = bytes.as_deref().map(hash);
        let text = bytes.as_deref().and_then(|bytes| {
            (bytes.len() <= 5 * 1024 * 1024)
                .then(|| std::str::from_utf8(bytes).ok())
                .flatten()
        });
        return Ok(json!({"hash":digest,"text":text}));
    }
    let _guard = core.operations.lock().unwrap();
    let (current, status, stamp) = snapshot(&root)?;
    if string(args, "stamp")? != stamp {
        bail!(CHANGED)
    }
    if let Some(sid) = args["sessionId"].as_str() {
        let session = core.session(sid)?;
        if session["workspaceKind"] == "worktree" && dunce::canonicalize(directory)? == root {
            let project = core.project(string(&session, "projectId")?)?;
            let project_dir = dunce::canonicalize(string(&project, "directory")?)?;
            if worktree_root(&project_dir).is_ok_and(|repository| project_dir != repository) {
                bail!("Bu eski oturumun Git çalışma alanı hatalı konumda. Proje klasöründen yeni bir oturum aç.")
            }
        }
    }
    let result = match action {
        "stage" => {
            let unstage = args["unstage"] == true;
            let names = args["files"].as_array().context("Dosyaları seç")?;
            if names.is_empty() {
                bail!("Dosya seçilmedi")
            };
            let operation: &[&str] = if unstage {
                if git(&root, &["rev-parse", "--verify", "HEAD"]).is_ok() {
                    &["reset", "HEAD", "--"]
                } else {
                    &["rm", "--cached", "--"]
                }
            } else {
                &["add", "--"]
            };
            let renames = if unstage {
                status_entries(&status)
                    .filter_map(|(xy, name, old)| xy.starts_with('R').then_some((name, old?)))
                    .collect::<HashMap<_, _>>()
            } else {
                HashMap::new()
            };
            let mut cmd = vec!["--literal-pathspecs".to_string()];
            cmd.extend(operation.iter().map(|arg| (*arg).to_string()));
            let mut paths = BTreeSet::new();
            for name in names {
                let name = name.as_str().context("Dosya adı geçersiz")?;
                paths.insert(name);
                paths.extend(renames.get(name).copied());
            }
            for path in paths {
                safe_path(&root, path)?;
                cmd.push(path.into());
            }
            let o = run(&root, "git", &cmd)?;
            if !o.status.success() {
                bail!("{}", String::from_utf8_lossy(&o.stderr))
            };
            String::from_utf8_lossy(&o.stdout).to_string()
        }
        "commit" => {
            let message = string(args, "message")?.trim();
            if message.is_empty() {
                bail!("Commit mesajı yaz")
            };
            if git(&root, &["diff", "--cached", "--name-only"])?.is_empty() {
                bail!("Hazırlanmış dosya yok")
            };
            if checkout(&root) != current {
                bail!(CHANGED)
            }
            git(&root, &["commit", "-m", message])?
        }
        "fetch" => {
            let remote = remote(&root, args)?;
            git(&root, &["fetch", "--", &remote])?
        }
        "push" => {
            let remote = remote(&root, args)?;
            let branch = current
                .reference
                .as_deref()
                .context("Seçili Git dalı yok")?;
            if checkout(&root) != current {
                bail!(CHANGED)
            }
            git(
                &root,
                &[
                    "push",
                    "--set-upstream",
                    "--",
                    &remote,
                    &format!("{branch}:{branch}"),
                ],
            )?
        }
        "branch" => {
            let name = string(args, "name")?;
            git(&root, &["check-ref-format", "--branch", name])?;
            git(&root, &["switch", "-c", name])?
        }
        "switch" => {
            let name = string(args, "name")?;
            if name.starts_with('-') {
                bail!("Dal geçersiz")
            }
            git(&root, &["switch", name])?
        }
        "merge" => {
            let target = checked_ref(&root, string(args, "target")?)?;
            if !git(&root, &["status", "--porcelain"])?.is_empty() {
                bail!("Birleştirmeden önce değişiklikleri commit et")
            };
            git(&root, &["merge", "--no-edit", &target])?
        }
        "merge_continue" => git(&root, &["-c", "core.editor=true", "merge", "--continue"])?,
        "merge_abort" => git(&root, &["merge", "--abort"])?,
        "conflict_resolve" => {
            let name = string(args, "path")?;
            let p = safe_path(&root, name)?;
            let original = read_optional(&p)?;
            if json!(original.as_deref().map(hash)) != args["fileStamp"] {
                bail!("Çakışma dosyası değişti")
            };
            let stages = git(
                &root,
                &["--literal-pathspecs", "ls-files", "--unmerged", "--", name],
            )?;
            if stages.is_empty() {
                bail!("Çözülecek Git çakışması yok")
            }
            let chosen = match string(args, "choice")? {
                choice @ ("ours" | "theirs") => {
                    let stage = if choice == "ours" { "2" } else { "3" };
                    let exists = stages.lines().any(|line| {
                        line.split_once('\t')
                            .and_then(|(fields, _)| fields.split_whitespace().last())
                            == Some(stage)
                    });
                    exists
                        .then(|| git_bytes(&root, &["show", &format!(":{stage}:{name}")]))
                        .transpose()?
                }
                "manual" => Some(string(args, "text")?.as_bytes().to_vec()),
                _ => bail!("Çözüm seç"),
            };
            if let Some(original) = &original {
                crate::files::store(core, original)?;
            }
            crate::files::apply_bytes(&p, chosen.as_deref())?;
            match git(&root, &["--literal-pathspecs", "add", "--", name]) {
                Ok(output) => output,
                Err(error) => {
                    if read_optional(&p)? == chosen {
                        crate::files::apply_bytes(&p, original.as_deref())
                            .with_context(|| format!("Git hazırlama başarısız: {error}"))?;
                    }
                    return Err(error);
                }
            }
        }
        "pr" => {
            let title = string(args, "title")?;
            let base = string(args, "base")?;
            if base.starts_with('-') {
                bail!("Hedef dal geçersiz")
            };
            let o = run(
                &root,
                "gh",
                &[
                    "pr".into(),
                    "create".into(),
                    "--draft".into(),
                    "--title".into(),
                    title.into(),
                    "--body".into(),
                    args["body"].as_str().unwrap_or("").into(),
                    "--base".into(),
                    base.into(),
                ],
            )?;
            if !o.status.success() {
                bail!("{}", String::from_utf8_lossy(&o.stderr))
            };
            String::from_utf8_lossy(&o.stdout).to_string()
        }
        _ => bail!("Geçersiz Git işlemi"),
    };
    Ok(json!({"output":result,"state":state(&root)?}))
}
fn remote(root: &Path, args: &Value) -> Result<String> {
    let r = args["remote"].as_str().unwrap_or("origin");
    if !git(root, &["remote"])?.lines().any(|x| x == r) {
        bail!("Uzak depo bulunamadı")
    };
    Ok(r.into())
}
fn checked_ref(root: &Path, r: &str) -> Result<String> {
    if r.starts_with('-') || r.contains('\n') {
        bail!("Git kaydı geçersiz")
    };
    git(root, &["rev-parse", "--verify", &format!("{r}^{{commit}}")])
}
