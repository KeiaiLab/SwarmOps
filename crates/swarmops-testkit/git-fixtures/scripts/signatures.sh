# signatures: what an author or committer line can say, each kept as written. git writes the
# first four: author and committer at the same time; then a minute apart, at the offsets +0530
# and -1200, -1200 and +1400 (the farthest from UTC), +1400 and +0000. The last three are
# crafted, since git writes none of them: the offset -0000; the time -86400, a day before 1970;
# an author name in Latin-1 (git's commit turns a name that is not UTF-8 into UTF-8). Faithful.

. "$(dirname "$0")/lib.sh"

git init -q -b main

# signed <author offset> <committer offset> <message>: the work tree committed at this tick,
# its author and committer dates at those offsets, the committer a minute after the author.
signed() {
    tick
    git add -A
    GIT_AUTHOR_DATE="${GIT_AUTHOR_DATE% *} $1" GIT_COMMITTER_DATE="@$next_tick $2" \
        git commit -q -m "$3"
}

# crafted <author line> <message>: the work tree committed at this tick by a commit crafted
# with that author line; its committer line is git's.
crafted() {
    tick
    git add -A
    commit=$(
        {
            printf 'tree %s\n' "$(git write-tree)"
            printf 'parent %s\n' "$(git rev-parse HEAD)"
            printf 'author %s\n' "$1"
            printf 'committer %s\n' "$(git var GIT_COMMITTER_IDENT)"
            printf '\n%s\n' "$2"
        } | craft commit -
    )
    git update-ref HEAD "$commit"
}

printf 'one\n' > one.txt
commit_all 'author and committer at the same time, at +0000'

printf 'two\n' > two.txt
signed +0530 -1200 'authored at +0530, committed a minute later at -1200'

printf 'three\n' > three.txt
signed -1200 +1400 'authored at -1200, committed a minute later at +1400'

printf 'four\n' > four.txt
signed +1400 +0000 'authored at +1400, committed a minute later at +0000'

# $next_tick is the time the tick of `crafted` gives its committer.
printf 'five\n' > five.txt
crafted "Fixture Author <author@fixture.invalid> $next_tick -0000" \
    'authored at -0000, a zero offset written with a minus sign'

printf 'six\n' > six.txt
crafted 'Fixture Author <author@fixture.invalid> -86400 +0000' \
    'authored at -86400, a day before 1970'

printf 'seven\n' > seven.txt
crafted "$(printf 'Ren\351 Fixture <author@fixture.invalid> %s +0000' "$next_tick")" \
    'an author name in Latin-1'

finish
