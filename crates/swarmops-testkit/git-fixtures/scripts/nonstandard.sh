# nonstandard: commits whose bytes a changeset cannot give back, crafted, one oddity each and
# nothing ordinary, the first a root: an author line with two spaces before its `<`; an offset
# written +05:30; a header line without a value; no blank line after the headers, so no
# message; a parent listed twice. Each adds a file. Class nonstandard.

. "$(dirname "$0")/lib.sh"

git init -q -b main

# crafted <file> <parents and idents> <rest>: <file> added, then a commit of the index crafted
# from the tree line, the second argument's lines and the third's bytes, which follow them as
# they are; main moves to it.
crafted() {
    printf '%s\n' "$1" > "$1"
    git add "$1"
    tick
    commit=$(printf 'tree %s\n%s\n%s' "$(git write-tree)" "$2" "$3" | craft commit -)
    git update-ref HEAD "$commit"
}

author="author Fixture Author <author@fixture.invalid>"
committer="committer Fixture Committer <committer@fixture.invalid>"

crafted one.txt \
    "author Fixture Author  <author@fixture.invalid> $next_tick +0000
$committer $next_tick +0000" \
    '
two spaces before the < of the author line
'

crafted two.txt \
    "parent $(git rev-parse HEAD)
$author $next_tick +05:30
$committer $next_tick +0000" \
    '
an offset written +05:30
'

crafted three.txt \
    "parent $(git rev-parse HEAD)
$author $next_tick +0000
$committer $next_tick +0000
x-no-value" \
    '
a header line without a value
'

crafted four.txt \
    "parent $(git rev-parse HEAD)
$author $next_tick +0000
$committer $next_tick +0000" \
    ''

crafted five.txt \
    "parent $(git rev-parse HEAD)
parent $(git rev-parse HEAD)
$author $next_tick +0000
$committer $next_tick +0000" \
    '
a parent listed twice
'

finish
