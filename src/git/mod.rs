use anyhow::{Context, Result};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

pub struct GitRepo {
    root: PathBuf,
}

impl GitRepo {
    pub fn open(path: &str) -> Result<Self> {
        let root = std::fs::canonicalize(path)?;
        Ok(Self { root })
    }

    fn grit_dir(&self) -> PathBuf {
        self.root.join(".grit")
    }

    /// Create an isolated git worktree for an agent
    pub fn create_worktree(&self, agent_id: &str) -> Result<PathBuf> {
        let wt_path = self.grit_dir().join("worktrees").join(agent_id);
        let branch_name = format!("agent/{}", agent_id);

        if wt_path.exists() {
            anyhow::bail!("Worktree already exists at {}", wt_path.display());
        }

        std::fs::create_dir_all(wt_path.parent().unwrap())?;

        // Decide create-vs-reuse from a real ref existence check rather than
        // parsing localized "already exists" stderr. If the agent branch
        // survived a previous run, reuse it but warn loudly — the worktree will
        // resume on top of whatever commits that branch already carries.
        let branch_exists = Command::new("git")
            .args([
                "rev-parse",
                "--verify",
                "--quiet",
                &format!("refs/heads/{}", branch_name),
            ])
            .current_dir(&self.root)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);

        let output = if branch_exists {
            eprintln!(
                "  warn: branch {} already exists from a previous run; reusing it for agent {}",
                branch_name, agent_id
            );
            Command::new("git")
                .args([
                    "worktree",
                    "add",
                    "--",
                    &wt_path.to_string_lossy(),
                    &branch_name,
                ])
                .current_dir(&self.root)
                .output()
        } else {
            Command::new("git")
                .args([
                    "worktree",
                    "add",
                    "-b",
                    &branch_name,
                    "--",
                    &wt_path.to_string_lossy(),
                ])
                .current_dir(&self.root)
                .output()
        }
        .context("Failed to run git worktree add")?;

        if !output.status.success() {
            anyhow::bail!(
                "git worktree add failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }

        Ok(wt_path)
    }

    /// Remove a worktree for an agent.
    ///
    /// This does NOT delete the agent branch. Branch deletion is the caller's
    /// responsibility and must only happen once the branch has been merged —
    /// see `delete_agent_branch`. Deleting the branch here unconditionally used
    /// to orphan the agent's commit whenever the merge was skipped (issue #21).
    pub fn remove_worktree(&self, agent_id: &str) -> Result<()> {
        let wt_path = self.grit_dir().join("worktrees").join(agent_id);

        if !wt_path.exists() {
            anyhow::bail!("Worktree does not exist at {}", wt_path.display());
        }

        let output = Command::new("git")
            .args([
                "worktree",
                "remove",
                "--force",
                "--",
                &wt_path.to_string_lossy(),
            ])
            .current_dir(&self.root)
            .output()
            .context("Failed to run git worktree remove")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("git worktree remove failed: {}", stderr);
        }

        Ok(())
    }

    /// Delete the agent branch ref. Only safe to call after the branch has been
    /// merged, since `-D` discards the commit reachability the branch provided.
    pub fn delete_agent_branch(&self, agent_id: &str) -> Result<()> {
        let branch_name = format!("agent/{}", agent_id);
        let _ = Command::new("git")
            .args(["branch", "-D", "--", &branch_name])
            .current_dir(&self.root)
            .output();
        Ok(())
    }

