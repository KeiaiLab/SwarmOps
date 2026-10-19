# partial_clone: a partial clone without its blobs: repo.git is `git clone --bare
# --filter=blob:none` of a three-commit repository the script makes first, which allows the
# filter (uploadpack.allowFilter); it holds every commit and tree and no blob. Its remote, the
# promisor git would fetch a missing blob from, names no repository. Refused (E_GIT_READ).

. "$(dirname "$0")/lib.sh"

git init -q -b main
git config uploadpack.allowFilter true
mkdir dir
for n in 1 2 3; do
    printf '%s\n' "$n" > "dir/$n.txt"
    commit_all "commit $n"
done
git clone -q --bare --filter=blob:none "file://$WORK" "$OUT/repo.git"
# The clone's remote names the scratch directory, which is gone when the script ends.
git --git-dir="$OUT/repo.git" config remote.origin.url file:///nonexistent/partial_clone-source.git

finish
