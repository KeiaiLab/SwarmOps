<!-- Generated from the code by crates/swarmops-cli/src/reference.rs: do not edit. Change the code, then run SWARMOPS_REGEN_REFERENCE=1 with the regen_reference test. -->

# Error codes

A failed command prints an envelope whose `error` names a stable `E_*` code, the process exit code (`exit`) and whether running the operation again can succeed (`retryable`), with machine-readable `details` and a `hint`. This page is generated from the one table in the code that gives each code its exit code and `retryable` flag; `swarmops schema ErrorCode` prints the same codes and descriptions. Some codes are defined before any operation raises them: [Known limitations](../known-limitations.md) names them.

## Exit codes

| Exit | Meaning | Codes |
| --- | --- | --- |
| 0 | Success. |  |
| 1 | The operation failed. | [`E_EXISTS`](#e_exists), [`E_AMBIGUOUS_ID`](#e_ambiguous_id), [`E_IS_DIR`](#e_is_dir), [`E_TOO_LARGE`](#e_too_large), [`E_EMPTY_COMMIT`](#e_empty_commit), [`E_TYPE_CONFLICT`](#e_type_conflict), [`E_REF_NAME_INVALID`](#e_ref_name_invalid), [`E_STORE_CORRUPT`](#e_store_corrupt), [`E_UNSUPPORTED_FS`](#e_unsupported_fs), [`E_IO`](#e_io), [`E_INTERNAL`](#e_internal), [`E_NOTHING_TO_LAND`](#e_nothing_to_land), [`E_NOTHING_TO_UNDO`](#e_nothing_to_undo), [`E_GIT_IMPORT_LOSSY`](#e_git_import_lossy), [`E_GIT_UNSUPPORTED`](#e_git_unsupported), [`E_GIT_READ`](#e_git_read), [`E_GIT_WRITE`](#e_git_write) |
| 2 | The request or the command line is malformed. | [`E_INVALID_ARG`](#e_invalid_arg) |
| 3 | The change conflicts with what it lands on: resolve it, then run the operation again. | [`E_CONFLICT`](#e_conflict), [`E_GIT_IMPORT_DIVERGED`](#e_git_import_diverged), [`E_GIT_EXPORT_DIVERGED`](#e_git_export_diverged) |
| 4 | The operation can succeed when it is run again. | [`E_CAS_MISMATCH`](#e_cas_mismatch), [`E_IO_TRANSIENT`](#e_io_transient), [`E_BUSY`](#e_busy), [`E_CAS_RETRY_EXHAUSTED`](#e_cas_retry_exhausted), [`E_MAINTENANCE_BUSY`](#e_maintenance_busy) |
| 5 | An identity or policy requirement refused the operation. | [`E_IDENTITY_REQUIRED`](#e_identity_required), [`E_AUTH`](#e_auth), [`E_POLICY_DENIED`](#e_policy_denied) |
| 6 | Something the request names does not exist. | [`E_NOT_A_REPO`](#e_not_a_repo), [`E_REV_NOT_FOUND`](#e_rev_not_found), [`E_PATH_NOT_FOUND`](#e_path_not_found), [`E_UNKNOWN_PARENT`](#e_unknown_parent), [`E_REF_NOT_FOUND`](#e_ref_not_found), [`E_OP_NOT_FOUND`](#e_op_not_found), [`E_GIT_NOT_A_REPO`](#e_git_not_a_repo), [`E_GIT_REF_NOT_FOUND`](#e_git_ref_not_found) |

## Codes

| Code | Exit | Retryable | Description |
| --- | --- | --- | --- |
| [`E_EXISTS`](#e_exists) | 1 | no | `init` found a store at the target path already. |
| [`E_NOT_A_REPO`](#e_not_a_repo) | 6 | no | No store at the given path, or no store was named. |
| [`E_IDENTITY_REQUIRED`](#e_identity_required) | 5 | no | A mutation was asked for without an identity. |
| [`E_REV_NOT_FOUND`](#e_rev_not_found) | 6 | no | A revision names no changeset. |
| [`E_AMBIGUOUS_ID`](#e_ambiguous_id) | 1 | no | An id prefix matches more than one changeset; give more digits. |
| [`E_PATH_NOT_FOUND`](#e_path_not_found) | 6 | no | The path does not exist at that revision. |
| [`E_IS_DIR`](#e_is_dir) | 1 | no | A file operation named a directory. |
| [`E_TOO_LARGE`](#e_too_large) | 1 | no | The content is larger than the operation allows. |
| [`E_EMPTY_COMMIT`](#e_empty_commit) | 1 | no | The commit changes nothing and `allow_empty` is off. |
| [`E_TYPE_CONFLICT`](#e_type_conflict) | 1 | no | A change puts an entry below a file, or a file onto a directory. |
| [`E_UNKNOWN_PARENT`](#e_unknown_parent) | 6 | no | The parent changeset is not in the store. |
| [`E_CAS_MISMATCH`](#e_cas_mismatch) | 4 | yes | The ref no longer holds the expected target (or, with `create`, exists at another one); `details.actual` holds what it does, `null` when the ref does not exist. |
| [`E_REF_NOT_FOUND`](#e_ref_not_found) | 6 | no | No ref by that name. |
| [`E_REF_NAME_INVALID`](#e_ref_name_invalid) | 1 | no | The ref name is not `main` or `release/<name>`. |
| [`E_STORE_CORRUPT`](#e_store_corrupt) | 1 | no | Stored data failed verification. |
| [`E_INVALID_ARG`](#e_invalid_arg) | 2 | no | A malformed argument or request, including a command-line usage error. |
| [`E_UNSUPPORTED_FS`](#e_unsupported_fs) | 1 | no | The store path is not on a native Linux filesystem (for example WSL's `/mnt/*`). |
| [`E_IO`](#e_io) | 1 | no | The filesystem refused an operation: `details.fs_op` names it, and `details.store_file` the file of the store it was on, relative to the store's root. |
| [`E_IO_TRANSIENT`](#e_io_transient) | 4 | yes | A journal append failed, and the store cut what it wrote back off the journal (`details.committed` is `false`): nothing was committed, so running the same operation again is safe. |
| [`E_BUSY`](#e_busy) | 4 | yes | Another writer held the store's metadata lock for the whole wait, or commits landed during every one of a read's attempts to take a snapshot; nothing was committed (a commit's flushed objects may remain, for GC), and running the operation again can succeed. |
| [`E_INTERNAL`](#e_internal) | 1 | no | A bug in SwarmOps, such as a caught panic. |
| [`E_CONFLICT`](#e_conflict) | 3 | no | The change conflicts with the target: `details.report` lists each conflicting file with its kind, its hunks and who changed it on the target. |
| [`E_CAS_RETRY_EXHAUSTED`](#e_cas_retry_exhausted) | 4 | yes | The target moved before every one of the land's attempts could publish; nothing was recorded, and running the land again can succeed. |
| [`E_NOTHING_TO_LAND`](#e_nothing_to_land) | 1 | no | The land has nothing to add to the target: its head is on the target already, or its change is (`details.reason` says which). |
| [`E_NOTHING_TO_UNDO`](#e_nothing_to_undo) | 1 | no | The operation moved no ref, so there is nothing to restore. |
| [`E_OP_NOT_FOUND`](#e_op_not_found) | 6 | no | The store has no operation by that number. |
| [`E_AUTH`](#e_auth) | 5 | no | The caller's token failed verification. |
| [`E_POLICY_DENIED`](#e_policy_denied) | 5 | no | A policy refused the operation for this identity. |
| [`E_MAINTENANCE_BUSY`](#e_maintenance_busy) | 4 | yes | Maintenance (gc or verify) holds the store's maintenance lease. |
| [`E_GIT_NOT_A_REPO`](#e_git_not_a_repo) | 6 | no | `git import` found no git repository it can open at `git_path` (`details.git_path`). |
| [`E_GIT_REF_NOT_FOUND`](#e_git_ref_not_found) | 6 | no | `git import`: the source repository has no `git_ref`, or it names no commit (`details.git_ref`; `details.kind` the object it names instead, such as a tree). |
| [`E_GIT_IMPORT_DIVERGED`](#e_git_import_diverged) | 3 | no | `git import`: the target ref's head is not the import of `git_ref`'s commit or of an ancestor of it, because the source was rewritten or a native changeset was committed on the target, so nothing was written; there is no silent merge. |
| [`E_GIT_IMPORT_LOSSY`](#e_git_import_lossy) | 1 | no | `git import`: a commit holds content a changeset cannot keep as git has it; `details` names the `commit`, the `path` (also as `path_base64` when it is not UTF-8) and the lossy `class`, which the request's `lossy` can allow. |
| [`E_GIT_UNSUPPORTED`](#e_git_unsupported) | 1 | no | `git import` does not read the source: a SHA-256 repository, reftable refs, a shallow clone, a FIFO, a socket or a device where it reads a file of the git directory, a file it reads whole there that is longer than 16 MiB, a commit of more than 255 parents, or an entry whose mode is no kind of entry; `details.reason` says which (`object_format`, `ref_storage`, `shallow`, `special_file`, `file_too_long`, `parents`, `mode`), `details.file` names the file of `special_file` and `file_too_long` by its path in the git directory (`details.size` and `details.max` give the long one's length and the limit), and `details.commit` names the commit of `parents` and `mode`. |
| [`E_GIT_READ`](#e_git_read) | 1 | no | `git import` could not read the source: an object is missing (as in a partial clone), does not hash to its id or does not parse, or the filesystem refused a read (`details.reason`; `details.object` names the object when there is one). |
| [`E_GIT_EXPORT_DIVERGED`](#e_git_export_diverged) | 3 | no | `git export`: the branch holds commits that are not the export of the source ref's head or of an ancestor of it, so the branch did not move; there is no silent merge, and the request's `force` moves it anyway. |
| [`E_GIT_WRITE`](#e_git_write) | 1 | no | `git export` could not write the repository; `details.reason` says what failed: `io` (the filesystem refused a write; `details.path` names the file, relative to the git directory), `ref_locked` (the branch's lock file, `refs/heads/<branch>.lock`, exists: another git process holds it, or an earlier export died holding it, and git itself says to remove it once no git process runs; `details.path` names it) or `ref_name_conflict` (an existing ref stands where the branch's name needs a directory, or the other way round; `details.branch`). |

## E_EXISTS

Exit 1. Retryable: no.

`init` found a store at the target path already.

## E_NOT_A_REPO

Exit 6. Retryable: no.

No store at the given path, or no store was named.

## E_IDENTITY_REQUIRED

Exit 5. Retryable: no.

A mutation was asked for without an identity.

## E_REV_NOT_FOUND

Exit 6. Retryable: no.

A revision names no changeset.

## E_AMBIGUOUS_ID

Exit 1. Retryable: no.

An id prefix matches more than one changeset; give more digits.

## E_PATH_NOT_FOUND

Exit 6. Retryable: no.

The path does not exist at that revision.

## E_IS_DIR

Exit 1. Retryable: no.

A file operation named a directory.

## E_TOO_LARGE

Exit 1. Retryable: no.

The content is larger than the operation allows.

## E_EMPTY_COMMIT

Exit 1. Retryable: no.

The commit changes nothing and `allow_empty` is off.

## E_TYPE_CONFLICT

Exit 1. Retryable: no.

A change puts an entry below a file, or a file onto a directory.

## E_UNKNOWN_PARENT

Exit 6. Retryable: no.

The parent changeset is not in the store.

## E_CAS_MISMATCH

Exit 4. Retryable: yes.

The ref no longer holds the expected target (or, with `create`, exists at another one); `details.actual` holds what it does, `null` when the ref does not exist.

## E_REF_NOT_FOUND

Exit 6. Retryable: no.

No ref by that name.

## E_REF_NAME_INVALID

Exit 1. Retryable: no.

The ref name is not `main` or `release/<name>`.

## E_STORE_CORRUPT

Exit 1. Retryable: no.

Stored data failed verification. A metadata file that does not parse also names `details.fs_op` and `details.store_file`.

## E_INVALID_ARG

Exit 2. Retryable: no.

A malformed argument or request, including a command-line usage error.

## E_UNSUPPORTED_FS

Exit 1. Retryable: no.

The store path is not on a native Linux filesystem (for example WSL's `/mnt/*`).

## E_IO

Exit 1. Retryable: no.

The filesystem refused an operation: `details.fs_op` names it, and `details.store_file` the file of the store it was on, relative to the store's root. A journal append it refused for want of permission or as a read-only filesystem also says `details.committed: false`: nothing was committed, and running the same operation again fails the same way until the hint's cause is fixed.

## E_IO_TRANSIENT

Exit 4. Retryable: yes.

A journal append failed, and the store cut what it wrote back off the journal (`details.committed` is `false`): nothing was committed, so running the same operation again is safe. A failure after the commit point is no error: the operation succeeds, with a warning. `details.fs_op` and `details.store_file` name the append that failed.

## E_BUSY

Exit 4. Retryable: yes.

Another writer held the store's metadata lock for the whole wait, or commits landed during every one of a read's attempts to take a snapshot; nothing was committed (a commit's flushed objects may remain, for GC), and running the operation again can succeed.

## E_INTERNAL

Exit 1. Retryable: no.

A bug in SwarmOps, such as a caught panic.

## E_CONFLICT

Exit 3. Retryable: no.

The change conflicts with the target: `details.report` lists each conflicting file with its kind, its hunks and who changed it on the target. Nothing was recorded; resolve the conflicts, then land again (the `hint` says how).

## E_CAS_RETRY_EXHAUSTED

Exit 4. Retryable: yes.

The target moved before every one of the land's attempts could publish; nothing was recorded, and running the land again can succeed.

## E_NOTHING_TO_LAND

Exit 1. Retryable: no.

The land has nothing to add to the target: its head is on the target already, or its change is (`details.reason` says which).

## E_NOTHING_TO_UNDO

Exit 1. Retryable: no.

The operation moved no ref, so there is nothing to restore.

## E_OP_NOT_FOUND

Exit 6. Retryable: no.

The store has no operation by that number.

## E_AUTH

Exit 5. Retryable: no.

The caller's token failed verification.

## E_POLICY_DENIED

Exit 5. Retryable: no.

A policy refused the operation for this identity.

## E_MAINTENANCE_BUSY

Exit 4. Retryable: yes.

Maintenance (gc or verify) holds the store's maintenance lease.

## E_GIT_NOT_A_REPO

Exit 6. Retryable: no.

`git import` found no git repository it can open at `git_path` (`details.git_path`).

## E_GIT_REF_NOT_FOUND

Exit 6. Retryable: no.

`git import`: the source repository has no `git_ref`, or it names no commit (`details.git_ref`; `details.kind` the object it names instead, such as a tree).

## E_GIT_IMPORT_DIVERGED

Exit 3. Retryable: no.

`git import`: the target ref's head is not the import of `git_ref`'s commit or of an ancestor of it, because the source was rewritten or a native changeset was committed on the target, so nothing was written; there is no silent merge. `details` names `target_ref`, `target_head`, `git_head` and `base`, the git commit the head stands for (the one it holds, or for a native head the one it exports to); and `external_id` (`commit`, `changeset`) where the import passed over an external id entry whose changeset stands for another commit, which `verify --deep` reports.

## E_GIT_IMPORT_LOSSY

Exit 1. Retryable: no.

`git import`: a commit holds content a changeset cannot keep as git has it; `details` names the `commit`, the `path` (also as `path_base64` when it is not UTF-8) and the lossy `class`, which the request's `lossy` can allow. The import stops before that commit, and the target ref does not move.

## E_GIT_UNSUPPORTED

Exit 1. Retryable: no.

`git import` does not read the source: a SHA-256 repository, reftable refs, a shallow clone, a FIFO, a socket or a device where it reads a file of the git directory, a file it reads whole there that is longer than 16 MiB, a commit of more than 255 parents, or an entry whose mode is no kind of entry; `details.reason` says which (`object_format`, `ref_storage`, `shallow`, `special_file`, `file_too_long`, `parents`, `mode`), `details.file` names the file of `special_file` and `file_too_long` by its path in the git directory (`details.size` and `details.max` give the long one's length and the limit), and `details.commit` names the commit of `parents` and `mode`.

## E_GIT_READ

Exit 1. Retryable: no.

`git import` could not read the source: an object is missing (as in a partial clone), does not hash to its id or does not parse, or the filesystem refused a read (`details.reason`; `details.object` names the object when there is one).

## E_GIT_EXPORT_DIVERGED

Exit 3. Retryable: no.

`git export`: the branch holds commits that are not the export of the source ref's head or of an ancestor of it, so the branch did not move; there is no silent merge, and the request's `force` moves it anyway. `details.reason` says why: `branch_ahead` (commits made in git on top of the export of the head itself, which `git import` brings in), `both_moved` (commits made in git on top of an older export, while the source ref moved too), `branch_moved` (the branch names a commit that is no export of this history), `source_moved_back` (the source ref's head is behind the export the branch holds, after an `undo` or a `ref set` backwards), `lossy_history` (the branch holds the original commits of a history imported lossily, which the export would replace) or `moved_during_export` (another writer moved the branch while this export wrote; running the export again settles it, and what it wrote stays). The first five are found before anything is written. `details` also names `branch`, `git_head` (the commit the branch holds), `head_changeset` and, when the branch holds the export of a changeset, `exported_changeset`.

## E_GIT_WRITE

Exit 1. Retryable: no.

`git export` could not write the repository; `details.reason` says what failed: `io` (the filesystem refused a write; `details.path` names the file, relative to the git directory), `ref_locked` (the branch's lock file, `refs/heads/<branch>.lock`, exists: another git process holds it, or an earlier export died holding it, and git itself says to remove it once no git process runs; `details.path` names it) or `ref_name_conflict` (an existing ref stands where the branch's name needs a directory, or the other way round; `details.branch`). Running the export again after the cause is gone writes what is still missing.
