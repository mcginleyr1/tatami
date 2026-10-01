//! A throwaway git repository that borrows the real repository's objects
//! (via alternates) so each jj commit can be replayed in exactly the state git
//! hooks expect during `git commit`: the commit's tree checked out and staged,
//! with HEAD at its parent. The repository's own installed hooks then run
//! unmodified via `git hook run`. The scratch repo lives under `.jj/` so hooks
//! run inside the project tree.

use std::ffi::OsStr;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use tempfile::TempDir;

/// Untracked dependency directories that hooks commonly need (e.g. husky
/// prepends `node_modules/.bin` to PATH). They are symlinked from the jj
/// workspace into the shadow worktree.
const LINKED_DIRS: [&str; 2] = ["node_modules", ".venv"];

pub struct Shadow {
    dir: TempDir,
    worktree: PathBuf,
    workspace_root: PathBuf,
    hooks_path: PathBuf,
}

fn git<S: AsRef<OsStr>>(dir: &Path, args: &[S]) -> Command {
    let mut cmd = Command::new("git");
    cmd.current_dir(dir).args(args);
    for var in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_INDEX_FILE",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_COMMON_DIR",
        "GIT_PREFIX",
    ] {
        cmd.env_remove(var);
    }
    cmd
}

fn output(mut cmd: Command) -> Result<String> {
    let out = cmd
        .output()
        .with_context(|| format!("failed to run {cmd:?}; is git installed?"))?;
    if !out.status.success() {
        bail!(
            "{cmd:?} failed:\n{}",
            String::from_utf8_lossy(&out.stderr).trim_end()
        );
    }
    Ok(String::from_utf8(out.stdout)?)
}

/// `git config --get`, distinguishing "unset" (exit 1) from real failures.
fn config_get(git_dir: &Path, key: &str) -> Result<Option<String>> {
    let mut cmd = git(git_dir, &[OsStr::new("--git-dir"), git_dir.as_os_str()]);
    cmd.args(["config", "--get", key]);
    let out = cmd.output()?;
    match out.status.code() {
        Some(0) => Ok(Some(String::from_utf8(out.stdout)?.trim_end().to_string())),
        Some(1) => Ok(None),
        _ => bail!(
            "git config --get {key} failed:\n{}",
            String::from_utf8_lossy(&out.stderr).trim_end()
        ),
    }
}

impl Shadow {
    pub fn new(git_dir: &Path, workspace_root: &Path) -> Result<Shadow> {
        let git_dir = git_dir
            .canonicalize()
            .with_context(|| format!("git dir {} not found", git_dir.display()))?;
        let in_main = |args: &[&str]| {
            let mut cmd = git(&git_dir, &[OsStr::new("--git-dir"), git_dir.as_os_str()]);
            cmd.args(args);
            output(cmd)
        };

        // Inside the workspace's .jj/ (never snapshotted by jj) rather than
        // /tmp, so per-directory toolchains (mise, asdf, .tool-versions) and
        // parent-directory lookups like node_modules resolve as in the repo.
        let dir = tempfile::Builder::new()
            .prefix("tatami-")
            .tempdir_in(workspace_root.join(".jj"))
            .context("failed to create a scratch repo under .jj/")?;
        let worktree = dir.path().join("wt");
        let format = in_main(&["rev-parse", "--show-object-format"])?;
        output(git(
            dir.path(),
            &[
                OsStr::new("init"),
                OsStr::new("-q"),
                OsStr::new(&format!("--object-format={}", format.trim())),
                worktree.as_os_str(),
            ],
        ))?;
        fs::write(
            worktree.join(".git/objects/info/alternates"),
            format!("{}\n", git_dir.join("objects").display()),
        )?;

        let hooks_path = match config_get(&git_dir, "core.hooksPath")? {
            Some(path) => workspace_root.join(path),
            None => git_dir.join("hooks"),
        };
        output(git(
            &worktree,
            &[
                OsStr::new("config"),
                OsStr::new("core.hooksPath"),
                hooks_path.as_os_str(),
            ],
        ))?;

        // Config-defined hooks (`hook.<name>.command`) in the repo's local
        // config; global and system ones are picked up by git on its own.
        let mut cmd = git(&git_dir, &[OsStr::new("--git-dir"), git_dir.as_os_str()]);
        cmd.args(["config", "--local", "-z", "--get-regexp", r"^hook\."]);
        let local = cmd.output()?;
        if !matches!(local.status.code(), Some(0 | 1)) {
            bail!(
                "reading hook.* config failed:\n{}",
                String::from_utf8_lossy(&local.stderr).trim_end()
            );
        }
        for entry in String::from_utf8(local.stdout)?.split_terminator('\0') {
            let (key, value) = entry.split_once('\n').unwrap_or((entry, ""));
            output(git(&worktree, &["config", "--add", key, value]))?;
        }

        // Remote-tracking refs, so pre-push hooks can tell what the remote has.
        let remotes = in_main(&[
            "for-each-ref",
            "--format=%(objectname) %(refname)",
            "refs/remotes/",
        ])?;
        fs::write(
            worktree.join(".git/packed-refs"),
            format!("# pack-refs with: peeled fully-peeled sorted \n{remotes}"),
        )?;

        Ok(Shadow {
            dir,
            worktree,
            workspace_root: workspace_root.to_path_buf(),
            hooks_path,
        })
    }

