# Copyright (c) Facebook, Inc. and its affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License found in the LICENSE file in the root
# directory of this source tree.

  $ . "${TEST_FIXTURES}/library.sh"
  $ ENABLED_DERIVED_DATA='["git_trees", "filenodes", "hgchangesets"]' setup_common_config
  $ GIT_REPO="${TESTTMP}/repo-git"
  $ HG_REPO="${TESTTMP}/repo-hg"

# Setup git repsitory
  $ mkdir "$GIT_REPO"
  $ cd "$GIT_REPO"
  $ git init
  Initialized empty Git repository in $TESTTMP/repo-git/.git/
  $ echo "this is file1" > file1
  $ git add file1
  $ git commit -am "Add file1"
  [master (root-commit) 8ce3eae] Add file1
   1 file changed, 1 insertion(+)
   create mode 100644 file1

# Import it into Mononoke
  $ cd "$TESTTMP"
  $ gitimport "$GIT_REPO" --derive-trees --derive-hg --hggit-compatibility full-repo
  * using repo "repo" repoid RepositoryId(0) (glob)
  * Created 8ce3eae44760b500bf3f2c3922a95dcd3c908e9e => ChangesetId(Blake2(967b83a1a809dbd715163d6cbd5197b4733a09068c57251481c0bc76e6297ca0)) (glob)
  * 1 bonsai changesets have been committed (glob)
  * Ref: Some("refs/heads/master"): Some(ChangesetId(Blake2(967b83a1a809dbd715163d6cbd5197b4733a09068c57251481c0bc76e6297ca0))) (glob)
  * 1 tree(s) are valid! (glob)
  * Hg: 8ce3eae44760b500bf3f2c3922a95dcd3c908e9e: HgManifestId(HgNodeHash(Sha1(009adbc8d457927d2e1883c08b0692bc45089839))) (glob)

# Also check that a readonly import works
  $ gitimport "$GIT_REPO" --with-readonly-storage=true --derive-trees --derive-hg --hggit-compatibility full-repo
  * using repo "repo" repoid RepositoryId(0) (glob)
  * Created 8ce3eae44760b500bf3f2c3922a95dcd3c908e9e => ChangesetId(Blake2(967b83a1a809dbd715163d6cbd5197b4733a09068c57251481c0bc76e6297ca0)) (glob)
  * 1 bonsai changesets have been committed (glob)
  * Ref: Some("refs/heads/master"): Some(ChangesetId(Blake2(967b83a1a809dbd715163d6cbd5197b4733a09068c57251481c0bc76e6297ca0))) (glob)
  * 1 tree(s) are valid! (glob)
  * Hg: 8ce3eae44760b500bf3f2c3922a95dcd3c908e9e: HgManifestId(HgNodeHash(Sha1(009adbc8d457927d2e1883c08b0692bc45089839))) (glob)

# Set master (gitimport does not do this yet)
  $ mononoke_admin bookmarks set master 967b83a1a809dbd715163d6cbd5197b4733a09068c57251481c0bc76e6297ca0
  * using repo "repo" repoid RepositoryId(0) (glob)
  * changeset resolved as: ChangesetId(Blake2(967b83a1a809dbd715163d6cbd5197b4733a09068c57251481c0bc76e6297ca0)) (glob)
  * Current position of BookmarkName { bookmark: "master" } is None (glob)

# Start Mononoke
  $ mononoke
  $ wait_for_mononoke

# Clone the repository
  $ cd "$TESTTMP"
  $ hgmn_clone 'ssh://user@dummy/repo' "$HG_REPO"
  $ cd "$HG_REPO"
  $ cat "file1"
  this is file1

# Try out hggit compatibility
  $ hg --config extensions.hggit= git-updatemeta
  $ hg --config extensions.hggit= log -T '{gitnode}'
  8ce3eae44760b500bf3f2c3922a95dcd3c908e9e (no-eol)
