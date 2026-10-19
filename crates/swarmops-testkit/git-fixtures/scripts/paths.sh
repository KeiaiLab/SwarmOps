# paths: names a changeset cannot hold, whose paths are UTF-8 and never `.` or `..`: a file whose
# name is Latin-1, caf\351.txt (git takes a name's bytes as they are); then a directory whose
# name is Latin-1, r\351pertoire, holding a file; then, crafted, since git refuses to write it,
# a tree entry named `..`, a file, beside the others. One class each commit, in that order.
# Class paths.

. "$(dirname "$0")/lib.sh"

git init -q -b main
printf 'plain\n' > plain.txt
commit_all 'add plain.txt'

cafe=$(printf 'caf\351.txt')
printf 'caf\n' > "$cafe"
commit_all 'add a file whose name is Latin-1'

dir=$(printf 'r\351pertoire')
mkdir "$dir"
printf 'inner\n' > "$dir/inner.txt"
commit_all 'add a directory whose name is Latin-1'

# The root tree once more, with the file `..` first, where git's order puts it: the other names
# start with c, p and r, which all come after `.`.
dotdot=$(printf 'dot dot\n' | git hash-object -w --stdin)
tree=$(
    {
        entry 100644 .. "$dotdot"
        entry 100644 "$cafe" "$(git rev-parse "HEAD:$cafe")"
        entry 100644 plain.txt "$(git rev-parse HEAD:plain.txt)"
        entry 40000 "$dir" "$(git rev-parse "HEAD:$dir")"
    } | craft tree -
)
tick
git update-ref HEAD "$(git commit-tree -p HEAD -m 'add the file .., crafted' "$tree")"

finish
