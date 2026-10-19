# replace_ref: a replacement ref, which git's porcelain follows and the import does not: main is
# c1, c2, c3, and `git replace <c2> <c2'>` makes refs/replace/<c2> name c2', a commit of c2's
# parent with another file and message. The manifest (GIT_NO_REPLACE_OBJECTS=1) and the import
# read the original c2. Faithful.

. "$(dirname "$0")/lib.sh"

git init -q -b main
printf 'one\n' > file.txt
commit_all 'c1'
printf 'two\n' > file.txt
commit_all 'c2'
c2=$(git rev-parse HEAD)
printf 'three\n' > file.txt
commit_all 'c3'

tick
blob=$(printf 'replaced\n' | git hash-object -w --stdin)
tree=$(printf '100644 blob %s\tfile.txt\n' "$blob" | git mktree)
replacement=$(git commit-tree -p "$c2^" -m "c2', which replaces c2" "$tree")
git replace "$c2" "$replacement"

finish
