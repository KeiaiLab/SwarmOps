# needs git 2.45
# reftable: a repository whose refs are kept in a reftable (`git init --ref-format=reftable`, new
# in git 2.45), two commits; the import reads refs kept as files only. Refused. An older git skips
# the script (the line above), so it has no archive while the declared git is 2.43.0: the version
# matrix's newest git builds it, and SWARMOPS_GIT_FIXTURES_DIR hands it to the tests.

. "$(dirname "$0")/lib.sh"

git init -q -b main --ref-format=reftable
printf 'one\n' > one.txt
commit_all 'the first commit'
printf 'two\n' > two.txt
commit_all 'the second commit'

finish
