#!/bin/sh
# Rewrites expected.json: for each case under cases/ (its base, ours and theirs), what
# `git merge-file -p` makes of it, as the test `diff3::agrees_with_the_git_merge_file_corpus`
# reads it: the exit status (the number of conflicts, 0 for a clean merge) and the output (the
# merged file, with conflict markers where it conflicts) in base64. No test runs git.
#
# The oracle is git 2.43, the version umbrella spec §8 declares, and the script refuses any
# other. git 2.43's merge-file has no --diff-algorithm (git 2.44 added it), so it pairs lines with
# its Myers diff where merge_content uses the histogram diff: the cases are built from distinct
# lines, which the two pair up alike, and known_differences.json records, with its reason, any
# case on which the merge may differ from git's.
#
# It runs only by hand (from a terminal) or with SWARMOPS_REGEN_FIXTURES=1:
#   SWARMOPS_REGEN_FIXTURES=1 sh crates/swarmops-merge/tests/fixtures/merge_file/generate.sh
# SWARMOPS_GIT names the git to run (default: `git` on PATH).
set -eu

here=$(cd "$(dirname "$0")" && pwd)
if [ "${SWARMOPS_REGEN_FIXTURES:-}" != 1 ] && [ ! -t 0 ]; then
    echo "generate.sh: rewrites expected.json; run it by hand or with SWARMOPS_REGEN_FIXTURES=1" >&2
    exit 2
fi
git=${SWARMOPS_GIT:-git}
version=$("$git" --version 2>&1) || version="no version: '$git --version' failed"
case $version in
"git version 2.43" | "git version 2.43."*) ;;
*)
    echo "generate.sh: needs git 2.43 (umbrella spec §8), found: $version" >&2
    exit 2
    ;;
esac

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
mkdir "$work/home"
# The environment M3's git fixtures pin: no system or global config, an empty HOME, the C
# locale. merge-file runs in a scratch directory that no repository holds, so that no repository
# configuration (merge.conflictStyle, for one) reaches it.
export GIT_CONFIG_NOSYSTEM=1 GIT_CONFIG_GLOBAL=/dev/null HOME="$work/home" TZ=UTC LC_ALL=C
GIT_CEILING_DIRECTORIES=$(dirname "$work")
export GIT_CEILING_DIRECTORIES
cd "$work"

out="$work/expected.json"
printf '{\n' >"$out"
first=1
for case in "$here"/cases/*/; do
    name=$(basename "$case")
    # Copies: merge-file never writes to its inputs with -p, and the cases stay as committed.
    for side in base ours theirs; do
        cp "$case$side" "$work/$side"
    done
    status=0
    "$git" merge-file -p -L ours -L base -L theirs ours base theirs >"$work/output" || status=$?
    if [ "$status" -gt 127 ]; then
        echo "generate.sh: git merge-file failed on $name (exit status $status)" >&2
        exit 1
    fi
    if [ "$first" = 1 ]; then
        first=0
    else
        printf ',\n' >>"$out"
    fi
    printf '  "%s": {"status": %d, "output": "%s"}' \
        "$name" "$status" "$(base64 -w0 <"$work/output")" >>"$out"
done
printf '\n}\n' >>"$out"
mv "$out" "$here/expected.json"
echo "generate.sh: wrote $here/expected.json with $version"
