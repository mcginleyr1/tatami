use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use tempfile::TempDir;

/// The hooks look for this marker in added lines. The pattern `FORBIDD[E]N`
/// keeps hook scripts from matching themselves.
const PRE_COMMIT: &str = r#"#!/bin/sh
git diff --cached --name-only >> "LOG"
if git diff --cached | grep -q '^+.*FORBIDD[E]N'; then
  echo "forbidden marker found" >&2
  exit 1
fi
"#;

/// Strips trailing whitespace from staged files and fails if it changed
/// anything, like pre-commit's trailing-whitespace fixer.
const FIXER: &str = r#"#!/bin/sh
changed=0
for f in $(git diff --cached --name-only --diff-filter=ACM); do
  sed 's/[[:space:]]*$//' "$f" > "$f.tmp"
  if cmp -s "$f" "$f.tmp"; then rm "$f.tmp"; else mv "$f.tmp" "$f"; changed=1; fi
done
exit $changed
"#;

struct Repo {
    tmp: TempDir,
    root: PathBuf,
    remote: PathBuf,
}

impl Repo {
    fn new() -> Repo {
        let tmp = TempDir::new().unwrap();
        fs::write(
            tmp.path().join("jj.toml"),
            "[user]\nname = \"Test\"\nemail = \"test@example.com\"\n",
        )
        .unwrap();
        let repo = Repo {
            root: tmp.path().join("repo"),
            remote: tmp.path().join("remote"),
            tmp,
        };
        repo.jj_in(repo.tmp.path(), &["git", "init", "--no-colocate", "remote"]);
        repo.jj_in(repo.tmp.path(), &["git", "init", "--colocate", "repo"]);
        let remote_git = repo.remote.join(".jj/repo/store/git");
        repo.jj(&[
            "git",
            "remote",
            "add",
            "origin",
            remote_git.to_str().unwrap(),
        ]);
        repo.write("README", "hello\n");
        repo.jj(&["commit", "-m", "init"]);
        repo.jj(&["bookmark", "create", "main", "-r", "@-"]);
        repo.jj(&["git", "push", "-b", "main"]);
        repo
    }

    fn env(&self, cmd: &mut Command) {
        let bin_dir = Path::new(env!("CARGO_BIN_EXE_tatami")).parent().unwrap();
        let path = std::env::var_os("PATH").unwrap_or_default();
        let path = std::env::join_paths(
            std::iter::once(bin_dir.to_path_buf()).chain(std::env::split_paths(&path)),
        )
        .unwrap();
        cmd.env("PATH", path)
            .env("JJ_CONFIG", self.tmp.path().join("jj.toml"))
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("PRE_COMMIT_HOME", self.tmp.path().join("pre-commit-home"));
    }

    fn jj_in(&self, dir: &Path, args: &[&str]) -> String {
        let mut cmd = Command::new("jj");
        cmd.current_dir(dir).args(args);
        self.env(&mut cmd);
        let out = cmd.output().unwrap();
        assert!(out.status.success(), "jj {args:?}: {}", stderr(&out));
        String::from_utf8(out.stdout).unwrap()
    }

    fn jj(&self, args: &[&str]) -> String {
        self.jj_in(&self.root, args)
    }

    fn tatami(&self, args: &[&str]) -> Output {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_tatami"));
        cmd.current_dir(&self.root).args(args);
        self.env(&mut cmd);
        cmd.output().unwrap()
    }

