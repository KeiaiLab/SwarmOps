# The helpers every fixture script shares. A script starts with
#
#     . "$(dirname "$0")/lib.sh"
#
# and is run as `sh <name>.sh <out>` by swarmops_testkit::git, in its pinned environment: no
# system or global git configuration, an empty HOME, TZ=UTC, LC_ALL=C, and a fixed author and
# committer. What the script leaves in <out> is the fixture; it works in a scratch directory
# of its own, the current directory when it starts, which is removed when it exits.
# POSIX sh only: the fixtures are rebuilt by every git of the version matrix, on any runner.

set -eu

if [ "$#" -ne 1 ]; then
    echo "usage: sh $0 <out>" >&2
    exit 2
fi
mkdir -p "$1"
OUT=$(cd "$1" && pwd)
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT
cd "$WORK"

# The author and committer dates: 1700000000 +0000 until the first tick, then one tick later
# each time, 60 s apart, so that every run of a script makes the same objects.
next_tick=1700000000
GIT_AUTHOR_DATE="@$next_tick +0000"
GIT_COMMITTER_DATE="@$next_tick +0000"
export GIT_AUTHOR_DATE GIT_COMMITTER_DATE

# tick: sets the author and committer dates of the next commit or tag, 60 s after the last.
tick() {
    GIT_AUTHOR_DATE="@$next_tick +0000"
    GIT_COMMITTER_DATE="@$next_tick +0000"
    export GIT_AUTHOR_DATE GIT_COMMITTER_DATE
    next_tick=$((next_tick + 60))
}

# commit_all <message>: one tick, then the whole work tree committed with that message.
commit_all() {
    tick
    git add -A
    git commit -q -m "$1"
}

# craft <type> <file>: writes <file>'s bytes as an object of <type> that git would not make
# itself (a tree out of order, a commit with an odd header), and prints its id.
craft() {
    git hash-object -t "$1" -w --literally "$2"
}

# finish [--keep-loose]: the repository becomes the fixture's <out>/repo.git, bare, holding
# only what its refs reach: its index and reflogs go, and the objects only they or nothing
# reach (an octopus merge leaves the trees of its steps behind, which ones depending on git's
# version). Unless --keep-loose, its objects are then packed into one pack and its refs into
# packed-refs.
finish() {
    if [ ! -e "$OUT/repo.git" ]; then
        mv "$WORK/.git" "$OUT/repo.git"
    fi
    git --git-dir="$OUT/repo.git" config core.bare true
    rm -f "$OUT/repo.git/index"
    git --git-dir="$OUT/repo.git" reflog expire --expire=now --all
    git --git-dir="$OUT/repo.git" prune --expire=now
    if [ "${1-}" != --keep-loose ]; then
        git --git-dir="$OUT/repo.git" -c pack.threads=1 repack -adq
        git --git-dir="$OUT/repo.git" pack-refs --all
    fi
}
