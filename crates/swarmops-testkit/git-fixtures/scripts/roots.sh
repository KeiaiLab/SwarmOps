# roots: a history with three roots and the messages git's porcelain does not write. main starts
# at a root, joins a second root (`merge --allow-unrelated-histories`), holds an `--allow-empty`
# commit, joins a third root whose tree is the empty tree, then a commit whose message has no
# final line feed (`commit-tree` takes it from stdin as it is) and one whose message is empty
# (`--allow-empty-message`). HEAD reaches every commit. Faithful.

. "$(dirname "$0")/lib.sh"

git init -q -b main
printf 'first\n' > first.txt
commit_all 'the first root'

# The second root, made beside main without its work tree: one file, second.txt.
tick
blob=$(printf 'second\n' | git hash-object -w --stdin)
tree=$(printf '100644 blob %s\tsecond.txt\n' "$blob" | git mktree)
root=$(git commit-tree -m 'the second root' "$tree")
tick
git merge -q --allow-unrelated-histories --no-ff -m 'join the second root' "$root"

tick
git commit -q --allow-empty -m 'an empty commit: the tree of its parent'

# The third root, whose tree is the empty tree.
tick
empty=$(git hash-object -t tree -w /dev/null)
root=$(git commit-tree -m 'the third root, of the empty tree' "$empty")
tick
git merge -q --allow-unrelated-histories --no-ff -m 'join the third root' "$root"

printf 'third\n' > third.txt
git add third.txt
tick
commit=$(printf 'a message without a final line feed' | git commit-tree -p HEAD "$(git write-tree)")
git update-ref HEAD "$commit"

printf 'fourth\n' > fourth.txt
git add fourth.txt
tick
git commit -q --allow-empty-message -m ''

finish
