# tatami

Run your repo's git hooks (pre-commit, prek, lefthook, hk, husky, plain
`.git/hooks` scripts, ...) when you push from [jj](https://github.com/jj-vcs/jj).

jj never runs git hooks: it doesn't create commits through git, and
`jj git push` passes `--no-verify`. With tatami, `jj push` runs your hooks
against every commit you're about to push, each one on its own, and only
pushes if they pass.

## Getting started

### 1. Requirements

- jj (tested with 0.45.1) and git, on macOS or Linux.
- A **colocated** repo, with `.git/` next to `.jj/`. This is the default for
  `jj git init` and `jj git clone`, and most hook managers need it to install.

### 2. Install tatami

```sh
cargo install --git https://github.com/mcginleyr1/tatami
```

### 3. Install your hooks as usual

tatami runs the hooks your hook manager installs, so install them the normal
way, inside the jj repo:

| Hook manager | Install command |
| --- | --- |
| pre-commit | `pre-commit install` (add `--hook-type commit-msg --hook-type pre-push` if you use those stages) |
| prek | `prek install` (same `--hook-type` flags) |
| lefthook | `lefthook install` |
| hk | `hk install` |
| husky | `npx husky` (usually run for you by `"prepare": "husky"` in `package.json`) |
| simple-git-hooks | `npx simple-git-hooks` |
| overcommit | `overcommit --install` |
| plain scripts | put executable files in `.git/hooks/` |

### 4. Add the `jj push` alias

```sh
tatami install          # for this repo
tatami install --user   # or for every repo
```

### 5. Push

```sh
jj push -b my-feature
```

`jj push` takes the same arguments as `jj git push`. It runs your hooks, then
`jj git push`. If any hook fails, nothing is pushed.

## Everyday use

```sh
jj push -b my-feature              # check, then push
jj push -c @-                      # anything `jj git push` accepts
jj push -b my-feature --fix        # apply formatter fixes, then check and push
jj push -b my-feature --no-verify  # skip the hooks
tatami check                       # just run the hooks on trunk()..@
tatami check -r 'mutable()'        # ...or on any revset
tatami fix                         # apply the hooks' fixes to trunk()..@
```

On each push, tatami runs:

- **pre-commit** hooks for every commit being pushed, with exactly that
  commit's changes staged;
- **commit-msg** hooks against each commit's jj description;
- **pre-push** hooks once for each pushed commit, with the same arguments
  and stdin git would pass.

## When a check fails

**A hook failed on a commit.** tatami names the commit:

```
tatami: pre-commit hooks failed for qpvuntsmwlqt add login form
```

If it's something a formatter or fixer hook can fix (trailing whitespace,
prettier, `ruff --fix`, ...), run `tatami fix` or push with `--fix`. Each
commit is rewritten with exactly what its hooks changed, and commits on top
of it get their own fixed versions, so there are no rebase conflicts.

Anything else (a failing linter, a type error) you fix by hand, as usual with
jj. Edit the files in your working copy, then either:

- run `jj absorb` to move each fix into the commit that last touched those
  lines; or
- run `jj squash --into qpvuntsmwlqt` to move all of `@` into that commit.

Then run `jj push` again.

**`.pre-commit-config.yaml exists but no git hooks are installed`.** Your repo
expects hooks, but none would run. tatami refuses rather than pass silently.
Run your hook manager's install command (step 3).

**`refusing to check N commits (limit 100)`.** This usually happens on a first
push to a remote that has none of your history. Check what matters with
`tatami check -r '<revset>'`, then push with `--no-verify`.

**`... has unresolved conflicts`.** Resolve the conflict first. Hooks would
see jj's conflict encoding, not your files.

**`unrecognized jj git push --dry-run line`.** Your jj version prints
something tatami doesn't understand. It stops rather than guess. Use
`--no-verify` and please open an issue.

**A hook can't find a tool.** Hooks run inside your repo's `.jj/` directory,
so mise, asdf and `.tool-versions` resolve as they do in your project.
Top-level `node_modules` and `.venv` are linked in. Tools that live only in
nested directories, such as `packages/*/node_modules`, may not be found.

## How it works

For each commit, tatami builds a scratch git repository under `.jj/`. It
borrows your repo's objects instead of copying them. In it, the commit's tree
is checked out and staged, with HEAD at its parent. That is exactly what a
hook sees during `git commit`. tatami then runs your installed hooks there
with `git hook run`, and deletes the scratch repo afterwards.

Because of this, every hook manager's own "staged files" logic sees exactly
that commit's changes, wherever your working copy is. Hook config files are
read from the commit being checked.

`tatami fix` runs the same way, but keeps what the hooks rewrite, staging and
rerunning them as you would after a fixer stops a `git commit`. It then has
jj's own [`jj fix`](https://docs.jj-vcs.dev/latest/config/#code-formatting-and-other-file-content-transformations)
write those files into the commits, with tatami as the only fix tool for that
run. Your own `[fix.tools]` aren't run.

## Limitations

- Plain `jj git push`, GUIs and IDEs bypass tatami. Use `jj push`, and keep CI
  as the final gate.
- Only `tatami fix` (or `--fix`) writes hook changes back, and only edits to
  existing files. Hooks that create or delete files, or commit-msg hooks that
  add trailers, don't change your commits.
- `pre-push` isn't run for bookmark deletions.
- Hooks that look at branch names see a detached HEAD.
- Tested with plain hook scripts and the pre-commit framework. prek, lefthook,
  hk, husky with lint-staged and overcommit should behave the same, but aren't
  covered by tests yet.

## Development

```sh
cargo test     # needs jj and git; the pre-commit test also uses `uvx`
cargo clippy --all-targets -- -D warnings
```
