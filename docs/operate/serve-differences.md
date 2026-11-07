# Where serve answers differently from git

`swarmops serve` speaks git's own protocol, so stock git should not tell it from git's server. A
test of the build checks that: one scripted session runs against a served store and against
`git upload-pack` and `git receive-pack` on a bare repository holding the same branches and tags,
and compares each command's exit status, the refs it reports and the objects the client holds
afterwards. The session clones, lists refs, makes a clone of depth 1 and deepens it, fetches,
pushes a new branch, forces a push onto `main`, deletes the branch, and lands. The bare repository
refuses a push to `main` that is not a fast-forward, as a protected branch on a hosting service
does.

Any difference not in the table below fails the build, naming the command and the ref or object.
A ref of a push is its status line (`git push --porcelain`), and `advertised <ref>` is what the
server lists right after the push. Each row covers the ref or object its first code span names,
`*` standing for any characters.

| Command | Field | Difference | Reason |
| --- | --- | --- | --- |
| `PushNonFastForward` | `refs` | `refs/heads/main`: refused by both; serve's status reads `pre-receive hook declined`, git's `non-fast-forward` | serve decides every ref of a push in its pre-receive hook, so git reports the hook; serve's reason, `E_POLICY_DENIED` here, is on a `remote:` line ([Refusals a client sees](serve-over-ssh.md#refusals-a-client-sees)) |
| `PushLand` | `refs` | `advertised refs/land/*`: absent on serve; git's server keeps the pushed commit there | a push to `refs/land/<target>` is a land: serve squashes it onto the target and leaves nothing under `refs/land/` ([What a client types](serve-over-ssh.md#what-a-client-types)) |
| `PushLand` | `refs` | `advertised refs/heads/main`: serve's is the landed commit; git's has not moved | the land moves the target to a new commit whose parent is the old head |
| `PushLand` | `refs` | `advertised HEAD`: as `refs/heads/main` | `HEAD` names `main` on both |
