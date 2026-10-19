# modes_odd: tree entries whose modes git no longer writes, crafted: a file of 100664 (group
# writable, as the first versions of git wrote one), a file of 100600, and a directory written
# 040000, zero-padded, where git writes 40000. Beside them a file of the usual 100644, in one
# commit after an ordinary one. Class modes.

. "$(dirname "$0")/lib.sh"

git init -q -b main
printf 'plain\n' > plain.txt
commit_all 'add plain.txt'

group=$(printf 'group\n' | git hash-object -w --stdin)
private=$(printf 'private\n' | git hash-object -w --stdin)
inner=$(printf 'inner\n' | git hash-object -w --stdin)
padded=$(entry 100644 inner.txt "$inner" | craft tree -)
tree=$(
    {
        entry 100664 group.txt "$group"
        entry 040000 padded "$padded"
        entry 100644 plain.txt "$(git rev-parse HEAD:plain.txt)"
        entry 100600 private.txt "$private"
    } | craft tree -
)
tick
git update-ref HEAD "$(git commit-tree -p HEAD -m 'add files of 100664 and 100600, a directory of 040000' "$tree")"

finish