    fn write(&self, path: &str, content: &str) {
        let path = self.root.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    fn commit(&self, path: &str, content: &str, message: &str) {
        self.write(path, content);
        self.jj(&["commit", "-m", message]);
    }

    /// Installs `script` as a hook; "LOG" in it is replaced with a log path.
    fn hook(&self, name: &str, script: &str) {
        let path = self.root.join(".git/hooks").join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            script.replace("LOG", self.log_path().to_str().unwrap()),
        )
        .unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn log_path(&self) -> PathBuf {
        self.tmp.path().join("hook.log")
    }

    fn log(&self) -> String {
        fs::read_to_string(self.log_path()).unwrap_or_default()
    }

    fn remote_has(&self, bookmark: &str) -> bool {
        self.jj_in(&self.remote, &["git", "import"]);
        let revset = format!("present(bookmarks(exact:\"{bookmark}\"))");
        !self
            .jj_in(
                &self.remote,
                &["log", "--no-graph", "-r", &revset, "-T", "commit_id"],
            )
            .is_empty()
    }

    fn id(&self, revision: &str) -> String {
        self.jj(&["log", "--no-graph", "-r", revision, "-T", "commit_id"])
    }
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

#[test]
fn rejects_a_bad_commit_buried_under_a_clean_tip() {
    let repo = Repo::new();
    repo.hook("pre-commit", PRE_COMMIT);
    repo.commit("a.txt", "FORBIDDEN\n", "add a");
    repo.commit("a.txt", "fine now\n", "fix a");
    repo.jj(&["bookmark", "create", "feature", "-r", "@-"]);

    let out = repo.tatami(&["push", "-b", "feature"]);

    assert!(!out.status.success(), "push should fail: {}", stderr(&out));
    assert!(
        stderr(&out).contains("pre-commit hooks failed"),
        "{}",
        stderr(&out)
    );
    assert!(!repo.remote_has("feature"));
}

#[test]
fn pushes_a_clean_stack_and_stages_exactly_each_commits_files() {
    let repo = Repo::new();
    repo.hook("pre-commit", PRE_COMMIT);
    repo.commit("a.txt", "a\n", "add a");
    repo.commit("b.txt", "b\n", "add b");
    repo.jj(&["bookmark", "create", "feature", "-r", "@-"]);

    let out = repo.tatami(&["push", "-b", "feature"]);

    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(repo.log(), "a.txt\nb.txt\n");
    assert!(repo.remote_has("feature"));
}

#[test]
fn checks_a_bookmark_that_is_not_under_the_working_copy() {
    let repo = Repo::new();
    repo.hook("pre-commit", PRE_COMMIT);
    repo.jj(&["new", "main"]);
    repo.commit("side.txt", "FORBIDDEN\n", "side work");
    repo.jj(&["bookmark", "create", "side", "-r", "@-"]);
    repo.jj(&["new", "main"]);
    repo.write("clean.txt", "clean\n");

    let out = repo.tatami(&["push", "-b", "side"]);

    assert!(!out.status.success(), "push should fail: {}", stderr(&out));
    assert!(!repo.remote_has("side"));
}

#[test]
fn runs_commit_msg_hooks_on_descriptions() {
    let repo = Repo::new();
    repo.hook(
        "commit-msg",
        "#!/bin/sh\ngrep -q '^JIRA-' \"$1\" || { echo 'needs a JIRA key' >&2; exit 1; }\n",
    );
    repo.commit("a.txt", "a\n", "no ticket");
    repo.jj(&["bookmark", "create", "feature", "-r", "@-"]);

    let bad = repo.tatami(&["push", "-b", "feature"]);
    assert!(!bad.status.success());
    assert!(
        stderr(&bad).contains("commit-msg hooks failed"),
        "{}",
        stderr(&bad)
    );

    repo.jj(&["describe", "-r", "feature", "-m", "JIRA-1 add a"]);
    let good = repo.tatami(&["push", "-b", "feature"]);
    assert!(good.status.success(), "{}", stderr(&good));
}

#[test]
fn feeds_pre_push_the_stdin_and_arguments_git_would() {
    let repo = Repo::new();
    repo.hook(
        "pre-push",
        "#!/bin/sh\necho \"args: $1 $2\" >> \"LOG\"\ncat >> \"LOG\"\n",
    );
    repo.commit("a.txt", "a\n", "add a");
    repo.jj(&["bookmark", "create", "feature", "-r", "@-"]);
    let feature = repo.id("feature");
    let zeros = "0".repeat(feature.len());

    let out = repo.tatami(&["push", "-b", "feature"]);

    assert!(out.status.success(), "{}", stderr(&out));
    let url = repo
        .remote
        .join(".jj/repo/store/git")
        .canonicalize()
        .unwrap();
    assert_eq!(
        repo.log(),
        format!(
            "args: origin {}\nrefs/heads/feature {feature} refs/heads/feature {zeros}\n",
            url.display()
        )
    );
}

#[test]
fn refuses_when_a_hook_config_exists_but_hooks_are_not_installed() {
    let repo = Repo::new();
    repo.commit(".pre-commit-config.yaml", "repos: []\n", "add config");
    repo.jj(&["bookmark", "create", "feature", "-r", "@-"]);

    let out = repo.tatami(&["push", "-b", "feature"]);

    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("no git hooks are installed"),
        "{}",
        stderr(&out)
    );
    assert!(!repo.remote_has("feature"));
}

#[test]
fn pushes_when_the_repo_has_no_hooks_at_all() {
    let repo = Repo::new();
    repo.commit("a.txt", "a\n", "add a");
    repo.jj(&["bookmark", "create", "feature", "-r", "@-"]);

    let out = repo.tatami(&["push", "-b", "feature"]);

    assert!(out.status.success(), "{}", stderr(&out));
    assert!(repo.remote_has("feature"));
}

#[test]
fn no_verify_skips_hooks_wherever_it_appears() {
    let repo = Repo::new();
    repo.hook("pre-commit", PRE_COMMIT);
    repo.commit("a.txt", "FORBIDDEN\n", "add a");
    repo.jj(&["bookmark", "create", "feature", "-r", "@-"]);

    let out = repo.tatami(&["push", "-b", "feature", "--no-verify"]);

    assert!(out.status.success(), "{}", stderr(&out));
    assert!(repo.remote_has("feature"));
}

#[test]
fn honours_core_hooks_path_like_husky() {
    let repo = Repo::new();
    let config = repo.root.join(".git/config");
    let mut text = fs::read_to_string(&config).unwrap();
    text.push_str("[core]\n\thooksPath = .myhooks\n");
    fs::write(&config, text).unwrap();
    repo.commit(".gitignore", ".myhooks/\n", "ignore hooks");
    let hook = repo.root.join(".myhooks/pre-commit");
    fs::create_dir_all(hook.parent().unwrap()).unwrap();
    fs::write(
        &hook,
        PRE_COMMIT.replace("LOG", repo.log_path().to_str().unwrap()),
    )
    .unwrap();
    fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();
    repo.commit("a.txt", "FORBIDDEN\n", "add a");
    repo.jj(&["bookmark", "create", "feature", "-r", "@-"]);

    let out = repo.tatami(&["push", "-b", "feature"]);

    assert!(!out.status.success(), "push should fail: {}", stderr(&out));
}

#[test]
fn install_makes_jj_push_run_the_hooks_and_pass_arguments_through() {
    let repo = Repo::new();
    repo.hook("pre-commit", PRE_COMMIT);
    repo.commit("a.txt", "FORBIDDEN\n", "add a");
    repo.jj(&["bookmark", "create", "feature", "-r", "@-"]);
    let install = repo.tatami(&["install"]);
    assert!(install.status.success(), "{}", stderr(&install));
    let jj_push = |args: &[&str]| {
        let mut cmd = Command::new("jj");
        cmd.current_dir(&repo.root).arg("push").args(args);
        repo.env(&mut cmd);
        cmd.output().unwrap()
    };

    let blocked = jj_push(&["-b", "feature"]);
    assert!(!blocked.status.success(), "{}", stderr(&blocked));
    assert!(
        stderr(&blocked).contains("pre-commit hooks failed"),
        "{}",
        stderr(&blocked)
    );
    assert!(!repo.remote_has("feature"));

    let forced = jj_push(&["-b", "feature", "--no-verify"]);
    assert!(forced.status.success(), "{}", stderr(&forced));
    assert!(repo.remote_has("feature"));
}

#[test]
fn hooks_run_inside_the_project_tree_and_leave_nothing_behind() {
    let repo = Repo::new();
    repo.hook("pre-commit", "#!/bin/sh\npwd -P >> \"LOG\"\n");
    repo.commit("a.txt", "a\n", "add a");

    let out = repo.tatami(&["check", "-r", "@-"]);

    assert!(out.status.success(), "{}", stderr(&out));
    let jj_dir = repo.root.join(".jj").canonicalize().unwrap();
    assert!(
        repo.log().starts_with(jj_dir.to_str().unwrap()),
        "hook ran in {}",
        repo.log()
    );
    let leftovers: Vec<_> = fs::read_dir(&jj_dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().starts_with("tatami-"))
        .collect();
    assert!(leftovers.is_empty(), "scratch repo not cleaned up");
}

#[test]
fn check_runs_hooks_over_a_revset_without_pushing() {
    let repo = Repo::new();
    repo.hook("pre-commit", PRE_COMMIT);
    repo.commit("a.txt", "a\n", "add a");
    repo.commit("b.txt", "FORBIDDEN\n", "add b");

    let out = repo.tatami(&["check", "-r", "main..@"]);

    assert!(!out.status.success());
    assert!(stderr(&out).contains("add b"), "{}", stderr(&out));
}

#[test]
fn fix_rewrites_each_commit_and_its_descendants_without_conflicts() {
    let repo = Repo::new();
    repo.hook("pre-commit", FIXER);
    repo.commit("a.txt", "one  \n", "add a");
    repo.write("a.txt", "one  \ntwo  \n");
    repo.commit("b.txt", "b  \n", "extend a, add b");

    let out = repo.tatami(&["fix", "-r", "main..@"]);

    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(repo.jj(&["file", "show", "-r", "@--", "a.txt"]), "one\n");
    assert_eq!(
        repo.jj(&["file", "show", "-r", "@-", "a.txt"]),
        "one\ntwo\n"
    );
    assert_eq!(repo.jj(&["file", "show", "-r", "@-", "b.txt"]), "b\n");
    assert_eq!(
        repo.jj(&["log", "--no-graph", "-r", "conflicts()", "-T", "commit_id"]),
        ""
    );
    let check = repo.tatami(&["check", "-r", "main..@"]);
    assert!(check.status.success(), "{}", stderr(&check));
}

#[test]
fn push_fix_applies_fixes_and_then_pushes() {
    let repo = Repo::new();
    repo.hook("pre-commit", FIXER);
    repo.commit("a.txt", "x  \n", "add a");
    repo.jj(&["bookmark", "create", "feature", "-r", "@-"]);

    let out = repo.tatami(&["push", "-b", "feature", "--fix"]);

    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(repo.jj(&["file", "show", "-r", "feature", "a.txt"]), "x\n");
    assert!(repo.remote_has("feature"));
}

#[test]
fn fix_applies_what_it_can_and_reports_the_rest() {
    let repo = Repo::new();
    let fix_then_lint = FIXER.replace(
        "exit $changed",
        "git diff --cached | grep -q '^+.*FORBIDD[E]N' && exit 1\nexit $changed",
    );
    repo.hook("pre-commit", &fix_then_lint);
    repo.commit("a.txt", "FORBIDDEN  \n", "add a");

    let out = repo.tatami(&["fix", "-r", "main..@"]);

    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("hooks still fail after fixing"),
        "{}",
        stderr(&out)
    );
    assert_eq!(
        repo.jj(&["file", "show", "-r", "@-", "a.txt"]),
        "FORBIDDEN\n"
    );
}

