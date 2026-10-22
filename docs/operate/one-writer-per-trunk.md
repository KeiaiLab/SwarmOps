# One writer per trunk

Commands and outputs here follow the
[conventions of the operations guide](../operations.md#how-to-read-these-pages).

## Before you start

- The git side, `GIT_SIDE`, and the `field` helper of the [daily loop](daily-loop.md)
  ([once](daily-loop.md#once-bring-the-repository-in), [step 2](daily-loop.md#2-agents-commit-and-land)).

## The rule

Between an import and the next move of the trunk (step 5), only one side may change the trunk, the
store (through the review branch) or git.

## Change the trunk without breaking the rule

People change it in one of three ways that keep to this:

- through SwarmOps, with an identity of their own, as an agent does; the review covers their changes
  too;
- on the trunk in git, after step 5 and before an agent lands again, which step 1 then takes in;
- on a branch, which you import into a ref of its own and land:

```sh
git -C "$GIT_SIDE" fetch origin feature/farewell:feature/farewell
swarmops git import "$GIT_SIDE" --ref feature/farewell --into release/farewell
swarmops land release/farewell --key "$(uuidgen)"
swarmops ref delete release/farewell --expect "$(swarmops resolve release/farewell | field result.id)"
```

The land squashes the branch's commits into one changeset on `main`, whose author is the author of
the branch's last commit and whose committer is you; the next export takes it to the review branch,
and after the review the push of step 5 takes it to the trunk as a fast-forward:

```sh
swarmops git export "$GIT_SIDE" --ref main --branch review
git -C "$GIT_SIDE" push origin review
git -C "$GIT_SIDE" log -1 --format="%an <%ae> / %cn <%ce>%n%B" review
```

```text
{"ok":true,"op":"git_export","api":1,"result":{"exported":1,"objects_written":1,"head_changeset":"813dcf7f…","head_sha":"c19cdeac0daf33835ee6c3d470724450e4229a30","branch":"review","old_sha":"7073a6601acef10a0091e2b7737916399735e721","branch_moved":true,"incremental":true,"rewritten":0},…}
To …
   0eb07c3..c19cdea  review -> review
Ada Lovelace <ada@example.com> / operator/ada <ada@operator.swarmops.local>
farewell: say goodbye

SwarmOps-Changeset: 813dcf7f16416001a5d5ef4815cf9d4c073250d76403db6ed4add70b19bce83f
SwarmOps-Land-Key: a3921b5a-d809-4003-ad53-b77894dcfb22
SwarmOps-Squashed-From: c2ec3b59e9030b9a5c0ff8c0bea98f546e43d45e8a057c2568d9e5255eba469f
```

## Tell a divergence from a lagging trunk

When both sides moved anyway (here an agent landed a change and exported it, and before the review
ended a person pushed one to the trunk), nothing is merged: the push of step 5 is rejected as no
fast-forward, and step 1's import fails with `E_GIT_IMPORT_DIVERGED` (exit 3) and writes nothing:

```sh
git -C "$GIT_SIDE" push origin review:stable
git -C "$GIT_SIDE" fetch origin stable:stable
swarmops git import "$GIT_SIDE" --ref stable --into main --pretty
```

```text
To …
 ! [rejected]        review -> stable (fetch first)
error: failed to push some refs to '…'
hint: Updates were rejected because the remote contains work that you do not
…
From …
   7073a66..9745112  stable     -> stable
{
  "ok": false,
  "op": "git_import",
  "api": 1,
  "error": {
    "code": "E_GIT_IMPORT_DIVERGED",
    "message": "main has diverged from the git history: its head 3d264e8e03aa34acbb82049c1b6d906c418fe35839e22b33d359b56e8b326e4e stands for git commit d9d94398b96c722eb2afab2148198c0fd17a2d5b, which the tip 9745112402b9a55c434417c1d9228ba4d3f78985 does not reach",
    "retryable": false,
    "exit": 3,
    "details": {
      "base": "d9d94398b96c722eb2afab2148198c0fd17a2d5b",
      "git_head": "9745112402b9a55c434417c1d9228ba4d3f78985",
      "target_head": "3d264e8e03aa34acbb82049c1b6d906c418fe35839e22b33d359b56e8b326e4e",
      "target_ref": "main"
    },
    "hint": "swarmops log main"
  },
  …
}
```

The import fails the same way while an export merely waits for its review, so look at what the trunk
has that the review branch lacks: nothing if the trunk only lags, and here the person's commit:

```sh
git -C "$GIT_SIDE" log --oneline review..stable
```

```text
9745112 release: 1.0.2
```

## Put the store's work on top of git's

To keep git's history as it is, with no forced push to the trunk, put the store's new work on top of
git's: import the trunk into a ref of its own, land `main`'s changes onto it (`land` starts from the
changeset both sides share, the merge base of the two), move `main` there, and export. The review
branch still holds the export of the old line, and `main` no longer descends from it, so the export
refuses (`E_GIT_EXPORT_DIVERGED`, reason `source_moved_back`) until it is forced, and the push is
forced too: the branch is a proposal that nothing builds on, and the new line goes through the
review again.

```sh
swarmops git import "$GIT_SIDE" --ref stable --into release/git
HEAD_WAS=$(swarmops resolve main | field result.id)
LANDED=$(swarmops land main --target release/git --key "$(uuidgen)" | field result.landed)
swarmops ref set main "$LANDED" --expect "$HEAD_WAS"
swarmops git export "$GIT_SIDE" --ref main --branch review --force
git -C "$GIT_SIDE" push --force origin review
swarmops ref delete release/git --expect "$LANDED"
```

```text
{"ok":true,"op":"git_export","api":1,"result":{"exported":1,"objects_written":2,"head_changeset":"c250bd46…","head_sha":"6f4295e40f9f8db8449ff18cc036637abb4dac8d","branch":"review","old_sha":"d9d94398b96c722eb2afab2148198c0fd17a2d5b","branch_moved":true,"incremental":true,"rewritten":0,"warnings":["the branch review had diverged (source_moved_back), and the export moved it all the same"]},…}
To …
 + d9d9439...6f4295e review -> review (forced update)
```

That land squashes all of `main`'s changes since the shared changeset into one changeset. The other
way is to give up the commits made in git: a forced push of the review branch to the trunk
(`git push --force origin review:stable`, and `git fetch origin +stable:stable` after it) replaces
the trunk with the store's work, and the commits made in git are gone from it.

## Export refusals and their reasons

An export that cannot continue its branch fails with `E_GIT_EXPORT_DIVERGED` and a reason, each with
its hint: `both_moved` (git and `main` both moved past the branch's last export, and nothing is
merged), `branch_ahead` (commits made in git on top of the export: an import of the branch takes
them in), `branch_moved` (the branch names no export of this history), `source_moved_back` (`main`
moved back, as `op log` shows), `lossy_history` (the branch holds the commits a lossy import was
made of: export to another branch) and `moved_during_export` (run it again). `--force` moves the
branch all the same, with a warning. An export that finds `refs/heads/<branch>.lock` in the git
side, which a git process holds or a dead one left, fails with `E_GIT_WRITE` (`ref_locked`) until
the file is removed, once no git process runs.
