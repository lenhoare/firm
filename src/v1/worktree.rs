//! Git worktree isolation. Each attempt gets its own branch and working directory, so
//! parallel agents cannot collide and a bad attempt is discarded by deleting a branch.
//!
//! The operator's own branch is never touched: a run works on an integration branch
//! created from the base commit, and attempts branch from the integration tip so that
//! dependent tasks build on work already merged.

use anyhow::{Context, Result, bail, ensure};
use std::path::{Path, PathBuf};

/// Run git and return stdout, failing with git's own stderr so errors stay diagnosable.
pub async fn git(cwd: &Path, args: &[&str]) -> Result<String> {
    let output = tokio::process::Command::new("git")
        .args(args)
        .current_dir(cwd)
        .kill_on_drop(true)
        .output()
        .await
        .with_context(|| format!("Could not run git {}", args.join(" ")))?;
    if !output.status.success() {
        bail!(
            "git {} failed ({}): {}",
            args.join(" "),
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

#[derive(Debug)]
pub struct Worktrees {
    /// The operator's repository — the configured workspace.
    repo: PathBuf,
    /// Where attempt worktrees are created, outside the repository.
    root: PathBuf,
    integration_branch: String,
    integration_path: PathBuf,
}

impl Worktrees {
    /// Prepare a run: verify the workspace is a clean git repository, record the base
    /// commit, and create the integration branch and its worktree.
    pub async fn create(repo: &Path, root: &Path, run_id: &str) -> Result<(Self, String)> {
        let inside = git(repo, &["rev-parse", "--is-inside-work-tree"])
            .await
            .context("The v1 workspace must be a git repository")?;
        ensure!(inside == "true", "The workspace is not a git work tree");
        // A workspace nested inside a larger repository would silently isolate that outer
        // repository instead, handing agents the whole enclosing project to edit. Refuse.
        let toplevel = git(repo, &["rev-parse", "--show-toplevel"]).await?;
        let toplevel = std::fs::canonicalize(&toplevel)?;
        let workspace = std::fs::canonicalize(repo)?;
        ensure!(
            toplevel == workspace,
            "The workspace must be the root of its own git repository.\n\
             {} belongs to the repository at {}.\n\
             Give the target project its own repository, or point `workspace` at that root.",
            workspace.display(),
            toplevel.display()
        );
        let dirty = git(repo, &["status", "--porcelain"]).await?;
        ensure!(
            dirty.is_empty(),
            "Commit or stash the workspace first; a run needs a clean tree.\n{dirty}"
        );
        let base_commit = git(repo, &["rev-parse", "HEAD"])
            .await
            .context("The workspace has no commits yet")?;

        let short = &run_id[..8];
        let integration_branch = format!("firm/run-{short}");
        let root = root.join(format!("run-{short}"));
        std::fs::create_dir_all(&root)?;
        let integration_path = root.join("integration");
        git(
            repo,
            &[
                "worktree",
                "add",
                "-b",
                &integration_branch,
                &integration_path.to_string_lossy(),
                &base_commit,
            ],
        )
        .await
        .context("Could not create the integration worktree")?;
        Ok((
            Self {
                repo: repo.to_path_buf(),
                root,
                integration_branch,
                integration_path,
            },
            base_commit,
        ))
    }

    /// Reattach to a run that already exists, so it can be continued.
    ///
    /// Its integration worktree may have been pruned since; the branch is what matters and
    /// the working directory is recreated from it. The workspace must still be clean —
    /// resuming into a tree someone has edited would merge onto an unexpected base.
    pub async fn attach(
        repo: &Path,
        root: &Path,
        run_id: &str,
        integration_branch: &str,
    ) -> Result<Self> {
        let dirty = git(repo, &["status", "--porcelain"]).await?;
        ensure!(
            dirty.is_empty(),
            "Commit or stash the workspace before resuming.\n{dirty}"
        );
        git(repo, &["rev-parse", "--verify", integration_branch])
            .await
            .with_context(|| format!("The run's branch {integration_branch} no longer exists"))?;

        let short = &run_id[..8];
        let root = root.join(format!("run-{short}"));
        std::fs::create_dir_all(&root)?;
        let integration_path = root.join("integration");
        if !integration_path.join(".git").exists() {
            // Pruned, or never created here. Recreate it from the branch.
            let _ = git(repo, &["worktree", "prune"]).await;
            git(
                repo,
                &[
                    "worktree",
                    "add",
                    &integration_path.to_string_lossy(),
                    integration_branch,
                ],
            )
            .await
            .context("Could not recreate the integration worktree")?;
        }
        Ok(Self {
            repo: repo.to_path_buf(),
            root,
            integration_branch: integration_branch.to_string(),
            integration_path,
        })
    }

    /// Rescue work stranded by an unclean stop.
    ///
    /// A clean stop commits an agent's work before recording the interruption. A hard kill
    /// — SIGKILL, a lost terminal, a power cut — does not, and leaves the files sitting
    /// uncommitted in an attempt worktree that nothing will ever look at again. Committing
    /// them to the attempt's own branch means an unclean stop loses no more than a clean
    /// one. The directories are then cleared away, having served their purpose.
    ///
    /// Except those named by `keep`: worktrees an interrupted attempt may still be
    /// continued in. Their work is committed like any other, but the directory stays.
    /// Removing it is what silently turned every resume back into a fresh start.
    pub async fn salvage_abandoned(&self, keep: &[String]) -> Result<Vec<(String, Vec<String>)>> {
        let mut rescued = Vec::new();
        let Ok(entries) = std::fs::read_dir(&self.root) else {
            return Ok(rescued);
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();
            if !name.starts_with("attempt-") || !path.join(".git").exists() {
                continue;
            }
            let resumable = keep.iter().any(|k| Path::new(k) == path);
            let dirty = git(&path, &["status", "--porcelain"]).await.unwrap_or_default();
            if !dirty.is_empty() {
                let branch = git(&path, &["rev-parse", "--abbrev-ref", "HEAD"])
                    .await
                    .unwrap_or_else(|_| name.clone());
                let attempt = Attempt {
                    branch: branch.clone(),
                    path: path.clone(),
                    base_commit: String::new(),
                };
                if attempt
                    .commit("firm: work recovered after an unclean stop")
                    .await
                    .unwrap_or(false)
                {
                    let files = git(&path, &["show", "--name-only", "--format=", "HEAD"])
                        .await
                        .unwrap_or_default();
                    rescued.push((
                        branch,
                        files.lines().map(str::to_string).collect::<Vec<_>>(),
                    ));
                }
            }
            // Otherwise the directory has served its purpose; the branch carries the work.
            if !resumable {
                let _ = git(
                    &self.repo,
                    &["worktree", "remove", "--force", &path.to_string_lossy()],
                )
                .await;
            }
        }
        let _ = git(&self.repo, &["worktree", "prune"]).await;
        Ok(rescued)
    }

    pub fn integration_path(&self) -> &Path {
        &self.integration_path
    }
    pub fn integration_branch(&self) -> &str {
        &self.integration_branch
    }

    /// The current integration tip. Attempts branch from here, not from the run's base
    /// commit, so each new attempt already contains everything merged so far.
    pub async fn integration_tip(&self) -> Result<String> {
        git(&self.integration_path, &["rev-parse", "HEAD"]).await
    }

    /// Create an isolated worktree for one attempt, branched from `from_commit`.
    pub async fn create_attempt(&self, attempt_id: &str, from_commit: &str) -> Result<Attempt> {
        let short = &attempt_id[..8];
        let branch = attempt_branch(attempt_id);
        let path = self.root.join(format!("attempt-{short}"));
        git(
            &self.repo,
            &[
                "worktree",
                "add",
                "-b",
                &branch,
                &path.to_string_lossy(),
                from_commit,
            ],
        )
        .await
        .context("Could not create the attempt worktree")?;
        Ok(Attempt {
            branch,
            path,
            base_commit: from_commit.to_string(),
        })
    }

    /// Merge a verified attempt into the integration branch. Callers must serialise this;
    /// a conflict is aborted cleanly and reported rather than left half-applied.
    pub async fn merge(&self, attempt: &Attempt, message: &str) -> Result<()> {
        let result = git(
            &self.integration_path,
            &["merge", "--no-ff", "-m", message, &attempt.branch],
        )
        .await;
        if result.is_err() {
            // Leave the integration branch exactly as it was.
            let _ = git(&self.integration_path, &["merge", "--abort"]).await;
        }
        result.map(|_| ())
    }

    /// Undo the most recent merge commit. Used when an attempt passed on its own but
    /// broke the integration branch, so the branch never keeps work that fails together.
    pub async fn revert_last_merge(&self) -> Result<()> {
        git(&self.integration_path, &["reset", "--hard", "HEAD~1"])
            .await
            .map(|_| ())
    }

    /// Remove an attempt's working directory. The branch is kept: it is the evidence of
    /// what the agent actually wrote, including for attempts that were rejected.
    pub async fn discard(&self, attempt: &Attempt) -> Result<()> {
        git(
            &self.repo,
            &[
                "worktree",
                "remove",
                "--force",
                &attempt.path.to_string_lossy(),
            ],
        )
        .await
        .map(|_| ())
    }
}

/// The branch an attempt will use, known before its worktree exists so the board can
/// record a durable reservation before anything external happens.
pub fn attempt_branch(attempt_id: &str) -> String {
    format!("firm/attempt-{}", &attempt_id[..8])
}

pub struct Attempt {
    pub branch: String,
    pub path: PathBuf,
    pub base_commit: String,
}

impl Attempt {
    /// Commit whatever the agent left behind. Returns false when it changed nothing —
    /// which is a real and important outcome: v0's first live trial had a worker announce
    /// an implementation while leaving the file byte-for-byte unchanged.
    pub async fn commit(&self, message: &str) -> Result<bool> {
        git(&self.path, &["add", "-A"]).await?;
        if git(&self.path, &["status", "--porcelain"]).await?.is_empty() {
            return Ok(false);
        }
        git(
            &self.path,
            &[
                "-c",
                "user.name=Firm",
                "-c",
                "user.email=firm@localhost",
                "commit",
                "--no-verify",
                "-m",
                message,
            ],
        )
        .await?;
        Ok(true)
    }

    /// The whole change this attempt made, for a reviewer to read. Bounded: a reviewer
    /// prompt is a model call, and an unbounded diff is an unbounded bill.
    pub async fn diff(&self, limit: usize) -> Result<String> {
        let text = git(
            &self.path,
            &["diff", "--unified=3", &self.base_commit, "HEAD"],
        )
        .await
        .unwrap_or_default();
        if text.len() <= limit {
            return Ok(text);
        }
        let mut end = limit;
        while end > 0 && !text.is_char_boundary(end) {
            end -= 1;
        }
        Ok(format!("{}\n[diff truncated]", &text[..end]))
    }

    pub async fn files_changed(&self) -> Result<Vec<String>> {
        let diff = git(
            &self.path,
            &["diff", "--name-only", &self.base_commit, "HEAD"],
        )
        .await?;
        Ok(diff.lines().map(str::to_string).collect())
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;

    /// A throwaway git repository with one commit, used by the worktree and engine tests.
    pub async fn repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path();
        git(path, &["init", "-q", "-b", "main"]).await.unwrap();
        git(path, &["config", "user.name", "Test"]).await.unwrap();
        git(path, &["config", "user.email", "test@localhost"])
            .await
            .unwrap();
        std::fs::write(path.join("README.md"), "base\n").unwrap();
        git(path, &["add", "-A"]).await.unwrap();
        git(path, &["commit", "-q", "-m", "base"]).await.unwrap();
        dir
    }

    #[tokio::test]
    async fn a_dirty_workspace_is_refused() {
        let repo_dir = repo().await;
        std::fs::write(repo_dir.path().join("scratch.txt"), "uncommitted").unwrap();
        let root = tempfile::tempdir().unwrap();
        let error = Worktrees::create(repo_dir.path(), root.path(), &uuid::Uuid::new_v4().to_string())
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("clean tree"), "{error}");
    }

    #[tokio::test]
    async fn a_workspace_nested_in_a_larger_repository_is_refused() {
        // Exactly the shipped examples/taskboard situation: a subdirectory of a bigger
        // repo. Isolating it would hand agents the whole enclosing project.
        let outer = repo().await;
        let nested = outer.path().join("subproject");
        std::fs::create_dir(&nested).unwrap();
        std::fs::write(nested.join("lib.rs"), "// work here\n").unwrap();
        git(outer.path(), &["add", "-A"]).await.unwrap();
        git(outer.path(), &["commit", "-q", "-m", "add subproject"])
            .await
            .unwrap();

        let root = tempfile::tempdir().unwrap();
        let error = Worktrees::create(&nested, root.path(), &uuid::Uuid::new_v4().to_string())
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("root of its own git repository"), "{error}");
    }

    #[tokio::test]
    async fn attempts_are_isolated_and_merge_onto_the_integration_branch() {
        let repo_dir = repo().await;
        let root = tempfile::tempdir().unwrap();
        let run_id = uuid::Uuid::new_v4().to_string();
        let (trees, base) = Worktrees::create(repo_dir.path(), root.path(), &run_id)
            .await
            .unwrap();

        // Two attempts branched from the same tip, each writing a different file.
        let one = trees
            .create_attempt(&uuid::Uuid::new_v4().to_string(), &base)
            .await
            .unwrap();
        let two = trees
            .create_attempt(&uuid::Uuid::new_v4().to_string(), &base)
            .await
            .unwrap();
        std::fs::write(one.path.join("one.txt"), "1\n").unwrap();
        std::fs::write(two.path.join("two.txt"), "2\n").unwrap();
        assert!(!one.path.join("two.txt").exists(), "attempts cannot see each other");

        assert!(one.commit("attempt one").await.unwrap());
        assert_eq!(one.files_changed().await.unwrap(), ["one.txt"]);
        assert!(two.commit("attempt two").await.unwrap());

        trees.merge(&one, "merge one").await.unwrap();
        trees.merge(&two, "merge two").await.unwrap();
        assert!(trees.integration_path().join("one.txt").exists());
        assert!(trees.integration_path().join("two.txt").exists());

        // The operator's own branch is untouched throughout.
        assert!(!repo_dir.path().join("one.txt").exists());
        assert_eq!(
            git(repo_dir.path(), &["rev-parse", "HEAD"]).await.unwrap(),
            base
        );

        trees.discard(&one).await.unwrap();
        assert!(!one.path.exists());
    }

    #[tokio::test]
    async fn work_stranded_by_an_unclean_stop_is_recovered_onto_its_branch() {
        // A hard kill gives the controller no chance to commit, so an agent's files sit
        // uncommitted in a worktree nothing will look at again. They must not be lost.
        let repo_dir = repo().await;
        let root = tempfile::tempdir().unwrap();
        let (trees, base) = Worktrees::create(
            repo_dir.path(),
            root.path(),
            &uuid::Uuid::new_v4().to_string(),
        )
        .await
        .unwrap();
        let attempt = trees
            .create_attempt(&uuid::Uuid::new_v4().to_string(), &base)
            .await
            .unwrap();
        // The agent wrote this and then everything died before any commit.
        std::fs::write(attempt.path.join("half-done.rs"), "fn work() {}\n").unwrap();

        let rescued = trees.salvage_abandoned(&[]).await.unwrap();
        assert_eq!(rescued.len(), 1, "the stranded work was found");
        assert!(rescued[0].1.iter().any(|f| f == "half-done.rs"), "{rescued:?}");

        // It is on the attempt's branch, and the worktree is gone.
        let listing = git(
            repo_dir.path(),
            &["ls-tree", "-r", "--name-only", &attempt.branch],
        )
        .await
        .unwrap();
        assert!(listing.contains("half-done.rs"), "{listing}");
        assert!(!attempt.path.exists(), "the spent worktree is cleaned up");

        // Nothing to rescue the second time, and it does not fail.
        assert!(trees.salvage_abandoned(&[]).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn salvage_keeps_the_worktree_an_interrupted_attempt_can_continue_in() {
        // Salvage used to remove every attempt directory, which quietly defeated resuming:
        // by the time a task was dispatched its worktree was gone, so continuing an
        // interrupted attempt always fell back to starting again. Found in a live run.
        let repo_dir = repo().await;
        let root = tempfile::tempdir().unwrap();
        let (trees, base) = Worktrees::create(
            repo_dir.path(),
            root.path(),
            &uuid::Uuid::new_v4().to_string(),
        )
        .await
        .unwrap();
        let carry_on = trees
            .create_attempt(&uuid::Uuid::new_v4().to_string(), &base)
            .await
            .unwrap();
        let spent = trees
            .create_attempt(&uuid::Uuid::new_v4().to_string(), &base)
            .await
            .unwrap();
        std::fs::write(carry_on.path.join("half-done.rs"), "fn work() {}\n").unwrap();
        std::fs::write(spent.path.join("abandoned.rs"), "fn gone() {}\n").unwrap();

        let keep = vec![carry_on.path.to_string_lossy().to_string()];
        let rescued = trees.salvage_abandoned(&keep).await.unwrap();
        assert_eq!(rescued.len(), 2, "both had uncommitted work to commit: {rescued:?}");

        assert!(
            carry_on.path.exists(),
            "the interrupted attempt keeps somewhere to carry on"
        );
        assert!(!spent.path.exists(), "the finished one is still cleaned up");

        // Keeping the directory must not mean skipping the commit: the work is safe either way.
        let listing = git(
            repo_dir.path(),
            &["ls-tree", "-r", "--name-only", &carry_on.branch],
        )
        .await
        .unwrap();
        assert!(listing.contains("half-done.rs"), "{listing}");
    }

    #[tokio::test]
    async fn an_agent_that_changes_nothing_is_reported_honestly() {
        let repo_dir = repo().await;
        let root = tempfile::tempdir().unwrap();
        let (trees, base) = Worktrees::create(
            repo_dir.path(),
            root.path(),
            &uuid::Uuid::new_v4().to_string(),
        )
        .await
        .unwrap();
        let attempt = trees
            .create_attempt(&uuid::Uuid::new_v4().to_string(), &base)
            .await
            .unwrap();
        assert!(!attempt.commit("nothing").await.unwrap());
        assert!(attempt.files_changed().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn conflicting_merges_abort_and_leave_integration_intact() {
        let repo_dir = repo().await;
        let root = tempfile::tempdir().unwrap();
        let (trees, base) = Worktrees::create(
            repo_dir.path(),
            root.path(),
            &uuid::Uuid::new_v4().to_string(),
        )
        .await
        .unwrap();
        let one = trees.create_attempt(&uuid::Uuid::new_v4().to_string(), &base).await.unwrap();
        let two = trees.create_attempt(&uuid::Uuid::new_v4().to_string(), &base).await.unwrap();
        std::fs::write(one.path.join("same.txt"), "from one\n").unwrap();
        std::fs::write(two.path.join("same.txt"), "from two\n").unwrap();
        one.commit("one").await.unwrap();
        two.commit("two").await.unwrap();

        trees.merge(&one, "merge one").await.unwrap();
        let tip = trees.integration_tip().await.unwrap();
        assert!(trees.merge(&two, "merge two").await.is_err(), "conflict is reported");
        assert_eq!(
            trees.integration_tip().await.unwrap(),
            tip,
            "a conflicted merge leaves the integration branch untouched"
        );
        assert!(
            git(trees.integration_path(), &["status", "--porcelain"])
                .await
                .unwrap()
                .is_empty(),
            "the aborted merge leaves no conflict markers behind"
        );
    }
}
