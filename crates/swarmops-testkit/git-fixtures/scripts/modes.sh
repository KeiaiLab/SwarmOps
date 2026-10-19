# modes: what a tree entry's mode and name can be: an exec bit set and cleared, a symlink, a
# symlink to a directory, a file `a.b` beside a directory `a` (git's tree order puts `a.b`
# first, as if the directory were `a/`; byte order puts `a` first), and a path with a space and
# a UTF-8 `é` in it.

. "$(dirname "$0")/lib.sh"

git init -q -b main
printf '#!/bin/sh\necho run\n' > run.sh
commit_all 'add run.sh, not executable'

chmod +x run.sh
commit_all 'set the exec bit of run.sh'

chmod -x run.sh
commit_all 'clear the exec bit of run.sh'

printf 'target\n' > target.txt
ln -s target.txt link
commit_all 'add link, a symlink to target.txt'

mkdir dir
printf 'x\n' > dir/x.txt
ln -s dir dirlink
commit_all 'add dirlink, a symlink to the directory dir'

mkdir a
printf 'inner\n' > a/inner.txt
printf 'a.b\n' > a.b
commit_all 'add the file a.b beside the directory a'

mkdir 'with space'
printf 'caf\303\251\n' > "$(printf 'with space/caf\303\251.txt')"
commit_all 'add a path with a space and a UTF-8 e-acute'

finish
