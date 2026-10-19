# shallow: a shallow clone, as a CI checkout makes one: repo.git is `git clone --bare --depth 2`
# of a five-commit repository the script makes first, so it holds the last two commits, the
# older of them listed in repo.git/shallow with its parent missing. Its remote names no
# repository. Refused.

. "$(dirname "$0")/lib.sh"

git init -q -b main
for n in 1 2 3 4 5; do
    printf '%s\n' "$n" > "$n.txt"
    commit_all "commit $n"
done
git clone -q --bare --depth 2 "file://$WORK" "$OUT/repo.git"
# The clone's remote names the scratch directory, which is gone when the script ends.
git --git-dir="$OUT/repo.git" config remote.origin.url file:///nonexistent/shallow-source.git

finish