    /// Merge an agent's worktree branch back into the current branch.
    /// Uses a file lock to serialize merges (git can't handle concurrent merges).
    pub fn merge_worktree(&self, agent_id: &str) -> Result<()> {
        let branch_name = format!("agent/{}", agent_id);
        let wt_path = self.grit_dir().join("worktrees").join(agent_id);

        if !wt_path.exists() {
            anyhow::bail!("Worktree does not exist for agent {}", agent_id);
        }

        // Commit any changes in the worktree
        let status_output = Command::new("git")
            .args(["status", "--porcelain"])
            .current_dir(&wt_path)
            .output()?;

        let status_str = String::from_utf8_lossy(&status_output.stdout);
        if !status_str.trim().is_empty() {
            let add_output = Command::new("git")
                .args(["add", "-A"])
                .current_dir(&wt_path)
                .output()
                .context("Failed to run git add in worktree")?;

            if !add_output.status.success() {
                anyhow::bail!(
                    "git add failed in worktree {}: {}",
                    wt_path.display(),
                    String::from_utf8_lossy(&add_output.stderr)
                );
            }

            let commit_output = Command::new("git")
                .args(["commit", "-m", &format!("grit: agent {} changes", agent_id)])
                .current_dir(&wt_path)
                .output()
                .context("Failed to run git commit in worktree")?;

            if !commit_output.status.success() {
                let stderr = String::from_utf8_lossy(&commit_output.stderr);
                // "nothing to commit" is OK (e.g. only untracked files that were gitignored)
                if !stderr.contains("nothing to commit") {
                    anyhow::bail!(
                        "git commit failed in worktree {}: {}",
                        wt_path.display(),
                        stderr
                    );
                }
            }
        }

        // Acquire merge lock (serialize all merges because git can't handle concurrent ones)
        let lock_path = self.grit_dir().join("merge.lock");
        let _lock = self.acquire_file_lock(&lock_path)?;

        // Get current branch (session branch or main)
        let current = self.current_branch()?;

        // Refuse to merge into a dirty main worktree. Running rebase/merge here
        // against uncommitted changes can leave the main repository in a broken
        // state (issue #21 reported a core.bare flip and a skipped, silent
        // merge). Bail loudly and leave the agent branch untouched so the work
        // is recoverable.
        // Only tracked changes (staged or modified) can interfere with the
        // merge; untracked files — such as the `.gitignore` `grit init` writes —
        // are harmless, so exclude them from the guard.
        let main_status = Command::new("git")
            .args(["status", "--porcelain", "--untracked-files=no"])
            .current_dir(&self.root)
            .output()
            .context("Failed to check main worktree status")?;
        if !String::from_utf8_lossy(&main_status.stdout)
            .trim()
            .is_empty()
        {
            anyhow::bail!(
                "main worktree at {} has uncommitted changes; refusing to merge \
                 agent/{} to avoid corrupting the repository. Commit or stash them \
                 and run `grit done` again. The branch agent/{} is preserved.",
                self.root.display(),
                agent_id,
                agent_id
            );
        }

        // Rebase the agent branch on top of the current branch before merging so
        // the agent's changes apply cleanly on top of other agents' work. The
        // rebase MUST run inside the agent worktree: the branch is checked out
        // there, so rebasing it from the main repo fails with "already used by
        // worktree" and silently does nothing.
        let rebase_output = Command::new("git")
            .args(["rebase", &current])
            .current_dir(&wt_path)
            .output()?;

        if !rebase_output.status.success() {
            // Rebase failed (likely a conflict) — warn (so the operator knows
            // the branch was not cleanly rebased), abort, and fall back to a
            // plain merge, which surfaces the conflict explicitly below.
            eprintln!(
                "  warn: rebase of agent/{} onto {} failed; falling back to a direct merge: {}",
                agent_id,
                current,
                String::from_utf8_lossy(&rebase_output.stderr).trim()
            );
            let _ = Command::new("git")
                .args(["rebase", "--abort"])
                .current_dir(&wt_path)
                .output();
        }

        // Merge the agent branch into current branch
        let output = Command::new("git")
            .args([
                "merge",
                "--no-ff",
                &branch_name,
                "-m",
                &format!("grit: merge agent/{}", agent_id),
            ])
            .current_dir(&self.root)
            .output()
            .context("Failed to run git merge")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            // Abort any failed merge state. If the abort itself fails, the main
            // worktree may be left mid-merge — surface that in the error so the
            // operator knows manual `git merge --abort` may be needed.
            let abort = Command::new("git")
                .args(["merge", "--abort"])
                .current_dir(&self.root)
                .output();
            let abort_note = match abort {
                Ok(o) if o.status.success() => String::new(),
                Ok(o) => format!(
                    " (warning: `git merge --abort` also failed: {}; the main worktree may be mid-merge)",
                    String::from_utf8_lossy(&o.stderr).trim()
                ),
                Err(e) => format!(" (warning: could not run `git merge --abort`: {e}; the main worktree may be mid-merge)"),
            };
            anyhow::bail!("git merge failed: {}{}", stderr, abort_note);
        }

