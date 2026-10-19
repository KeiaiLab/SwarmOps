# tree_order: trees out of git's order, crafted, since git sorts every tree it writes; after an
# ordinary commit, one root tree each commit: b.txt before a.txt, out of any order; the
# directory a before the file a.b, which is byte order but not git's (git compares a directory
# as its name and a `/`, so a.b comes first); the name dup.txt twice, naming two different
# files. Class tree_order.

. "$(dirname "$0")/lib.sh"

git init -q -b main
printf 'a\n' > a.txt
printf 'b\n' > b.txt
commit_all 'add a.txt and b.txt'
a=$(git rev-parse HEAD:a.txt)
b=$(git rev-parse HEAD:b.txt)

# commit_tree <tree> <message>: a commit of <tree> on main, at the next tick.
commit_tree() {
    tick
    git update-ref HEAD "$(git commit-tree -p HEAD -m "$2" "$1")"
}

tree=$(
    {
        entry 100644 b.txt "$b"
        entry 100644 a.txt "$a"
    } | craft tree -
)
commit_tree "$tree" 'b.txt before a.txt'

inner=$(entry 100644 inner.txt "$a" | craft tree -)
tree=$(
    {
        entry 40000 a "$inner"
        entry 100644 a.b "$b"
    } | craft tree -
)
commit_tree "$tree" 'the directory a before the file a.b: byte order, not git order'

tree=$(
    {
        entry 100644 dup.txt "$a"
        entry 100644 dup.txt "$b"
    } | craft tree -
)
commit_tree "$tree" 'dup.txt twice, naming a.txt and then b.txt'

finish