#[test]
fn fix_does_not_run_the_users_own_jj_fix_tools() {
    let repo = Repo::new();
    repo.jj(&[
        "config",
        "set",
        "--repo",
        "fix.tools.upper.command",
        r#"["tr", "a-z", "A-Z"]"#,
    ]);
    repo.jj(&[
        "config",
        "set",
        "--repo",
        "fix.tools.upper.patterns",
        r#"["all()"]"#,
    ]);
    repo.hook("pre-commit", FIXER);
    repo.commit("a.txt", "x  \n", "add a");

    let out = repo.tatami(&["fix", "-r", "main..@"]);

    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(repo.jj(&["file", "show", "-r", "@-", "a.txt"]), "x\n");
}

#[test]
fn fix_works_with_a_real_pre_commit_fixer() {
    let Some(repo) = pre_commit_repo(
        "repos:\n- repo: local\n  hooks:\n  - id: strip\n    name: strip trailing whitespace\n    language: system\n    entry: perl -pi -e 's/[ \\t]+$//'\n    types: [text]\n",
    ) else {
        return;
    };
    repo.commit("a.txt", "x  \n", "add a");

    let out = repo.tatami(&["fix", "-r", "main..@"]);

    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(repo.jj(&["file", "show", "-r", "@-", "a.txt"]), "x\n");
}

