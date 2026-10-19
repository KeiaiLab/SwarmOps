# linear: five commits on main, one after the other: an add, a modify, a delete, a nested path
# (a/b/c.txt), and an empty file beside a binary one that holds NUL bytes.

. "$(dirname "$0")/lib.sh"

git init -q -b main
printf 'hello\n' > hello.txt
printf 'goodbye\n' > goodbye.txt
commit_all 'add hello.txt and goodbye.txt'

printf 'hello, world\n' > hello.txt
commit_all 'modify hello.txt'

git rm -q goodbye.txt
commit_all 'delete goodbye.txt'

mkdir -p a/b
printf 'c\n' > a/b/c.txt
commit_all 'add a/b/c.txt'

: > empty
printf 'bin\000ary\000\001\002\377\n' > data.bin
commit_all 'add an empty file and a binary file'

finish
