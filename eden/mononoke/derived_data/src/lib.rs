/*
 * Copyright (c) Facebook, Inc. and its affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#![deny(warnings)]

use anyhow::Error;
use async_trait::async_trait;
use auto_impl::auto_impl;
use blobrepo::BlobRepo;
use context::CoreContext;
use lock_ext::LockExt;
use mononoke_types::{BonsaiChangeset, ChangesetId, RepositoryId};
use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex},
};
use thiserror::Error;

pub mod batch;
pub mod derive_impl;
pub mod mapping_impl;

pub use mapping_impl::{BlobstoreExistsMapping, BlobstoreRootIdMapping};

pub use crate::derive_impl::DeriveMode;

#[derive(Debug, Error)]
pub enum DeriveError {
    #[error("Derivation of {0} is not enabled for repo={2} repoid={1}")]
    Disabled(&'static str, RepositoryId, String),
    #[error("{0}")]
    Error(#[from] Error),
}

/// Trait for the data that can be derived from bonsai changeset.
/// Examples of that are hg changeset id, unodes root manifest id, git changeset ids etc
#[async_trait]
pub trait BonsaiDerived: Sized + 'static + Send + Sync + Clone {
    /// Name of derived data
    ///
    /// Should be unique string (among derived data types), which is used to identify or
    /// name data (for example lease keys) assoicated with particular derived data type.
    const NAME: &'static str;

    type Mapping: BonsaiDerivedMapping<Value = Self>;

    /// Get mapping associated with this derived data type.
    fn mapping(ctx: &CoreContext, repo: &BlobRepo) -> Self::Mapping;

    /// Defines how to derive new representation for bonsai having derivations
    /// for parents and having a current bonsai object.
    ///
    /// Note that if any data has to be persistently stored in blobstore, mysql or any other store
    /// then it's responsiblity of implementor of `derive_from_parents()` to save it.
    /// For example, to derive HgChangesetId we also need to derive all filenodes and all manifests
    /// and then store them in blobstore. Derived data library is only responsible for
    /// updating BonsaiDerivedMapping.
    async fn derive_from_parents(
        ctx: CoreContext,
        repo: BlobRepo,
        bonsai: BonsaiChangeset,
        parents: Vec<Self>,
    ) -> Result<Self, Error>;

    /// This function is the entrypoint for changeset derivation, it converts
    /// bonsai representation to derived one by calling derive_from_parents(), and saves mapping
    /// from csid -> BonsaiDerived in BonsaiDerivedMapping
    ///
    /// This function fails immediately if this type of derived data is not enabled for this repo.
    async fn derive(
        ctx: &CoreContext,
        repo: &BlobRepo,
        csid: ChangesetId,
    ) -> Result<Self, DeriveError> {
        let mapping = Self::mapping(&ctx, &repo);
        derive_impl::derive_impl::<Self, Self::Mapping>(
            ctx,
            repo,
            &mapping,
            csid,
            DeriveMode::OnlyIfEnabled,
        )
        .await
    }

    /// Fetch the derived data in cases where we might not want to trigger derivation, e.g. when scrubbing.
    async fn fetch_derived(
        ctx: &CoreContext,
        repo: &BlobRepo,
        csid: &ChangesetId,
    ) -> Result<Option<Self>, Error> {
        let mapping = Self::mapping(ctx, repo);
        derive_impl::fetch_derived::<Self, Self::Mapping>(ctx, csid, &mapping).await
    }

    /// Returns min(number of ancestors of `csid` to be derived, `limit`)
    ///
    /// This function fails immediately if derived data is not enabled for this repo.
    async fn count_underived(
        ctx: &CoreContext,
        repo: &BlobRepo,
        csid: &ChangesetId,
        limit: u64,
    ) -> Result<u64, DeriveError> {
        let mapping = Self::mapping(&ctx, &repo);
        let underived = derive_impl::find_topo_sorted_underived::<Self, Self::Mapping, _>(
            ctx,
            repo,
            &mapping,
            Some(*csid),
            Some(limit),
            DeriveMode::OnlyIfEnabled,
        )
        .await?;
        Ok(underived.len() as u64)
    }

    /// Find all underived ancestors reachable from provided set of changesets.
    ///
    /// Items are returned in topologically sorted order starting from changesets
    /// with no dependencies or derived dependencies.
    async fn find_all_underived_ancestors(
        ctx: &CoreContext,
        repo: &BlobRepo,
        csids: Vec<ChangesetId>,
    ) -> Result<Vec<ChangesetId>, DeriveError> {
        let mapping = Self::mapping(&ctx, &repo);
        let underived = derive_impl::find_topo_sorted_underived::<Self, Self::Mapping, _>(
            ctx,
            repo,
            &mapping,
            csids,
            None,
            DeriveMode::OnlyIfEnabled,
        )
        .await?;
        Ok(underived)
    }

    async fn is_derived(
        ctx: &CoreContext,
        repo: &BlobRepo,
        csid: &ChangesetId,
    ) -> Result<bool, DeriveError> {
        let count = Self::count_underived(&ctx, &repo, &csid, 1).await?;
        Ok(count == 0)
    }

    /// This method might be overridden by BonsaiDerived implementors if there's a more efficient
    /// way to derive a batch of commits
    async fn batch_derive<BatchMapping>(
        ctx: &CoreContext,
        repo: &BlobRepo,
        csids: Vec<ChangesetId>,
        mapping: &BatchMapping,
        mode: DeriveMode,
    ) -> Result<HashMap<ChangesetId, Self>, Error>
    where
        BatchMapping: BonsaiDerivedMapping<Value = Self> + Send + Sync + Clone + 'static,
    {
        let mut res = HashMap::new();
        // The default implementation must derive sequentially with no
        // parallelism or concurrency, as dependencies between changesets may
        // cause O(n^2) derivations.
        for csid in csids {
            let derived =
                derive_impl::derive_impl::<Self, BatchMapping>(ctx, repo, mapping, csid, mode)
                    .await?;
            res.insert(csid, derived);
        }
        Ok(res)
    }
}

/// After derived data was generated then it will be stored in BonsaiDerivedMapping, which is
/// normally a persistent store. This is used to avoid regenerating the same derived data over
/// and over again.
#[async_trait]
#[auto_impl(Arc)]
pub trait BonsaiDerivedMapping: Send + Sync + Clone {
    type Value: BonsaiDerived;

    /// Fetches mapping from bonsai changeset ids to generated value
    async fn get(
        &self,
        ctx: CoreContext,
        csids: Vec<ChangesetId>,
    ) -> Result<HashMap<ChangesetId, Self::Value>, Error>;

    /// Saves mapping between bonsai changeset and derived data id
    async fn put(&self, ctx: CoreContext, csid: ChangesetId, id: Self::Value) -> Result<(), Error>;
}

/// This mapping can be used when we want to ignore values before it was put
/// again for some specific set of commits. It is useful when we want either
/// re-backfill derived data or investigate performance problems.
#[derive(Clone)]
pub struct RegenerateMapping<M> {
    regenerate: Arc<Mutex<HashSet<ChangesetId>>>,
    base: M,
}

impl<M> RegenerateMapping<M> {
    pub fn new(base: M) -> Self {
        Self {
            regenerate: Default::default(),
            base,
        }
    }

    pub fn regenerate<I: IntoIterator<Item = ChangesetId>>(&self, csids: I) {
        self.regenerate.with(|regenerate| regenerate.extend(csids))
    }
}

#[async_trait]
impl<M> BonsaiDerivedMapping for RegenerateMapping<M>
where
    M: BonsaiDerivedMapping,
{
    type Value = M::Value;

    async fn get(
        &self,
        ctx: CoreContext,
        mut csids: Vec<ChangesetId>,
    ) -> Result<HashMap<ChangesetId, Self::Value>, Error> {
        self.regenerate
            .with(|regenerate| csids.retain(|id| !regenerate.contains(&id)));
        self.base.get(ctx, csids).await
    }

    async fn put(&self, ctx: CoreContext, csid: ChangesetId, id: Self::Value) -> Result<(), Error> {
        self.regenerate.with(|regenerate| regenerate.remove(&csid));
        self.base.put(ctx, csid, id).await
    }
}
