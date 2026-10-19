# many_parents: merges of many roots. main is a merge of 255 roots, the most parents a changeset
# holds (issue 8: at most 255); too-many is a merge of 256, one more than the import takes, the
# 255 and one root more. Each root adds a file of its own, root-NNN.txt, and each merge's tree
# holds the files of all its parents.

. "$(dirname "$0")/lib.sh"

git init -q -b main

# The roots, each made without the work tree; the arguments collect their -p options, and
# lines their files, as git mktree reads them.
set --
lines=
n=0
while [ "$n" -lt 256 ]; do
    n=$((n + 1))
    tick
    blob=$(printf 'root %d\n' "$n" | git hash-object -w --stdin)
    line=$(printf '100644 blob %s\troot-%03d.txt' "$blob" "$n")
    tree=$(printf '%s\n' "$line" | git mktree)
    root=$(git commit-tree -m "root $n" "$tree")
    set -- "$@" -p "$root"
    lines="$lines$line
"
    if [ "$n" -eq 255 ]; then
        tick
        tree=$(printf '%s' "$lines" | git mktree)
        merge=$(git commit-tree "$@" -m 'a merge of 255 roots' "$tree")
        git update-ref refs/heads/main "$merge"
    fi
done
tick
tree=$(printf '%s' "$lines" | git mktree)
merge=$(git commit-tree "$@" -m 'a merge of 256 roots' "$tree")
git update-ref refs/heads/too-many "$merge"

finish