        Ok(())
    }

    /// Simple file-based spinlock for serializing git operations
    fn acquire_file_lock(&self, path: &Path) -> Result<FileLock> {
        let max_retries = 200; // 200 × 50ms = 10s max wait
        for attempt in 0..max_retries {
            // Try to exclusively create the lock file
            match fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(path)
            {
                Ok(file) => {
                    use std::io::Write;
                    let mut file = file;
                    let _ = write!(file, "{}", std::process::id());
                    return Ok(FileLock {
                        path: path.to_path_buf(),
                    });
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    // Decide whether the existing lock is stale. Prefer a
                    // definitive liveness check on the recorded PID; only fall
                    // back to a time heuristic when liveness cannot be
                    // determined. A live holder is NEVER treated as stale,
                    // regardless of how long it has held the lock — a large
                    // merge can legitimately run for minutes, and stealing its
                    // lock would let two `git merge` operations run concurrently
                    // against the same worktree.
                    let mut is_stale = false;
                    let mut liveness_known = false;
                    if let Ok(contents) = fs::read_to_string(path) {
                        if let Ok(pid) = contents.trim().parse::<u32>() {
                            // Check if process is alive (kill with signal 0)
                            use std::process::Command as Cmd;
                            if let Ok(output) =
                                Cmd::new("kill").args(["-0", &pid.to_string()]).output()
                            {
                                liveness_known = true;
                                if !output.status.success() {
                                    // Process is dead -- lock is stale.
                                    is_stale = true;
                                }
                                // Process is alive -- hold off and keep waiting.
                            }
                        }
                    }
                    if !liveness_known {
                        // Could not determine the holder's liveness (unreadable
                        // PID, or `kill` unavailable). Fall back to a time-based
                        // heuristic so a crashed holder cannot wedge the lock
                        // forever.
                        if let Ok(meta) = fs::metadata(path) {
                            if let Ok(modified) = meta.modified() {
                                if modified.elapsed().unwrap_or_default().as_secs() > 30 {
                                    is_stale = true;
                                }
                            }
                        }
                    }
                    if is_stale {
                        let _ = fs::remove_file(path);
                        continue;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
                Err(e) => anyhow::bail!("Failed to acquire merge lock: {}", e),
            }
            if attempt > 0 && attempt % 20 == 0 {
                eprintln!("  waiting for merge lock ({} attempts)...", attempt);
            }
        }
        anyhow::bail!("Timeout acquiring merge lock after 10s")
    }

    /// Get the current branch name
    pub fn current_branch(&self) -> Result<String> {
        let output = Command::new("git")
            .args(["branch", "--show-current"])
            .current_dir(&self.root)
            .output()?;
        let branch = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if branch.is_empty() {
            anyhow::bail!("Not on a branch (detached HEAD?)");
        }
        Ok(branch)
    }

    /// Create a session branch (feature branch where agents merge into)
    pub fn create_session_branch(&self, session_name: &str) -> Result<String> {
        let branch_name = format!("grit/{}", session_name);

        // Decide create-vs-switch from a real existence check rather than from
        // parsing stderr: git localizes its messages, so matching on
        // "already exists" silently breaks under non-English locales. The
        // branch name is `grit/<validated-name>` and can never start with `-`,
        // so option injection is not a concern (and no `--` separator is used:
        // `git checkout -b -- <branch>` misparses `<branch>` as a start-point).
        let exists = Command::new("git")
            .args([
                "rev-parse",
                "--verify",
                "--quiet",
                &format!("refs/heads/{}", branch_name),
            ])
            .current_dir(&self.root)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);

        let checkout_args: &[&str] = if exists {
            &["checkout", &branch_name]
        } else {
            &["checkout", "-b", &branch_name]
        };

        let output = Command::new("git")
            .args(checkout_args)
            .current_dir(&self.root)
            .output()
            .context("Failed to create or switch to session branch")?;

        if !output.status.success() {
            anyhow::bail!(
                "git checkout for session branch '{}' failed: {}",
                branch_name,
                String::from_utf8_lossy(&output.stderr)
            );
        }

        Ok(branch_name)
    }

    /// Push session branch to remote and create PR via gh CLI
    pub fn push_and_create_pr(&self, branch: &str, title: &str, body: &str) -> Result<String> {
        // Push to origin
        let output = Command::new("git")
            .args(["push", "-u", "origin", branch])
            .current_dir(&self.root)
            .output()
            .context("Failed to push branch")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            // "Everything up-to-date" is OK
            if !stderr.contains("up-to-date") {
                anyhow::bail!("git push failed: {}", stderr);
            }
        }

        // Create PR via gh
        let output = Command::new("gh")
            .args(["pr", "create", "--title", title, "--body", body])
            .current_dir(&self.root)
            .output()
            .context("Failed to create PR (is `gh` installed?)")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            if stderr.contains("already exists") {
                // PR already exists — resolve its URL. Pass the branch
                // explicitly (without it `gh pr view` resolves against the
                // current branch, which may not be this one) and check the
                // command actually succeeded before trusting stdout.
                let view = Command::new("gh")
                    .args(["pr", "view", branch, "--json", "url", "-q", ".url"])
                    .current_dir(&self.root)
                    .output()?;
                if !view.status.success() {
                    anyhow::bail!(
                        "PR already exists for {} but `gh pr view` failed: {}",
                        branch,
                        String::from_utf8_lossy(&view.stderr)
                    );
                }
                return Ok(String::from_utf8_lossy(&view.stdout).trim().to_string());
            }
            anyhow::bail!("gh pr create failed: {}", stderr);
        }

        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }

    /// Switch back to a branch
    pub fn checkout(&self, branch: &str) -> Result<()> {
        let output = Command::new("git")
            .args(["checkout", branch])
            .current_dir(&self.root)
            .output()?;
        if !output.status.success() {
            anyhow::bail!(
                "git checkout {} failed: {}",
                branch,
                String::from_utf8_lossy(&output.stderr)
            );
        }
        Ok(())
    }

    /// List all active agent worktrees
    pub fn list_worktrees(&self) -> Result<Vec<String>> {
        let wt_dir = self.grit_dir().join("worktrees");
        if !wt_dir.exists() {
            return Ok(Vec::new());
        }

        let mut agents = Vec::new();
        for entry in std::fs::read_dir(&wt_dir)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                if let Some(name) = entry.file_name().to_str() {
                    agents.push(name.to_string());
                }
            }
        }
        agents.sort();
        Ok(agents)
    }
}

