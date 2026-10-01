//! `tatami fix`: run the pre-commit hooks on each commit, keep whatever their
//! fixers rewrite, then let `jj fix` write those files into the commits.
//! `jj fix` updates descendants too and never creates conflicts. During that
//! run tatami is the only enabled fix tool: `tatami fix-tool <path>` looks the
//! file's content up in the cache of fixes the hooks produced, and passes
//! anything else through unchanged.

use std::collections::BTreeSet;
use std::fs;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io::{Read, Write};
use std::path::Path;

use anyhow::{Context, Result, bail};

use crate::jj::{self, Commit};
use crate::shadow::Shadow;

const CACHE_ENV: &str = "TATAMI_FIX_CACHE";

/// Both writer and reader are this binary, so the std hasher's unspecified
/// algorithm is stable enough; a hit is also confirmed against `.orig`.
fn cache_key(path: &str, content: &[u8]) -> String {
    let mut hasher = DefaultHasher::new();
    path.hash(&mut hasher);
    content.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

pub fn fix(root: &Path, shadow: &Shadow, commits: &[Commit]) -> Result<()> {
    let cache = tempfile::Builder::new()
        .prefix("tatami-fix-")
        .tempdir_in(root.join(".jj"))?;
    let mut paths = BTreeSet::new();
    let mut failing = vec![];
    for c in commits {
        eprintln!("tatami: fixing {} {}", c.short(), c.subject());
        shadow.stage(&c.id, c.git_parent())?;
        let fixes = shadow.run_fixers()?;
        for (path, fixed) in fixes.files {
            let original = shadow.blob(&c.id, &path)?;
            let key = cache_key(&path, &original);
            fs::write(cache.path().join(format!("{key}.orig")), &original)?;
            fs::write(cache.path().join(format!("{key}.fixed")), &fixed)?;
            paths.insert(path);
        }
        if !fixes.passed {
            failing.push(format!("{} {}", c.short(), c.subject()));
        }
    }
    if paths.is_empty() {
        eprintln!("tatami: hooks changed nothing");
    } else {
        apply(root, cache.path(), commits, &paths)?;
    }
    if !failing.is_empty() {
        bail!(
            "hooks still fail after fixing (these need a manual fix):\n  {}",
            failing.join("\n  ")
        );
    }
    Ok(())
}

fn apply(root: &Path, cache: &Path, commits: &[Commit], paths: &BTreeSet<String>) -> Result<()> {
    let exe = std::env::current_exe()?;
    let exe = exe.to_str().context("tatami's own path isn't UTF-8")?;
    let revset = commits
        .iter()
        .map(|c| c.id.as_str())
        .collect::<Vec<_>>()
        .join(" | ");
    let mut cmd = jj::command();
    cmd.current_dir(root).env(CACHE_ENV, cache).args([
        "--config",
        &format!(
            "fix.tools.tatami.command=[{}, \"fix-tool\", \"$path\"]",
            jj::quote(exe)
        ),
        "--config",
        r#"fix.tools.tatami.patterns=["all()"]"#,
    ]);
    for name in jj::fix_tool_names(root)? {
        if name != "tatami" {
            cmd.arg("--config")
                .arg(format!("fix.tools.{name}.enabled=false"));
        }
    }
    cmd.args(["fix", "--include-unchanged-files", "-s", &revset])
        .args(paths.iter().map(|p| format!("root-file:{}", jj::quote(p))));
    if !cmd.status()?.success() {
        bail!("`jj fix` failed to apply the hooks' fixes");
    }
    Ok(())
}

/// The `jj fix` tool: stdin is one file's content in some commit.
pub fn tool(path: &str) -> Result<()> {
    let cache =
        std::env::var_os(CACHE_ENV).context("fix-tool is only meant to be run by `tatami fix`")?;
    let mut content = vec![];
    std::io::stdin().read_to_end(&mut content)?;
    let key = cache_key(path, &content);
    let cache = Path::new(&cache);
    let fixed = match fs::read(cache.join(format!("{key}.orig"))) {
        Ok(original) if original == content => fs::read(cache.join(format!("{key}.fixed")))?,
        _ => content,
    };
    std::io::stdout().write_all(&fixed)?;
    Ok(())
}