    /// Checks out and stages `tree_of`, with HEAD at `head` (None = unborn).
    pub fn stage(&self, tree_of: &str, head: Option<&str>) -> Result<()> {
        output(git(
            &self.worktree,
            &["read-tree", "--reset", "-u", tree_of],
        ))?;
        output(git(&self.worktree, &["clean", "-fdq"]))?;
        for name in LINKED_DIRS {
            let target = self.workspace_root.join(name);
            let link = self.worktree.join(name);
            if target.is_dir() && link.symlink_metadata().is_err() {
                std::os::unix::fs::symlink(&target, &link)?;
            }
        }
        match head {
            Some(id) => output(git(
                &self.worktree,
                &["update-ref", "--no-deref", "HEAD", id],
            ))?,
            None => output(git(
                &self.worktree,
                &["symbolic-ref", "HEAD", "refs/heads/tatami-unborn"],
            ))?,
        };
        Ok(())
    }

    /// Whether `git hook run event` would run anything. `git hook list` also
    /// sees config-defined hooks; gits without it (usage error, exit 129)
    /// only support hook files, so checking the file is exact there.
    pub fn has_hook(&self, event: &str) -> Result<bool> {
        let out = git(&self.worktree, &["hook", "list", event]).output()?;
        match out.status.code() {
            Some(0) => Ok(true),
            Some(1) => Ok(false),
            Some(129) => Ok(fs::metadata(self.hooks_path.join(event))
                .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)),
            _ => bail!(
                "`git hook list {event}` failed:\n{}",
                String::from_utf8_lossy(&out.stderr).trim_end()
            ),
        }
    }

    /// Runs the hooks for `event` with the terminal attached; true if they pass.
    pub fn run_hook(&self, event: &str, args: &[&str], stdin: Option<&str>) -> Result<bool> {
        let mut cmd = git(&self.worktree, &["hook", "run", "--ignore-missing"]);
        if let Some(input) = stdin {
            let path = self.dir.path().join(format!("{event}.stdin"));
            fs::write(&path, input)?;
            cmd.arg(format!("--to-stdin={}", path.display()));
        }
        cmd.arg(event).arg("--").args(args).stdin(Stdio::null());
        Ok(cmd.status()?.success())
    }

    /// Runs commit-msg hooks against `description` as git would, via
    /// `.git/COMMIT_EDITMSG`. Hooks that rewrite the message can't change the
    /// jj description; that is reported and otherwise ignored.
    pub fn run_commit_msg(&self, description: &str) -> Result<bool> {
        let path = self.worktree.join(".git/COMMIT_EDITMSG");
        fs::write(&path, description)?;
        let passed = self.run_hook("commit-msg", &[".git/COMMIT_EDITMSG"], None)?;
        if passed && fs::read_to_string(&path)? != description {
            eprintln!(
                "tatami: note: a commit-msg hook rewrote the message; jj descriptions are left unchanged"
            );
        }
        Ok(passed)
    }
}
