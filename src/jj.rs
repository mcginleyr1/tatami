use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

pub struct Commit {
    pub id: String,
    pub parents: Vec<String>,
    pub conflict: bool,
    pub description: String,
}

impl Commit {
    pub fn short(&self) -> &str {
        &self.id[..12.min(self.id.len())]
    }

    pub fn subject(&self) -> &str {
        self.description
            .lines()
            .next()
            .unwrap_or("(no description set)")
    }

    /// The parent git sees as HEAD while committing, or None for a root commit.
    pub fn git_parent(&self) -> Option<&str> {
        self.parents
            .first()
            .map(String::as_str)
            .filter(|p| !p.bytes().all(|b| b == b'0'))
    }
}

#[derive(Debug, PartialEq)]
pub enum RefKind {
    Bookmark,
    Tag,
}

#[derive(Debug, PartialEq)]
pub struct RefUpdate {
    pub kind: RefKind,
    pub name: String,
    pub old: Option<String>,
    pub new: Option<String>,
}

impl RefUpdate {
    pub fn git_ref(&self) -> String {
        match self.kind {
            RefKind::Bookmark => format!("refs/heads/{}", self.name),
            RefKind::Tag => format!("refs/tags/{}", self.name),
        }
    }
}

#[derive(Debug, PartialEq)]
pub struct RemotePush {
    pub remote: String,
    pub updates: Vec<RefUpdate>,
}

const COMMIT_TEMPLATE: &str = r#"commit_id ++ "\0" ++ parents.map(|c| c.commit_id()).join(" ") ++ "\0" ++ if(conflict, "1", "0") ++ "\0" ++ description ++ "\0""#;

pub fn command() -> Command {
    let mut cmd = Command::new("jj");
    cmd.args(["--no-pager", "--color=never"]);
    cmd
}

pub fn run(root: &Path, args: &[&str]) -> Result<String> {
    let output = command()
        .current_dir(root)
        .args(args)
        .output()
        .context("failed to run jj; is it installed and on PATH?")?;
    if !output.status.success() {
        bail!(
            "`jj {}` failed:\n{}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim_end()
        );
    }
    Ok(String::from_utf8(output.stdout)?)
}

pub fn workspace_root() -> Result<PathBuf> {
    let cwd = std::env::current_dir()?;
    Ok(PathBuf::from(run(&cwd, &["workspace", "root"])?.trim_end()))
}

pub fn git_dir(root: &Path) -> Result<PathBuf> {
    Ok(PathBuf::from(run(root, &["git", "root"])?.trim_end()))
}

/// Commits in `revset`, oldest first.
pub fn commits(root: &Path, revset: &str) -> Result<Vec<Commit>> {
    let out = run(
        root,
        &[
            "log",
            "--no-graph",
            "--reversed",
            "-r",
            revset,
            "-T",
            COMMIT_TEMPLATE,
        ],
    )?;
    let Some(out) = out.strip_suffix('\0') else {
        return Ok(vec![]);
    };
    let fields: Vec<&str> = out.split('\0').collect();
    let (records, rest) = fields.as_chunks::<4>();
    if !rest.is_empty() {
        bail!("unexpected `jj log` output for revset {revset:?}");
    }
    Ok(records
        .iter()
        .map(|[id, parents, conflict, description]| Commit {
            id: id.to_string(),
            parents: parents.split_whitespace().map(String::from).collect(),
            conflict: *conflict == "1",
            description: description.to_string(),
        })
        .collect())
}

pub fn resolve(root: &Path, revision: &str) -> Result<String> {
    let id = run(
        root,
        &["log", "--no-graph", "-r", revision, "-T", "commit_id"],
    )?;
    if id.is_empty() {
        bail!("revision {revision:?} resolved to nothing");
    }
    Ok(id)
}

pub fn remote_url(root: &Path, remote: &str) -> Result<String> {
    run(root, &["git", "remote", "list"])?
        .lines()
        .find_map(|line| {
            let (name, url) = line.split_once(' ')?;
            (name == remote).then(|| url.trim().to_string())
        })
        .with_context(|| format!("remote {remote:?} not found in `jj git remote list`"))
}

