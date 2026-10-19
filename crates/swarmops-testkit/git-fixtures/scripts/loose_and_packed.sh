# loose_and_packed: a repository as it is between two `git gc`s: three commits, a branch and a
# lightweight tag packed (`repack -ad`, `pack-refs --all`); then two commits, a branch and a
# tag written after them, loose, and main a loose ref over its stale packed one. finish
# --keep-loose leaves it so. Faithful.

. "$(dirname "$0")/lib.sh"

git init -q -b main
for n in 1 2 3; do
    printf '%s\n' "$n" > "$n.txt"
    commit_all "commit $n, packed"
done
git branch packed-branch
git tag packed-tag
git -c pack.threads=1 repack -adq
git pack-refs --all

for n in 4 5; do
    printf '%s\n' "$n" > "$n.txt"
    commit_all "commit $n, loose"
done
git branch loose-branch
git tag loose-tag

finish --keep-loose
