mod fix;
mod jj;
mod shadow;

use std::collections::BTreeMap;
use std::path::Path;
use std::process::{Command, ExitCode};

use anyhow::{Result, bail};
use clap::Parser;

use jj::{Commit, RemotePush};
use shadow::Shadow;

/// Replaying more commits than this almost always means the revset reaches
/// into history nobody meant to check (e.g. a first push with no trunk).
const MAX_COMMITS: usize = 100;

/// Hook-manager config files: if one exists but no hook is installed, the
/// repo expects checks that would silently not run, so tatami refuses.
const MANAGER_FILES: [&str; 11] = [
    ".pre-commit-config.yaml",
    ".pre-commit-config.yml",
    "lefthook.yml",
    "lefthook.yaml",
    ".lefthook.yml",
    ".lefthook.yaml",
    "lefthook.toml",
    "lefthook.json",
    "hk.pkl",
    ".husky",
    ".overcommit.yml",
];

#[derive(Parser)]
#[command(version, about = "Run a repo's git hooks against jj commits")]
enum Cli {
    /// Run hooks for every commit `jj git push` would send, then push.
    Push {
        /// Skip the hooks and just push.
        #[arg(long)]
        no_verify: bool,
        /// Apply the hooks' fixes to the commits first (see `tatami fix`).
        #[arg(long)]
        fix: bool,
        /// Arguments for `jj git push`.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Run pre-commit and commit-msg hooks for each commit in a revset.
    Check {
        #[arg(short, long, default_value = "trunk()..@")]
        revisions: String,
    },
    /// Rewrite each commit in a revset with what its pre-commit hooks fix.
    Fix {
        #[arg(short, long, default_value = "trunk()..@")]
        revisions: String,
    },
    /// Internal: the `jj fix` tool used by `tatami fix`.
    #[command(hide = true)]
    FixTool { path: String },
    /// Add a `jj push` alias that runs `tatami push`.
    Install {
        /// Write the alias to user config instead of this repo's config.
        #[arg(long)]
        user: bool,
    },
}

struct Hooks {
    pre_commit: bool,
    commit_msg: bool,
    pre_push: bool,
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(code) => code,
        Err(err) => {
            eprintln!("tatami: {err:#}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> Result<ExitCode> {
    match cli {
        Cli::Push {
            no_verify,
            fix,
            mut args,
        } => {
            // tatami's flags may also come after `jj git push` arguments.
            let mut take = |flag: &str| {
                let found = args.iter().any(|a| a == flag);
                args.retain(|a| a != flag);
                found
            };
            let fix = take("--fix") || fix;
            let no_verify = take("--no-verify") || no_verify;
            if fix {
                fix_push(&args)?;
            }
            if !no_verify {
                verify_push(&args)?;
            }
            let status = Command::new("jj")
                .args(["git", "push"])
                .args(&args)
                .status()?;
            Ok(status
                .code()
                .map_or(ExitCode::FAILURE, |c| ExitCode::from(c as u8)))
        }
        Cli::Check { revisions } => {
            let root = jj::workspace_root()?;
            let commits = jj::commits(&root, &revisions)?;
            validate(&commits)?;
            if let Some((shadow, hooks)) = shadow_with_hooks(&root)? {
                check_commits(&shadow, &hooks, &commits)?;
            }
            Ok(ExitCode::SUCCESS)
        }
        Cli::Fix { revisions } => {
            let root = jj::workspace_root()?;
            let commits = jj::commits(&root, &revisions)?;
            fix_commits(&root, &commits)?;
            Ok(ExitCode::SUCCESS)
        }
        Cli::FixTool { path } => {
            fix::tool(&path)?;
            Ok(ExitCode::SUCCESS)
        }
        Cli::Install { user } => {
            let dir = match user {
                true => std::env::current_dir()?,
                false => jj::workspace_root()?,
            };
            let scope = if user { "--user" } else { "--repo" };
            jj::run(
                &dir,
                &[
                    "config",
                    "set",
                    scope,
                    "aliases.push",
                    r#"["util", "exec", "--", "tatami", "push"]"#,
                ],
            )?;
            eprintln!("tatami: `jj push` now runs your git hooks, then `jj git push`");
            Ok(ExitCode::SUCCESS)
        }
    }
}

/// What `jj git push <args>` would do, and the commits it would send.
fn push_plan(root: &Path, args: &[String]) -> Result<(Vec<RemotePush>, Vec<Commit>)> {
    let dry = jj::command()
        .args(["git", "push", "--dry-run"])
        .args(args)
        .output()?;
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&dry.stdout),
        String::from_utf8_lossy(&dry.stderr)
    );
    if !dry.status.success() {
        bail!("`jj git push --dry-run` failed:\n{}", text.trim_end());
    }
    let pushes = jj::parse_dry_run(&text)?;
    let commits = match revset_to_check(&pushes) {
        Some(revset) => jj::commits(root, &revset)?,
        None => vec![],
    };
    validate(&commits)?;
    Ok((pushes, commits))
}

fn verify_push(args: &[String]) -> Result<()> {
    let root = jj::workspace_root()?;
    let (pushes, commits) = push_plan(&root, args)?;
    if commits.is_empty() {
        return Ok(());
    }
    let Some((shadow, hooks)) = shadow_with_hooks(&root)? else {
        return Ok(());
    };
    check_commits(&shadow, &hooks, &commits)?;
    if hooks.pre_push {
        run_pre_push(&root, &shadow, &pushes)?;
    }
    Ok(())
}

fn fix_push(args: &[String]) -> Result<()> {
    let root = jj::workspace_root()?;
    let (_, commits) = push_plan(&root, args)?;
    fix_commits(&root, &commits)
}

fn fix_commits(root: &Path, commits: &[Commit]) -> Result<()> {
    if commits.is_empty() {
        return Ok(());
    }
    validate(commits)?;
    match shadow_with_hooks(root)? {
        Some((shadow, hooks)) if hooks.pre_commit => fix::fix(root, &shadow, commits),
        _ => {
            eprintln!("tatami: no pre-commit hooks installed; nothing to fix");
            Ok(())
        }
    }
}

/// Every commit being sent: ancestors of the new targets that the remote
/// doesn't have yet and that aren't immutable.
fn revset_to_check(pushes: &[RemotePush]) -> Option<String> {
    let parts: Vec<String> = pushes
        .iter()
        .filter_map(|push| {
            let heads: Vec<&str> = push
                .updates
                .iter()
                .filter_map(|u| u.new.as_deref())
                .collect();
            (!heads.is_empty()).then(|| {
                format!(
                    "(::({}) ~ ::(remote_bookmarks(remote=exact:{}) | immutable_heads()))",
                    heads.join(" | "),
                    jj::quote(&push.remote)
                )
            })
        })
        .collect();
    (!parts.is_empty()).then(|| parts.join(" | "))
}

fn validate(commits: &[Commit]) -> Result<()> {
    if commits.len() > MAX_COMMITS {
        bail!(
            "refusing to check {} commits (limit {MAX_COMMITS}); check a narrower set with `tatami check -r`, or push with --no-verify",
            commits.len()
        );
    }
    if let Some(c) = commits.iter().find(|c| c.conflict) {
        bail!(
            "{} {} has unresolved conflicts; hooks would see jj's conflict encoding, not your files",
            c.short(),
            c.subject()
        );
    }
    Ok(())
}

/// The shadow repo plus which hook events have something to run, or None if
/// the repo has no hooks at all.
fn shadow_with_hooks(root: &Path) -> Result<Option<(Shadow, Hooks)>> {
    let shadow = Shadow::new(&jj::git_dir(root)?, root)?;
    let hooks = Hooks {
        pre_commit: shadow.has_hook("pre-commit")?,
        commit_msg: shadow.has_hook("commit-msg")?,
        pre_push: shadow.has_hook("pre-push")?,
    };
    if hooks.pre_commit || hooks.commit_msg || hooks.pre_push {
        return Ok(Some((shadow, hooks)));
    }
    if let Some(file) = MANAGER_FILES.iter().find(|f| root.join(f).exists()) {
        bail!(
            "{file} exists but no git hooks are installed, so nothing would be checked; run its installer (e.g. `pre-commit install`)"
        );
    }
    eprintln!("tatami: no git hooks installed; nothing to check");
    Ok(None)
}

fn check_commits(shadow: &Shadow, hooks: &Hooks, commits: &[Commit]) -> Result<()> {
    if !(hooks.pre_commit || hooks.commit_msg) {
        return Ok(());
    }
    eprintln!("tatami: checking {} commit(s)", commits.len());
    for c in commits {
        eprintln!("tatami: {} {}", c.short(), c.subject());
        shadow.stage(&c.id, c.git_parent())?;
        if hooks.pre_commit && !shadow.run_hook("pre-commit", &[], None)? {
            bail!("pre-commit hooks failed for {} {}", c.short(), c.subject());
        }
        if !hooks.commit_msg {
            continue;
        }
        if c.description.is_empty() {
            eprintln!(
                "tatami: {} has no description; skipping commit-msg",
                c.short()
            );
        } else if !shadow.run_commit_msg(&c.description)? {
            bail!("commit-msg hooks failed for {} {}", c.short(), c.subject());
        }
    }
    Ok(())
}

/// Runs pre-push once per pushed commit, checked out at that commit, with the
/// stdin lines git would send for the refs moving to it. Deletions carry no
/// content and are not checked.
fn run_pre_push(root: &Path, shadow: &Shadow, pushes: &[RemotePush]) -> Result<()> {
    for push in pushes {
        let url = jj::remote_url(root, &push.remote)?;
        let mut by_head: BTreeMap<String, String> = BTreeMap::new();
        for update in &push.updates {
            let Some(new) = &update.new else { continue };
            let new = jj::resolve(root, new)?;
            let old = match &update.old {
                Some(old) => jj::resolve(root, old)?,
                None => "0".repeat(new.len()),
            };
            let line = format!("{r} {new} {r} {old}\n", r = update.git_ref());
            by_head.entry(new).or_default().push_str(&line);
        }
        for (head, stdin) in by_head {
            eprintln!("tatami: pre-push {} -> {}", &head[..12], push.remote);
            shadow.stage(&head, Some(&head))?;
            if !shadow.run_hook("pre-push", &[&push.remote, &url], Some(&stdin))? {
                bail!("pre-push hooks failed for {}", &head[..12]);
            }
        }
    }
    Ok(())
}
