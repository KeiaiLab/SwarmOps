# sha256: a repository of the SHA-256 object format (`git init --object-format=sha256`), two
# commits; the import reads SHA-1 repositories only. Refused.

. "$(dirname "$0")/lib.sh"

git init -q -b main --object-format=sha256
printf 'one\n' > one.txt
commit_all 'the first commit'
printf 'two\n' > two.txt
commit_all 'the second commit'

finish