/// A repo using the real pre-commit framework with `config`, or None when
/// `uvx pre-commit` isn't available.
fn pre_commit_repo(config: &str) -> Option<Repo> {
    let available = Command::new("uvx")
        .args(["pre-commit", "--version"])
        .output()
        .is_ok_and(|o| o.status.success());
    if !available {
        eprintln!("skipping: `uvx pre-commit` unavailable");
        return None;
    }
    let repo = Repo::new();
    repo.commit(".pre-commit-config.yaml", config, "add pre-commit config");
    let mut install = Command::new("uvx");
    install
        .current_dir(&repo.root)
        .args(["pre-commit", "install"]);
    repo.env(&mut install);
    let out = install.output().unwrap();
    assert!(out.status.success(), "pre-commit install: {}", stderr(&out));
    Some(repo)
}

#[test]
fn works_with_the_real_pre_commit_framework() {
    let Some(repo) = pre_commit_repo(
        "repos:\n- repo: local\n  hooks:\n  - id: no-forbidden\n    name: no forbidden marker\n    language: pygrep\n    entry: FORBIDD[E]N\n",
    ) else {
        return;
    };
    repo.commit("a.txt", "FORBIDDEN\n", "add a");
    repo.commit("a.txt", "fine now\n", "fix a");
    repo.jj(&["bookmark", "create", "feature", "-r", "@-"]);

    let out = repo.tatami(&["push", "-b", "feature"]);

    assert!(!out.status.success(), "push should fail: {}", stderr(&out));
    assert!(
        stderr(&out).contains("no forbidden marker"),
        "{}",
        stderr(&out)
    );
    assert!(!repo.remote_has("feature"));
}
