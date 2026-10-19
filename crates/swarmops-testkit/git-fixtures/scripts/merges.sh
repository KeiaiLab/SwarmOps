# merges: main's history holds a --no-ff merge, an octopus of three branches, a merge that adds
# a file only its second parent has, and a criss-cross pair (two branches that each merged the
# other, so that they have two merge bases), both merged; an annotated tag `v1` names the
# octopus merge and a lightweight tag `light` the commit the --no-ff merge reaches only through
# its second parent. Every branch is kept.

. "$(dirname "$0")/lib.sh"

git init -q -b main
printf 'base\n' > base.txt
commit_all 'base'

# A --no-ff merge of a branch that changed base.txt while main added main.txt.
git checkout -q -b feature
printf 'base\nfeature\n' > base.txt
commit_all 'feature: extend base.txt'
git checkout -q main
printf 'main\n' > main.txt
commit_all 'main: add main.txt'
tick
git merge -q --no-ff -m 'merge feature' feature
git tag light feature

# An octopus: main and three branches forked from it, each adding a file of its own.
for branch in one two three; do
    git checkout -q -b "$branch" main
    printf '%s\n' "$branch" > "$branch.txt"
    commit_all "$branch: add $branch.txt"
done
git checkout -q main
tick
git merge -q --no-ff -m 'octopus: merge one, two and three' one two three
git tag -a -m 'v1: the octopus merge' v1

# A merge that adds side.txt, which only its second parent has; main moved meanwhile.
git checkout -q -b side main
printf 'side\n' > side.txt
commit_all 'side: add side.txt'
git checkout -q main
printf 'main, again\n' > main.txt
commit_all 'main: change main.txt'
tick
git merge -q --no-ff -m 'merge side' side

# A criss-cross pair: cross-a merges cross-b's first commit and cross-b merges cross-a's, so
# that the two have two merge bases; main then merges both.
git checkout -q -b cross-a main
printf 'a\n' > cross-a.txt
commit_all 'cross-a: add cross-a.txt'
git checkout -q -b cross-b main
printf 'b\n' > cross-b.txt
commit_all 'cross-b: add cross-b.txt'
git checkout -q cross-a
tick
git merge -q --no-ff -m 'cross-a: merge cross-b' cross-b
git checkout -q cross-b
tick
git merge -q --no-ff -m 'cross-b: merge cross-a' cross-a~1
git checkout -q main
tick
git merge -q --no-ff -m 'merge cross-a' cross-a
tick
git merge -q --no-ff -m 'merge cross-b' cross-b

finish
