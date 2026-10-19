# corrupt_object: a repository with one damaged object, as a disk or a copy can leave it: the
# blob of damaged.txt is a loose object whose file was replaced by a copy of other.txt's
# (`cp`), so that what it holds hashes to other.txt's id, not its own. other.txt comes in the
# first commit, damaged.txt in the second; everything else is intact. Refused (E_GIT_READ).

. "$(dirname "$0")/lib.sh"

git init -q -b main
printf 'other\n' > other.txt
commit_all 'add other.txt'
printf 'damaged\n' > damaged.txt
commit_all 'add damaged.txt'
damaged=$(git rev-parse HEAD:damaged.txt)
other=$(git rev-parse HEAD:other.txt)

finish --keep-loose

# loose <id>: the file of the loose object <id>.
loose() {
    printf '%s/objects/%s/%s' "$OUT/repo.git" "${1%"${1#??}"}" "${1#??}"
}
rm -f "$(loose "$damaged")"
cp "$(loose "$other")" "$(loose "$damaged")"
