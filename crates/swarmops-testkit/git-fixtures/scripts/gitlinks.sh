# gitlinks: submodule entries, mode 160000, each naming a commit of another repository, which a
# changeset cannot hold: first a gitlink at lib/sub beside the file lib/readme.txt, then a
# directory only/ that holds nothing but a gitlink, so that dropping it leaves only/ empty.
# Each is added to the index by `update-index --cacheinfo`, as `git submodule add` does, with no
# submodule repository behind it. Class gitlinks.

. "$(dirname "$0")/lib.sh"

git init -q -b main
mkdir lib
printf 'lib\n' > lib/readme.txt
commit_all 'add lib/readme.txt'

# From here on the work tree lacks the gitlinks' directories: no `git add -A`, which would
# remove them from the index.
git update-index --add --cacheinfo 160000,1111111111111111111111111111111111111111,lib/sub
tick
git commit -q -m 'add the gitlink lib/sub'

git update-index --add --cacheinfo 160000,2222222222222222222222222222222222222222,only/module
tick
git commit -q -m 'add only/, which holds nothing but the gitlink only/module'

finish
