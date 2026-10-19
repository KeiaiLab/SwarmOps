# empty_trees: a tree entry that names the empty tree, crafted, since git's index holds no empty
# directory and git never writes one: a/empty, beside the file a/keep.txt, so that dropping it
# leaves a/ as it was before. Class empty_trees.

. "$(dirname "$0")/lib.sh"

git init -q -b main
mkdir a
printf 'keep\n' > a/keep.txt
commit_all 'add a/keep.txt'

empty=$(git hash-object -t tree -w /dev/null)
a=$(
    {
        entry 40000 empty "$empty"
        entry 100644 keep.txt "$(git rev-parse HEAD:a/keep.txt)"
    } | craft tree -
)
tree=$(entry 40000 a "$a" | craft tree -)
tick
git update-ref HEAD "$(git commit-tree -p HEAD -m 'add a/empty, the empty tree' "$tree")"

finish
