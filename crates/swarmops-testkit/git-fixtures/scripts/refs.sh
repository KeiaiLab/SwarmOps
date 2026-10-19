# refs: branches for the import's incremental and divergence tests. older is three commits;
# newer, and main, older and three commits more; rewritten forks from older's second commit,
# makes its third again otherwise and adds one, so it leaves older's tip out; merged forks from
# there too, adds a commit and merges older, so its second parent is older's tip and its first
# parent a branch forked before it. Faithful.

. "$(dirname "$0")/lib.sh"

git init -q -b main
for n in 1 2 3; do
    printf '%s\n' "$n" > count.txt
    commit_all "count to $n"
done
fork=$(git rev-parse HEAD~1)
git branch older
for n in 4 5 6; do
    printf '%s\n' "$n" > count.txt
    commit_all "count to $n"
done
git branch newer

git checkout -q -b rewritten "$fork"
printf '3, rewritten\n' > count.txt
commit_all 'count to 3, rewritten'
printf '4, rewritten\n' > count.txt
commit_all 'count to 4, rewritten'

git checkout -q -b merged "$fork"
printf 'side\n' > side.txt
commit_all 'add side.txt'
tick
git merge -q --no-ff -m 'merge older' older

git checkout -q main

finish
