# headers: commit headers past tree, parents, author and committer, each kept as written. git
# writes the first: `encoding ISO-8859-1` over a Latin-1 message (i18n.commitEncoding). The
# others are crafted, as a signing git writes them, the signatures made up: `gpgsig`, a value
# of several lines with a blank one in it (a continuation line of one space); a merge whose
# `mergetag` holds a signed tag of its second parent; an unknown header, `x-custom`. Faithful.

. "$(dirname "$0")/lib.sh"

git init -q -b main
printf 'plain\n' > plain.txt
commit_all 'no header past the committer'

printf 'latin\n' > latin.txt
git add latin.txt
tick
git -c i18n.commitEncoding=ISO-8859-1 commit -q -m "$(printf 'caf\351: a message in Latin-1')"

# signature: a made-up armored signature, each of its lines after the first a continuation
# line, as a header's value is written.
signature() {
    printf '%s\n' '-----BEGIN PGP SIGNATURE-----'
    printf ' \n'
    printf ' iHUEABYKAB0WIQRGaXh0dXJlIHNpZ25hdHVyZSBub3QgcmVhbAAKCRBGaXh0dXJl\n'
    printf ' IHNpZ25hdHVyZQ==\n'
    printf ' =FAKE\n'
    printf ' -----END PGP SIGNATURE-----\n'
}

printf 'signed\n' > signed.txt
git add signed.txt
tick
commit=$(
    {
        printf 'tree %s\nparent %s\n' "$(git write-tree)" "$(git rev-parse HEAD)"
        idents
        printf 'gpgsig '
        signature
        printf '\na commit with a gpgsig header\n'
    } | craft commit -
)
git update-ref HEAD "$commit"

# A merge of the signed tag side-v1, which names side's tip: git's own merge, made again with
# the mergetag header git writes for a merged signed tag (the tag object, its lines after the
# first continuation lines, its signature at its end).
git checkout -q -b side
printf 'side\n' > side.txt
commit_all 'side: add side.txt'
git checkout -q main
tick
git merge -q --no-ff -m "Merge tag 'side-v1'" side
commit=$(
    {
        printf 'tree %s\n' "$(git rev-parse 'HEAD^{tree}')"
        printf 'parent %s\nparent %s\n' "$(git rev-parse HEAD^1)" "$(git rev-parse HEAD^2)"
        idents
        printf 'mergetag object %s\n' "$(git rev-parse side)"
        printf ' type commit\n'
        printf ' tag side-v1\n'
        printf ' tagger %s\n' "$(git var GIT_COMMITTER_IDENT)"
        printf ' \n'
        printf ' side-v1: a signed tag\n'
        printf ' '
        signature
        printf "\\nMerge tag 'side-v1'\\n"
    } | craft commit -
)
git update-ref HEAD "$commit"

printf 'custom\n' > custom.txt
git add custom.txt
tick
commit=$(
    {
        printf 'tree %s\nparent %s\n' "$(git write-tree)" "$(git rev-parse HEAD)"
        idents
        printf 'x-custom a header no git writes\n'
        printf '\na commit with an unknown header\n'
    } | craft commit -
)
git update-ref HEAD "$commit"

finish