/// RAII file lock — automatically removed when dropped
struct FileLock {
    path: PathBuf,
}

impl Drop for FileLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    /// Run a git command in `dir` and assert it succeeded.
    fn git(dir: &Path, args: &[&str]) {
        let out = Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .expect("git invocation failed");
        assert!(
            out.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn git_stdout(dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .expect("git invocation failed");
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// Create an initialized git repo with one commit (auth.rs + wip.txt) and
    /// an agent worktree holding a committed change on branch `agent/a1`.
    fn setup_repo_with_agent_commit() -> (PathBuf, GitRepo) {
        let unique = format!(
            "grit-test-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::SeqCst)
        );
        let root = std::env::temp_dir().join(unique);
        fs::create_dir_all(&root).unwrap();

        git(&root, &["init", "-q"]);
        git(&root, &["config", "user.email", "test@grit.test"]);
        git(&root, &["config", "user.name", "grit-test"]);
        git(&root, &["config", "commit.gpgsign", "false"]);
        fs::write(root.join("auth.rs"), "fn login() {}\n").unwrap();
        fs::write(root.join("wip.txt"), "base\n").unwrap();
        git(&root, &["add", "-A"]);
        git(&root, &["commit", "-q", "-m", "init"]);

        let repo = GitRepo::open(root.to_str().unwrap()).unwrap();
        let wt = repo.create_worktree("a1").unwrap();
        fs::write(wt.join("auth.rs"), "fn login() { /* edited */ }\n").unwrap();
        git(&wt, &["add", "-A"]);
        git(&wt, &["commit", "-q", "-m", "edit login"]);

        (root, repo)
    }

    #[test]
    fn merge_worktree_succeeds_on_clean_main_tree() {
        let (root, repo) = setup_repo_with_agent_commit();
        let agent_commit = git_stdout(&root, &["rev-parse", "agent/a1"]);

        repo.merge_worktree("a1").expect("merge should succeed");

        // The agent commit is now reachable from the current branch.
        let merged = Command::new("git")
            .args(["merge-base", "--is-ancestor", &agent_commit, "HEAD"])
            .current_dir(&root)
            .output()
            .unwrap();
        assert!(
            merged.status.success(),
            "agent commit should be merged into HEAD"
        );

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn merge_worktree_refuses_dirty_main_tree_and_preserves_branch() {
        let (root, repo) = setup_repo_with_agent_commit();
        let agent_commit = git_stdout(&root, &["rev-parse", "agent/a1"]);

        // Dirty the main worktree with a staged change to a tracked file.
        fs::write(root.join("wip.txt"), "staged wip\n").unwrap();
        git(&root, &["add", "wip.txt"]);

        let err = repo.merge_worktree("a1").unwrap_err().to_string();
        assert!(
            err.contains("uncommitted changes"),
            "expected a loud dirty-tree error, got: {err}"
        );

        // Regression for issue #21: the branch (and therefore the commit) must
        // survive a skipped merge so the work is recoverable.
        let branch = git_stdout(&root, &["branch", "--list", "agent/a1"]);
        assert!(!branch.is_empty(), "agent/a1 branch must be preserved");
        let still_there = git_stdout(&root, &["rev-parse", "agent/a1"]);
        assert_eq!(still_there, agent_commit, "agent commit must be intact");

        // The main repo must not have been flipped to bare.
        let bare = git_stdout(&root, &["config", "--get", "core.bare"]);
        assert_ne!(bare, "true", "core.bare must not be set on the main repo");

        fs::remove_dir_all(&root).ok();
    }
}
