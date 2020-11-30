/*
 * Copyright (c) Facebook, Inc. and its affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use crate::bundle_generator::{BookmarkChange, FilenodeVerifier};
use crate::errors::{
    ErrorKind::{BookmarkMismatchInBundleCombining, ReplayDataMissing, UnexpectedBookmarkMove},
    PipelineError,
};
use crate::{bind_sync_err, CombinedBookmarkUpdateLogEntry};
use anyhow::Error;
use blobrepo::{BlobRepo, ChangesetFetcher};
use blobrepo_hg::BlobRepoHg;
use bookmarks::{BookmarkName, BookmarkUpdateLogEntry, BookmarkUpdateReason, RawBundleReplayData};
use cloned::cloned;
use context::CoreContext;
use futures::{
    compat::Future01CompatExt,
    future::{self, try_join, try_join_all, BoxFuture, FutureExt, TryFutureExt},
    Future,
};
use getbundle_response::SessionLfsParams;
use itertools::Itertools;
use mercurial_bundle_replay_data::BundleReplayData;
use mercurial_types::HgChangesetId;
use metaconfig_types::LfsParams;
use mononoke_hg_sync_job_helper_lib::{
    retry, save_bundle_to_temp_file, save_bytes_to_temp_file, write_to_named_temp_file,
};
use mononoke_types::{datetime::Timestamp, ChangesetId};
use reachabilityindex::LeastCommonAncestorsHint;
use regex::Regex;
use skiplist::fetch_skiplist_index;
use slog::info;
use std::collections::HashMap;
use std::convert::TryFrom;
use std::sync::Arc;
use tempfile::NamedTempFile;

#[derive(Clone)]
pub struct PreparedBookmarkUpdateLogEntry {
    pub log_entry: BookmarkUpdateLogEntry,
    pub bundle_file: Arc<NamedTempFile>,
    pub timestamps_file: Arc<NamedTempFile>,
    pub cs_id: Option<(ChangesetId, HgChangesetId)>,
}

pub struct BundlePreparer {
    repo: BlobRepo,
    base_retry_delay_ms: u64,
    retry_num: usize,
    ty: BundleType,
}

#[derive(Clone)]
enum PrepareType {
    Generate {
        lca_hint: Arc<dyn LeastCommonAncestorsHint>,
        lfs_params: SessionLfsParams,
        filenode_verifier: FilenodeVerifier,
    },
    UseExisting {
        bundle_replay_data: RawBundleReplayData,
    },
}

#[derive(Clone)]
enum BundleType {
    // Use a bundle that was saved on Mononoke during the push
    UseExisting,
    // Generate a new bundle
    GenerateNew {
        lca_hint: Arc<dyn LeastCommonAncestorsHint>,
        lfs_params: LfsParams,
        filenode_verifier: FilenodeVerifier,
        bookmark_regex_force_lfs: Option<Regex>,
    },
}

impl BundlePreparer {
    pub async fn new_use_existing(
        repo: BlobRepo,
        base_retry_delay_ms: u64,
        retry_num: usize,
    ) -> Result<BundlePreparer, Error> {
        Ok(BundlePreparer {
            repo,
            base_retry_delay_ms,
            retry_num,
            ty: BundleType::UseExisting,
        })
    }

    pub async fn new_generate_bundles(
        ctx: CoreContext,
        repo: BlobRepo,
        base_retry_delay_ms: u64,
        retry_num: usize,
        maybe_skiplist_blobstore_key: Option<String>,
        lfs_params: LfsParams,
        filenode_verifier: FilenodeVerifier,
        bookmark_regex_force_lfs: Option<Regex>,
    ) -> Result<BundlePreparer, Error> {
        let blobstore = repo.get_blobstore().boxed();
        let skiplist =
            fetch_skiplist_index(&ctx, &maybe_skiplist_blobstore_key, &blobstore).await?;

        let lca_hint: Arc<dyn LeastCommonAncestorsHint> = skiplist;
        Ok(BundlePreparer {
            repo,
            base_retry_delay_ms,
            retry_num,
            ty: BundleType::GenerateNew {
                lca_hint,
                lfs_params,
                filenode_verifier,
                bookmark_regex_force_lfs,
            },
        })
    }

    pub async fn prepare_batches(
        &self,
        ctx: &CoreContext,
        entries: Vec<BookmarkUpdateLogEntry>,
    ) -> Result<Vec<BookmarkLogEntryBatch>, Error> {
        use BookmarkUpdateReason::*;

        for log_entry in &entries {
            match log_entry.reason {
                Pushrebase | Backsyncer | ManualMove | ApiRequest | XRepoSync | Push => {}
                Blobimport | TestMove => {
                    return Err(UnexpectedBookmarkMove(format!("{}", log_entry.reason)).into());
                }
            };
        }

        match &self.ty {
            BundleType::GenerateNew { lca_hint, .. } => {
                split_in_batches(ctx, lca_hint, &self.repo.get_changeset_fetcher(), entries).await
            }
            // We don't support combining bundles in UseExisting mode,
            // so just create batches with a single entry
            BundleType::UseExisting => Ok(entries
                .into_iter()
                .map(BookmarkLogEntryBatch::new)
                .collect()),
        }
    }

    pub fn prepare_bundles(
        &self,
        ctx: &CoreContext,
        batches: Vec<BookmarkLogEntryBatch>,
        overlay: &mut crate::BookmarkOverlay,
    ) -> impl Future<Output = Result<Vec<CombinedBookmarkUpdateLogEntry>, PipelineError>> {
        let mut futs = vec![];

        match &self.ty {
            BundleType::GenerateNew {
                lca_hint,
                lfs_params,
                filenode_verifier,
                bookmark_regex_force_lfs,
            } => {
                for batch in batches {
                    let prepare_type = PrepareType::Generate {
                        lca_hint: lca_hint.clone(),
                        lfs_params: get_session_lfs_params(
                            &ctx,
                            &batch.bookmark_name,
                            lfs_params.clone(),
                            &bookmark_regex_force_lfs,
                        ),
                        filenode_verifier: filenode_verifier.clone(),
                    };

                    let entries = batch.entries.clone();
                    let f = self.prepare_single_bundle(ctx.clone(), batch, overlay, prepare_type);
                    futs.push((f, entries));
                }
            }
            BundleType::UseExisting => {
                // We don't do any batching with UseExisting mode
                for batch in batches {
                    for log_entry in batch.entries {
                        let prepare_type = match &log_entry.bundle_replay_data {
                            Some(bundle_replay_data) => PrepareType::UseExisting {
                                bundle_replay_data: bundle_replay_data.clone(),
                            },
                            None => {
                                let err: Error = ReplayDataMissing { id: log_entry.id }.into();
                                return future::ready(Err(bind_sync_err(&[log_entry], err)))
                                    .boxed();
                            }
                        };

                        let batch = BookmarkLogEntryBatch::new(log_entry);
                        let entries = batch.entries.clone();
                        let f =
                            self.prepare_single_bundle(ctx.clone(), batch, overlay, prepare_type);
                        futs.push((f, entries));
                    }
                }
            }
        }

        let futs = futs
            .into_iter()
            .map(|(f, entries)| async move {
                let f = tokio::spawn(f);
                let res = f.map_err(Error::from).await;
                let res = match res {
                    Ok(Ok(res)) => Ok(res),
                    Ok(Err(err)) => Err(err),
                    Err(err) => Err(err),
                };
                res.map_err(|err| bind_sync_err(&entries, err))
            })
            .collect::<Vec<_>>();
        async move { try_join_all(futs).await }.boxed()
    }

    // Prepares a bundle that might be a result of combining a few BookmarkUpdateLogEntry.
    // Note that these entries should all move the same bookmark.
    fn prepare_single_bundle(
        &self,
        ctx: CoreContext,
        batch: BookmarkLogEntryBatch,
        overlay: &mut crate::BookmarkOverlay,
        prepare_type: PrepareType,
    ) -> BoxFuture<'static, Result<CombinedBookmarkUpdateLogEntry, Error>> {
        cloned!(self.repo);

        let book_values = overlay.get_bookmark_values();
        overlay.update(batch.bookmark_name.clone(), batch.to_cs_id.clone());

        let base_retry_delay_ms = self.base_retry_delay_ms;
        let retry_num = self.retry_num;
        async move {
            let entry_ids = batch
                .entries
                .iter()
                .map(|log_entry| log_entry.id)
                .collect::<Vec<_>>();
            info!(ctx.logger(), "preparing log entry ids #{:?} ...", entry_ids);
            // Check that all entries modify bookmark_name
            for entry in &batch.entries {
                if entry.bookmark_name != batch.bookmark_name {
                    return Err(BookmarkMismatchInBundleCombining {
                        ids: entry_ids,
                        entry_id: entry.id,
                        entry_bookmark_name: entry.bookmark_name.clone(),
                        bundle_bookmark_name: batch.bookmark_name,
                    }
                    .into());
                }
            }

            let bookmark_change = BookmarkChange::new(batch.from_cs_id, batch.to_cs_id)?;
            let bundle_timestamps = retry(
                &ctx.logger(),
                {
                    |_| {
                        Self::try_prepare_bundle_timestamps_file(
                            &ctx,
                            &repo,
                            prepare_type.clone(),
                            &book_values,
                            &bookmark_change,
                            &batch.bookmark_name,
                        )
                    }
                },
                base_retry_delay_ms,
                retry_num,
            )
            .map_ok(|(res, _)| res);

            let cs_id = async {
                match batch.to_cs_id {
                    Some(to_changeset_id) => {
                        let hg_cs_id = repo
                            .get_hg_from_bonsai_changeset(ctx.clone(), to_changeset_id)
                            .compat()
                            .await?;
                        Ok(Some((to_changeset_id, hg_cs_id)))
                    }
                    None => Ok(None),
                }
            };

            let ((bundle_file, timestamps_file), cs_id) =
                try_join(bundle_timestamps, cs_id).await?;

            info!(
                ctx.logger(),
                "successful prepare of entries #{:?}", entry_ids
            );

            Ok(CombinedBookmarkUpdateLogEntry {
                components: batch.entries,
                bundle_file: Arc::new(bundle_file),
                timestamps_file: Arc::new(timestamps_file),
                cs_id,
                bookmark: batch.bookmark_name,
            })
        }
        .boxed()
    }

    async fn try_prepare_bundle_timestamps_file<'a>(
        ctx: &'a CoreContext,
        repo: &'a BlobRepo,
        prepare_type: PrepareType,
        hg_server_heads: &'a [ChangesetId],
        bookmark_change: &'a BookmarkChange,
        bookmark_name: &'a BookmarkName,
    ) -> Result<(NamedTempFile, NamedTempFile), Error> {
        let blobstore = repo.get_blobstore();

        match prepare_type {
            PrepareType::Generate {
                lca_hint,
                lfs_params,
                filenode_verifier,
            } => {
                let (bytes, timestamps) = crate::bundle_generator::create_bundle(
                    ctx.clone(),
                    repo.clone(),
                    lca_hint.clone(),
                    bookmark_name.clone(),
                    bookmark_change.clone(),
                    hg_server_heads.to_vec(),
                    lfs_params,
                    filenode_verifier.clone(),
                )
                .compat()
                .await?;

                try_join(
                    save_bytes_to_temp_file(&bytes),
                    save_timestamps_to_file(&timestamps),
                )
                .await
            }
            PrepareType::UseExisting { bundle_replay_data } => {
                match BundleReplayData::try_from(bundle_replay_data) {
                    Ok(bundle_replay_data) => {
                        try_join(
                            save_bundle_to_temp_file(
                                &ctx,
                                &blobstore,
                                bundle_replay_data.bundle2_id,
                            ),
                            save_timestamps_to_file(&bundle_replay_data.timestamps),
                        )
                        .await
                    }
                    Err(e) => Err(e),
                }
            }
        }
    }
}

fn get_session_lfs_params(
    ctx: &CoreContext,
    bookmark: &BookmarkName,
    lfs_params: LfsParams,
    bookmark_regex_force_lfs: &Option<Regex>,
) -> SessionLfsParams {
    if let Some(regex) = bookmark_regex_force_lfs {
        if regex.is_match(bookmark.as_str()) {
            info!(ctx.logger(), "force generating lfs bundle for {}", bookmark);
            return SessionLfsParams {
                threshold: lfs_params.threshold,
            };
        }
    }

    if lfs_params.generate_lfs_blob_in_hg_sync_job {
        SessionLfsParams {
            threshold: lfs_params.threshold,
        }
    } else {
        SessionLfsParams { threshold: None }
    }
}

async fn save_timestamps_to_file(
    timestamps: &HashMap<HgChangesetId, Timestamp>,
) -> Result<NamedTempFile, Error> {
    let encoded_timestamps = timestamps
        .iter()
        .map(|(key, value)| {
            let timestamp = value.timestamp_seconds();
            format!("{}={}", key, timestamp)
        })
        .join("\n");

    write_to_named_temp_file(encoded_timestamps).await
}

pub struct BookmarkLogEntryBatch {
    entries: Vec<BookmarkUpdateLogEntry>,
    bookmark_name: BookmarkName,
    from_cs_id: Option<ChangesetId>,
    to_cs_id: Option<ChangesetId>,
}

impl BookmarkLogEntryBatch {
    pub fn new(log_entry: BookmarkUpdateLogEntry) -> Self {
        let bookmark_name = log_entry.bookmark_name.clone();
        let from_cs_id = log_entry.from_changeset_id;
        let to_cs_id = log_entry.to_changeset_id;
        Self {
            entries: vec![log_entry],
            bookmark_name,
            from_cs_id,
            to_cs_id,
        }
    }

    // Outer result's error means that some infrastructure error happened.
    // Inner result's error means that it wasn't possible to append entry,
    // and this entry is returned as the error.
    pub async fn try_append(
        &mut self,
        ctx: &CoreContext,
        lca_hint: &Arc<dyn LeastCommonAncestorsHint>,
        changeset_fetcher: &Arc<dyn ChangesetFetcher>,
        entry: BookmarkUpdateLogEntry,
    ) -> Result<Result<(), BookmarkUpdateLogEntry>, Error> {
        // Combine two bookmark update log entries only if bookmark names are the same
        if self.bookmark_name != entry.bookmark_name {
            return Ok(Err(entry));
        }

        // if it's a non-fast forward move then put it in the separate batch.
        // Otherwise some of the commits might not be synced to hg servers.
        // Consider this case:
        // C
        // |
        // B
        // |
        // A
        //
        // 1 entry - moves a bookmark from A to C
        // 2 entry - moves a bookmark from C to B
        //
        // if we combine them together then we get a batch that
        // moves a bookmark from A to B and commit C won't be synced
        // to hg servers. To prevent that let's put non-fast forward
        // moves to a separate branch
        match (entry.from_changeset_id, entry.to_changeset_id) {
            (Some(from_cs_id), Some(to_cs_id)) => {
                let is_ancestor = lca_hint
                    .is_ancestor(ctx, changeset_fetcher, from_cs_id, to_cs_id)
                    .await?;
                if !is_ancestor {
                    // Force non-forward moves to go to a separate batch
                    return Ok(Err(entry));
                }
            }
            _ => {}
        };

        // If we got a move where new from_cs_id is not equal to latest to_cs_id then
        // put it in a separate batch. This shouldn't normally happen though
        if self.to_cs_id != entry.from_changeset_id {
            return Ok(Err(entry));
        }

        self.to_cs_id = entry.to_changeset_id;
        self.entries.push(entry);
        Ok(Ok(()))
    }
}

async fn split_in_batches(
    ctx: &CoreContext,
    lca_hint: &Arc<dyn LeastCommonAncestorsHint>,
    changeset_fetcher: &Arc<dyn ChangesetFetcher>,
    entries: Vec<BookmarkUpdateLogEntry>,
) -> Result<Vec<BookmarkLogEntryBatch>, Error> {
    let mut batches: Vec<BookmarkLogEntryBatch> = vec![];

    for entry in entries {
        let entry = match batches.last_mut() {
            Some(batch) => match batch
                .try_append(ctx, lca_hint, changeset_fetcher, entry)
                .await?
            {
                Ok(()) => {
                    continue;
                }
                Err(entry) => entry,
            },
            None => entry,
        };
        batches.push(BookmarkLogEntryBatch::new(entry));
    }

    Ok(batches)
}

#[cfg(test)]
mod test {
    use super::*;
    use blobrepo_factory::new_memblob_empty;
    use fbinit::FacebookInit;
    use mononoke_types::RepositoryId;
    use skiplist::SkiplistIndex;
    use tests_utils::drawdag::create_from_dag;

    #[fbinit::compat_test]
    async fn test_split_in_batches_simple(fb: FacebookInit) -> Result<(), Error> {
        let ctx = CoreContext::test_mock(fb);
        let repo = new_memblob_empty(None)?;

        let commits = create_from_dag(
            &ctx,
            &repo,
            r##"
                A-B-C
            "##,
        )
        .await?;

        let sli: Arc<dyn LeastCommonAncestorsHint> = Arc::new(SkiplistIndex::new());

        let main = BookmarkName::new("main")?;
        let commit = commits.get("A").cloned().unwrap();
        let entries = vec![create_bookmark_log_entry(
            0,
            main.clone(),
            None,
            Some(commit),
        )];
        let res =
            split_in_batches(&ctx, &sli, &repo.get_changeset_fetcher(), entries.clone()).await?;

        assert_eq!(res.len(), 1);
        assert_eq!(res[0].entries, entries);
        assert_eq!(res[0].bookmark_name, main);
        assert_eq!(res[0].from_cs_id, None);
        assert_eq!(res[0].to_cs_id, Some(commit));

        Ok(())
    }

    #[fbinit::compat_test]
    async fn test_split_in_batches_all_in_one_batch(fb: FacebookInit) -> Result<(), Error> {
        let ctx = CoreContext::test_mock(fb);
        let repo = new_memblob_empty(None)?;

        let commits = create_from_dag(
            &ctx,
            &repo,
            r##"
                A-B-C
            "##,
        )
        .await?;

        let sli: Arc<dyn LeastCommonAncestorsHint> = Arc::new(SkiplistIndex::new());

        let main = BookmarkName::new("main")?;
        let commit_a = commits.get("A").cloned().unwrap();
        let commit_b = commits.get("B").cloned().unwrap();
        let commit_c = commits.get("C").cloned().unwrap();
        let entries = vec![
            create_bookmark_log_entry(0, main.clone(), None, Some(commit_a)),
            create_bookmark_log_entry(1, main.clone(), Some(commit_a), Some(commit_b)),
            create_bookmark_log_entry(2, main.clone(), Some(commit_b), Some(commit_c)),
        ];
        let res =
            split_in_batches(&ctx, &sli, &repo.get_changeset_fetcher(), entries.clone()).await?;

        assert_eq!(res.len(), 1);
        assert_eq!(res[0].entries, entries);
        assert_eq!(res[0].bookmark_name, main);
        assert_eq!(res[0].from_cs_id, None);
        assert_eq!(res[0].to_cs_id, Some(commit_c));

        Ok(())
    }

    #[fbinit::compat_test]
    async fn test_split_in_batches_different_bookmarks(fb: FacebookInit) -> Result<(), Error> {
        let ctx = CoreContext::test_mock(fb);
        let repo = new_memblob_empty(None)?;

        let commits = create_from_dag(
            &ctx,
            &repo,
            r##"
                A-B-C
            "##,
        )
        .await?;

        let sli: Arc<dyn LeastCommonAncestorsHint> = Arc::new(SkiplistIndex::new());

        let main = BookmarkName::new("main")?;
        let another = BookmarkName::new("another")?;
        let commit_a = commits.get("A").cloned().unwrap();
        let commit_b = commits.get("B").cloned().unwrap();
        let commit_c = commits.get("C").cloned().unwrap();
        let log_entry_1 = create_bookmark_log_entry(0, main.clone(), None, Some(commit_a));
        let log_entry_2 = create_bookmark_log_entry(1, another.clone(), None, Some(commit_b));
        let log_entry_3 =
            create_bookmark_log_entry(2, main.clone(), Some(commit_a), Some(commit_c));
        let entries = vec![
            log_entry_1.clone(),
            log_entry_2.clone(),
            log_entry_3.clone(),
        ];
        let res =
            split_in_batches(&ctx, &sli, &repo.get_changeset_fetcher(), entries.clone()).await?;

        assert_eq!(res.len(), 3);
        assert_eq!(res[0].entries, vec![log_entry_1]);
        assert_eq!(res[0].bookmark_name, main);
        assert_eq!(res[0].from_cs_id, None);
        assert_eq!(res[0].to_cs_id, Some(commit_a));

        assert_eq!(res[1].entries, vec![log_entry_2]);
        assert_eq!(res[1].bookmark_name, another);
        assert_eq!(res[1].from_cs_id, None);
        assert_eq!(res[1].to_cs_id, Some(commit_b));

        assert_eq!(res[2].entries, vec![log_entry_3]);
        assert_eq!(res[2].bookmark_name, main);
        assert_eq!(res[2].from_cs_id, Some(commit_a));
        assert_eq!(res[2].to_cs_id, Some(commit_c));

        Ok(())
    }

    #[fbinit::compat_test]
    async fn test_split_in_batches_non_forward_move(fb: FacebookInit) -> Result<(), Error> {
        let ctx = CoreContext::test_mock(fb);
        let repo = new_memblob_empty(None)?;

        let commits = create_from_dag(
            &ctx,
            &repo,
            r##"
                A-B-C
            "##,
        )
        .await?;

        let sli: Arc<dyn LeastCommonAncestorsHint> = Arc::new(SkiplistIndex::new());

        let main = BookmarkName::new("main")?;
        let commit_a = commits.get("A").cloned().unwrap();
        let commit_b = commits.get("B").cloned().unwrap();
        let commit_c = commits.get("C").cloned().unwrap();
        let log_entry_1 = create_bookmark_log_entry(0, main.clone(), None, Some(commit_a));
        let log_entry_2 =
            create_bookmark_log_entry(1, main.clone(), Some(commit_a), Some(commit_c));
        let log_entry_3 =
            create_bookmark_log_entry(2, main.clone(), Some(commit_c), Some(commit_b));
        let entries = vec![
            log_entry_1.clone(),
            log_entry_2.clone(),
            log_entry_3.clone(),
        ];
        let res =
            split_in_batches(&ctx, &sli, &repo.get_changeset_fetcher(), entries.clone()).await?;

        assert_eq!(res.len(), 2);
        assert_eq!(res[0].entries, vec![log_entry_1, log_entry_2]);
        assert_eq!(res[0].bookmark_name, main);
        assert_eq!(res[0].from_cs_id, None);
        assert_eq!(res[0].to_cs_id, Some(commit_c));

        assert_eq!(res[1].entries, vec![log_entry_3]);
        assert_eq!(res[1].bookmark_name, main);
        assert_eq!(res[1].from_cs_id, Some(commit_c));
        assert_eq!(res[1].to_cs_id, Some(commit_b));

        Ok(())
    }

    #[fbinit::compat_test]
    async fn test_split_in_batches_weird_move(fb: FacebookInit) -> Result<(), Error> {
        let ctx = CoreContext::test_mock(fb);
        let repo = new_memblob_empty(None)?;

        let commits = create_from_dag(
            &ctx,
            &repo,
            r##"
                A-B-C
            "##,
        )
        .await?;

        let sli: Arc<dyn LeastCommonAncestorsHint> = Arc::new(SkiplistIndex::new());

        let main = BookmarkName::new("main")?;
        let commit_a = commits.get("A").cloned().unwrap();
        let commit_b = commits.get("B").cloned().unwrap();
        let commit_c = commits.get("C").cloned().unwrap();
        let log_entry_1 = create_bookmark_log_entry(0, main.clone(), None, Some(commit_a));
        let log_entry_2 =
            create_bookmark_log_entry(1, main.clone(), Some(commit_b), Some(commit_c));
        let entries = vec![log_entry_1.clone(), log_entry_2.clone()];
        let res =
            split_in_batches(&ctx, &sli, &repo.get_changeset_fetcher(), entries.clone()).await?;

        assert_eq!(res.len(), 2);
        assert_eq!(res[0].entries, vec![log_entry_1]);
        assert_eq!(res[0].bookmark_name, main);
        assert_eq!(res[0].from_cs_id, None);
        assert_eq!(res[0].to_cs_id, Some(commit_a));

        assert_eq!(res[1].entries, vec![log_entry_2]);
        assert_eq!(res[1].bookmark_name, main);
        assert_eq!(res[1].from_cs_id, Some(commit_b));
        assert_eq!(res[1].to_cs_id, Some(commit_c));

        Ok(())
    }
    fn create_bookmark_log_entry(
        id: i64,
        bookmark_name: BookmarkName,
        from_changeset_id: Option<ChangesetId>,
        to_changeset_id: Option<ChangesetId>,
    ) -> BookmarkUpdateLogEntry {
        BookmarkUpdateLogEntry {
            id,
            repo_id: RepositoryId::new(0),
            bookmark_name,
            from_changeset_id,
            to_changeset_id,
            reason: BookmarkUpdateReason::TestMove,
            timestamp: Timestamp::now(),
            bundle_replay_data: None,
        }
    }
}