/// Parses the "Changes to push to ..." sections of `jj git push --dry-run`.
/// Any unrecognized line inside a section is an error: guessing would let
/// commits through unchecked.
pub fn parse_dry_run(text: &str) -> Result<Vec<RemotePush>> {
    let mut pushes: Vec<RemotePush> = vec![];
    let mut in_section = false;
    for line in text.lines() {
        if let Some(remote) = line
            .strip_prefix("Changes to push to ")
            .and_then(|r| r.strip_suffix(':'))
        {
            pushes.push(RemotePush {
                remote: unquote(remote)?,
                updates: vec![],
            });
            in_section = true;
        } else if let (true, Some(entry)) = (in_section, line.strip_prefix("  ")) {
            let update = parse_update(entry)
                .with_context(|| format!("unrecognized `jj git push --dry-run` line: {line:?}"))?;
            pushes.last_mut().unwrap().updates.push(update);
        } else {
            in_section = false;
        }
    }
    Ok(pushes)
}

fn parse_update(entry: &str) -> Result<RefUpdate> {
    let (head, desc) = entry
        .strip_suffix(']')
        .and_then(|e| e.rsplit_once(" ["))
        .context("missing [...]")?;
    let (kind, name) = head.split_once(": ").context("missing kind")?;
    let kind = match kind {
        "bookmark" => RefKind::Bookmark,
        "tag" => RefKind::Tag,
        other => bail!("unknown ref kind {other:?}"),
    };
    let words: Vec<&str> = desc.split(' ').collect();
    let (old, new) = match words.as_slice() {
        ["add", "to", new] => (None, Some(new)),
        ["delete", "from", old] => (Some(old), None),
        [
            "move",
            "forward" | "backward" | "sideways",
            "from",
            old,
            "to",
            new,
        ] => (Some(old), Some(new)),
        _ => bail!("unknown update {desc:?}"),
    };
    Ok(RefUpdate {
        kind,
        name: unquote(name)?,
        old: old.map(|s| s.to_string()),
        new: new.map(|s| s.to_string()),
    })
}

/// Undoes jj's symbol quoting for names that aren't plain identifiers.
fn unquote(symbol: &str) -> Result<String> {
    let Some(inner) = symbol.strip_prefix('"').and_then(|s| s.strip_suffix('"')) else {
        return Ok(symbol.to_string());
    };
    let mut out = String::new();
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some(e @ ('"' | '\\')) => out.push(e),
            other => bail!("unsupported escape \\{other:?} in {symbol}"),
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_all_update_kinds() {
        let text = "\
Warning: something unrelated
Changes to push to origin:
  bookmark: feature [add to 1234567890ab]
  bookmark: main [move forward from aaaaaaaaaaaa to bbbbbbbbbbbb]
  bookmark: \"odd name\" [move sideways from cccccccccccc to dddddddddddd]
  bookmark: gone [delete from eeeeeeeeeeee]
  tag: v1 [add to ffffffffffff]
Dry-run requested, not pushing.
";
        let pushes = parse_dry_run(text).unwrap();
        assert_eq!(pushes.len(), 1);
        let p = &pushes[0];
        assert_eq!(p.remote, "origin");
        assert_eq!(
            p.updates[0],
            RefUpdate {
                kind: RefKind::Bookmark,
                name: "feature".into(),
                old: None,
                new: Some("1234567890ab".into())
            }
        );
        assert_eq!(p.updates[1].old.as_deref(), Some("aaaaaaaaaaaa"));
        assert_eq!(p.updates[1].new.as_deref(), Some("bbbbbbbbbbbb"));
        assert_eq!(p.updates[2].name, "odd name");
        assert_eq!(p.updates[3].new, None);
        assert_eq!(p.updates[4].git_ref(), "refs/tags/v1");
    }

    #[test]
    fn parses_multiple_remotes() {
        let text = "\
Changes to push to origin:
  bookmark: a [add to 111111111111]
Changes to push to upstream:
  bookmark: b [add to 222222222222]
";
        let pushes = parse_dry_run(text).unwrap();
        assert_eq!(pushes.len(), 2);
        assert_eq!(pushes[1].remote, "upstream");
        assert_eq!(pushes[1].updates[0].name, "b");
    }

    #[test]
    fn nothing_changed_is_empty() {
        assert!(parse_dry_run("Nothing changed.\n").unwrap().is_empty());
    }

    #[test]
    fn rejects_unknown_lines_inside_a_section() {
        let text = "Changes to push to origin:\n  bookmark: a [teleport to 111111111111]\n";
        assert!(parse_dry_run(text).is_err());
    }
}
