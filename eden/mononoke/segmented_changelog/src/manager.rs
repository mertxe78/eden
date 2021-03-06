/*
 * Copyright (c) Facebook, Inc. and its affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::sync::Arc;

use anyhow::{format_err, Context, Result};
use async_trait::async_trait;
use slog::{debug, info};

use dag::InProcessIdDag;

use context::CoreContext;
use mononoke_types::{ChangesetId, RepositoryId};

use crate::bundle::SqlBundleStore;
use crate::dag::Dag;
use crate::iddag::IdDagSaveStore;
use crate::idmap::{SqlIdMap, SqlIdMapFactory};
use crate::logging::log_new_bundle;
use crate::types::{DagBundle, IdMapVersion};
use crate::{CloneData, SegmentedChangelog};

pub struct SegmentedChangelogManager {
    repo_id: RepositoryId,
    bundle_store: SqlBundleStore,
    iddag_save_store: IdDagSaveStore,
    idmap_factory: SqlIdMapFactory,
}

impl SegmentedChangelogManager {
    pub fn new(
        repo_id: RepositoryId,
        bundle_store: SqlBundleStore,
        iddag_save_store: IdDagSaveStore,
        idmap_factory: SqlIdMapFactory,
    ) -> Self {
        Self {
            repo_id,
            bundle_store,
            iddag_save_store,
            idmap_factory,
        }
    }

    pub async fn save_dag(
        &self,
        ctx: &CoreContext,
        iddag: &InProcessIdDag,
        idmap_version: IdMapVersion,
    ) -> Result<DagBundle> {
        // Save the IdDag
        let iddag_version = self
            .iddag_save_store
            .save(&ctx, &iddag)
            .await
            .with_context(|| format!("repo {}: error saving iddag", self.repo_id))?;
        // Update BundleStore
        let bundle = DagBundle::new(iddag_version, idmap_version);
        self.bundle_store
            .set(&ctx, bundle)
            .await
            .with_context(|| format!("repo {}: error updating bundle store", self.repo_id))?;
        log_new_bundle(ctx, self.repo_id, bundle);
        info!(
            ctx.logger(),
            "repo {}: segmented changelog dag bundle saved, idmap_version: {}, iddag_version: {}",
            self.repo_id,
            idmap_version,
            iddag_version,
        );
        Ok(bundle)
    }

    pub async fn load_dag(&self, ctx: &CoreContext) -> Result<(DagBundle, Dag)> {
        let bundle = self
            .bundle_store
            .get(&ctx)
            .await
            .with_context(|| {
                format!(
                    "repo {}: error loading segmented changelog bundle data",
                    self.repo_id
                )
            })?
            .ok_or_else(|| {
                format_err!(
                    "repo {}: segmented changelog metadata not found, maybe repo is not seeded",
                    self.repo_id
                )
            })?;
        let iddag = self
            .iddag_save_store
            .load(&ctx, bundle.iddag_version)
            .await
            .with_context(|| format!("repo {}: failed to load iddag", self.repo_id))?;
        let idmap = self.new_sql_idmap(bundle.idmap_version);
        debug!(
            ctx.logger(),
            "segmented changelog dag successfully loaded - repo_id: {}, idmap_version: {}, \
            iddag_version: {} ",
            self.repo_id,
            bundle.idmap_version,
            bundle.iddag_version,
        );
        let dag = Dag::new(iddag, idmap);
        Ok((bundle, dag))
    }

    pub fn new_sql_idmap(&self, idmap_version: IdMapVersion) -> Arc<SqlIdMap> {
        Arc::new(self.idmap_factory.sql_idmap(idmap_version))
    }
}

#[async_trait]
impl SegmentedChangelog for SegmentedChangelogManager {
    async fn location_to_many_changeset_ids(
        &self,
        ctx: &CoreContext,
        known: ChangesetId,
        distance: u64,
        count: u64,
    ) -> Result<Vec<ChangesetId>> {
        let (_, dag) = self.load_dag(&ctx).await.with_context(|| {
            format!(
                "repo {}: error loading segmented changelog from save",
                self.repo_id
            )
        })?;
        dag.location_to_many_changeset_ids(ctx, known, distance, count)
            .await
    }

    async fn clone_data(&self, ctx: &CoreContext) -> Result<CloneData<ChangesetId>> {
        let (_, dag) = self.load_dag(&ctx).await.with_context(|| {
            format!(
                "repo {}: error loading segmented changelog from save",
                self.repo_id
            )
        })?;
        dag.clone_data(ctx).await
    }
}
