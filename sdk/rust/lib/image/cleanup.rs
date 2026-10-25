//! Image deletion coordinates with file readers and publishers through catalog commit.
//!
//! Prepare bounded entry claims and durable inode pins before taking the catalog writer.
//! The transaction rechecks current ownership and selects only already-journaled entries;
//! unlink and journal disposal run after commit. No cache-file I/O holds the writer.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use microsandbox_image::{
    CachedImageMetadata, Digest, GlobalCache, Reference, storage_lease::StorageLease,
};
use sea_orm::{ColumnTrait, DatabaseTransaction, EntityTrait, PaginatorTrait, QueryFilter};

use super::{ImagePruneReport, cleanup_journal::Journal};
use crate::backend::LocalBackend;
use crate::db::{
    DbReadConnection,
    entity::{image_ref, layer, manifest, manifest_layer, snapshot},
};
use crate::{Image, MicrosandboxError, MicrosandboxResult};

//--------------------------------------------------------------------------------------------------
// Types
//--------------------------------------------------------------------------------------------------

#[derive(Clone, Default)]
struct Cleanup {
    files: Vec<PathBuf>,
    // These MUST outlive both the transaction and disk cleanup. Acquiring them only after
    // commit allows a publisher to recreate the same path in the intervening window.
    leases: HashMap<PathBuf, StorageLease>,
    more: bool,
    skipped: HashSet<PathBuf>,
    /// Metadata generations read under exclusive claims, outside the writer transaction.
    metadata_digests: HashMap<PathBuf, Option<String>>,
    /// Each claimed manifest's flat entries, read outside the writer transaction.
    flat: HashMap<i32, FlatClaim>,
    /// Claimed flat blobs that no ref names, which prune removes without a catalog owner.
    unnamed_blobs: Vec<PathBuf>,
}

/// A claimed manifest's flat ref, the blob it names, and whether the claim includes that blob.
#[derive(Clone)]
struct FlatClaim {
    reference: PathBuf,
    blob: Option<PathBuf>,
    claims_blob: bool,
}

/// Advisory catalog snapshot used only to choose bounded preparation candidates.
struct CleanupCatalog {
    references: Vec<(image_ref::Model, Option<manifest::Model>)>,
    manifests: Vec<manifest::Model>,
    layers: Vec<layer::Model>,
    manifest_layers: Vec<manifest_layer::Model>,
    sandbox_refs: HashSet<i32>,
    snapshot_refs: HashSet<String>,
}

/// Current durable owners, reloaded inside each cleanup transaction.
struct CleanupOwners {
    sandbox_refs: HashSet<i32>,
    snapshot_refs: HashSet<String>,
}

/// Frozen scope of an explicit removal, or all eligible entries for prune.
#[derive(Clone)]
struct CleanupScope {
    selected: Option<(String, bool)>,
    selected_manifests: HashSet<i32>,
    selected_layers: HashSet<i32>,
}

/// Prepared claims and journal remain owned until catalog commit and filesystem cleanup finish.
struct PreparedCleanup {
    cleanup: Cleanup,
    journal: Option<Journal>,
    skipped_in_use: u32,
}

//--------------------------------------------------------------------------------------------------
// Methods
//--------------------------------------------------------------------------------------------------

impl Image {
    /// Remove an image from the database and clean up orphaned layers on disk.
    ///
    /// Referenced images require `force` to untag, while their backing remains available to
    /// sandboxes and snapshots. Active storage operations always return
    /// [`MicrosandboxError::ImageInUse`], including with `force`.
    pub async fn remove_local(
        local: &LocalBackend,
        reference: &str,
        force: bool,
    ) -> MicrosandboxResult<()> {
        Self::delete_local(local, Some((reference.to_owned(), force))).await?;
        Ok(())
    }

    /// Remove cached image data that is not used by any sandbox or indexed snapshot.
    ///
    /// Pruning removes unused image references, then removes manifests and layers
    /// that become unreachable. Images used by existing sandboxes or snapshots
    /// are preserved.
    /// If a later cleanup batch fails, the error message includes removal counts from
    /// completed batches. The failing batch may have made additional progress.
    pub async fn prune_local(local: &LocalBackend) -> MicrosandboxResult<ImagePruneReport> {
        Self::delete_local(local, None).await
    }

    async fn delete_local(
        local: &LocalBackend,
        selected: Option<(String, bool)>,
    ) -> MicrosandboxResult<ImagePruneReport> {
        crate::backend::local::snapshot::publication::recover(local).await?;
        Journal::recover(local).await?;
        let pools = local.db().await?;
        // Freeze the scope of an explicit removal, not the mutable catalog contents. Every
        // batch rechecks owners inside its write transaction before removing an object.
        let mut selected_manifests = HashSet::new();
        let mut selected_layers = HashSet::new();
        if let Some((name, _)) = &selected {
            let reference = image_ref::Entity::find()
                .filter(image_ref::Column::Reference.eq(name))
                .one(pools.read())
                .await?
                .ok_or_else(|| MicrosandboxError::ImageNotFound(name.clone()))?;

            selected_manifests.insert(reference.manifest_id);
            selected_layers.extend(
                manifest_layer::Entity::find()
                    .filter(manifest_layer::Column::ManifestId.eq(reference.manifest_id))
                    .all(pools.read())
                    .await?
                    .into_iter()
                    .map(|row| row.layer_id),
            );
        }
        let mut total = ImagePruneReport::default();
        let mut skipped = HashSet::new();
        let mut completed_batches = 0;
        loop {
            let result = Self::delete_local_batch(
                local,
                selected.clone(),
                selected_manifests.clone(),
                selected_layers.clone(),
                skipped,
            )
            .await;
            let (report, more, next_skipped) = match result {
                Ok(batch) => batch,
                Err(error) if selected.is_none() && completed_batches > 0 => {
                    // Only completed batches are counted. A failing batch can already
                    // have committed catalog changes or unlinked some of its files.
                    return Err(MicrosandboxError::Custom(format!(
                        "image prune failed after completed batches removed {} image references, \
                         {} manifests, {} layers, {} fsmeta files and {} VMDK files \
                         ({} bytes reclaimed; {} busy entries skipped); \
                         the failing batch may have made additional progress: {error}",
                        total.image_refs_removed,
                        total.manifests_removed,
                        total.layers_removed,
                        total.fsmeta_removed,
                        total.vmdk_removed,
                        total.bytes_reclaimed.unwrap_or(0),
                        total.skipped_in_use,
                    )));
                }
                Err(error) => return Err(error),
            };

            completed_batches += 1;
            total.image_refs_removed += report.image_refs_removed;
            total.manifests_removed += report.manifests_removed;
            total.layers_removed += report.layers_removed;
            total.fsmeta_removed += report.fsmeta_removed;
            total.vmdk_removed += report.vmdk_removed;
            total.skipped_in_use += report.skipped_in_use;
            if let Some(bytes) = report.bytes_reclaimed {
                total.bytes_reclaimed =
                    Some(total.bytes_reclaimed.unwrap_or(0).saturating_add(bytes));
            }
            if !more {
                return Ok(total);
            }
            skipped = next_skipped;
        }
    }

    async fn delete_local_batch(
        local: &LocalBackend,
        selected: Option<(String, bool)>,
        selected_manifests: HashSet<i32>,
        selected_layers: HashSet<i32>,
        skipped: HashSet<PathBuf>,
    ) -> MicrosandboxResult<(ImagePruneReport, bool, HashSet<PathBuf>)> {
        let cache = GlobalCache::new_async(&local.cache_dir()).await?;
        let pools = local.db().await?;
        let catalog = CleanupCatalog::load(pools.read()).await?;
        let prepared = PreparedCleanup::prepare(
            local.cache_dir(),
            cache.clone(),
            catalog,
            selected.clone(),
            selected_manifests.clone(),
            selected_layers.clone(),
            skipped,
        )
        .await?;

        prepared
            .commit(local, cache, selected, selected_manifests, selected_layers)
            .await
    }
}

impl Cleanup {
    /// Acquire bounded, nonblocking claims in filesystem work, never under the catalog writer.
    fn claim(
        &mut self,
        mut paths: Vec<PathBuf>,
        report: &mut ImagePruneReport,
    ) -> MicrosandboxResult<bool> {
        paths.sort();
        paths.dedup();
        if paths.iter().any(|path| self.skipped.contains(path)) {
            return Ok(false);
        }
        let additional = paths
            .iter()
            .filter(|path| !self.leases.contains_key(*path))
            .count();
        if self.leases.len() + additional > 48 {
            self.more = true;
            return Ok(false);
        }

        let mut leases = Vec::new();
        for path in &paths {
            if self.leases.contains_key(path) {
                continue;
            }
            let Some(lease) = StorageLease::try_exclusive(path)? else {
                self.skipped.extend(paths);
                report.skipped_in_use += 1;
                return Ok(false);
            };
            leases.push((path.clone(), lease));
        }
        for path in paths {
            if !self.leases.contains_key(&path) {
                self.files.push(path);
            }
        }
        self.leases.extend(leases);
        Ok(true)
    }

    /// Claim a manifest's entries with `flat_paths`, its flat ref and, unless a ref this batch
    /// keeps names it too, the blob that ref names, as read before the claims.
    ///
    /// The ref may have been replaced, or another ref may have come to name the blob, since that
    /// read. Publishers lease a ref and the blob it names before writing the ref, so the ref
    /// cannot change once it is claimed, nor the blob's names once the blob is: read each again
    /// only under its claim.
    fn claim_manifest(
        &mut self,
        cache: &GlobalCache,
        digest: &Digest,
        flat_paths: Vec<PathBuf>,
        report: &mut ImagePruneReport,
    ) -> MicrosandboxResult<Option<FlatClaim>> {
        let reference = cache.flat_ref_path(digest);
        let read_blob = flat_paths.iter().find(|path| **path != reference).cloned();
        let mut paths = vec![cache.fsmeta_erofs_path(digest), cache.vmdk_path(digest)];
        paths.extend(flat_paths);
        let added = paths
            .iter()
            .filter(|path| !self.leases.contains_key(*path))
            .cloned()
            .collect::<Vec<_>>();
        let mut added = added;
        if !self.claim(paths, report)? {
            return Ok(None);
        }

        // A blob the ref came to name after the read is claimed now, before its names are read.
        // A busy one stays, and once no ref names it, a later prune reclaims it.
        let blob = cache.flat_ref_blob_path(digest)?;
        let blob_claimed = match &blob {
            Some(blob) if self.leases.contains_key(blob) => true,
            Some(blob) => {
                let claimed = self.claim(vec![blob.clone()], report)?;
                if claimed {
                    added.push(blob.clone());
                }
                claimed
            }
            None => false,
        };
        let deleting = self.files.iter().cloned().collect();
        let claims_blob = match &blob {
            Some(blob) if blob_claimed => !cache.named_flat_blob_paths(&deleting)?.contains(blob),
            _ => false,
        };

        // Give up the blobs this claim acquired but does not remove.
        let removed_blob = blob.clone().filter(|_| claims_blob);
        for candidate in [read_blob, blob.clone()].into_iter().flatten() {
            if removed_blob.as_ref() != Some(&candidate) && added.contains(&candidate) {
                self.release(&candidate);
            }
        }

        Ok(Some(FlatClaim {
            reference,
            blob,
            claims_blob,
        }))
    }

    /// Give up an entry this batch claimed but no longer removes.
    fn release(&mut self, path: &PathBuf) {
        self.leases.remove(path);
        self.files.retain(|candidate| candidate != path);
    }

    /// The transaction may select existing claims, but must never acquire new ones or do I/O.
    fn select(&mut self, paths: Vec<PathBuf>) -> bool {
        if paths.iter().any(|path| self.skipped.contains(path)) {
            return false;
        }
        if paths.iter().any(|path| !self.leases.contains_key(path)) {
            // Ownership may have changed since preparation. Reconsider in a fresh batch.
            self.more = true;
            return false;
        }
        for path in paths {
            if !self.files.contains(&path) {
                self.files.push(path);
            }
        }
        true
    }
    /// Remove eligible references, then keep metadata still owned by aliases or roots.
    async fn remove_references(
        &mut self,
        txn: &DatabaseTransaction,
        cache: &GlobalCache,
        owners: &CleanupOwners,
        scope: &CleanupScope,
        report: &mut ImagePruneReport,
    ) -> MicrosandboxResult<()> {
        let references = image_ref::Entity::find()
            .find_also_related(manifest::Entity)
            .all(txn)
            .await?;
        let retained_ids = references
            .iter()
            .filter_map(|(_, manifest)| manifest.as_ref())
            .filter(|manifest| owners.retains(manifest))
            .map(|manifest| manifest.id)
            .collect::<HashSet<_>>();
        let retained_paths = references
            .iter()
            .filter(|(reference, _)| retained_ids.contains(&reference.manifest_id))
            .filter_map(|(reference, _)| reference.reference.parse::<Reference>().ok())
            .map(|reference| cache.image_metadata_path(&reference))
            .collect::<HashSet<_>>();
        for (reference, manifest) in references {
            if self
                .remove_reference(txn, cache, owners, scope, reference, manifest)
                .await?
            {
                report.image_refs_removed += 1;
            }
        }

        self.preserve_metadata(txn, cache, owners, &retained_paths)
            .await
    }

    async fn remove_reference(
        &mut self,
        txn: &DatabaseTransaction,
        cache: &GlobalCache,
        owners: &CleanupOwners,
        scope: &CleanupScope,
        reference: image_ref::Model,
        manifest: Option<manifest::Model>,
    ) -> MicrosandboxResult<bool> {
        if let Some((name, _)) = &scope.selected
            && &reference.reference != name
        {
            return Ok(false);
        }
        let Some(manifest) = manifest else {
            return Ok(false);
        };
        if scope.selected.is_some() && !scope.selected_manifests.contains(&manifest.id) {
            return Err(MicrosandboxError::ImageInUse(
                "image reference changed during removal; retry".into(),
            ));
        }
        let retained = owners.retains(&manifest);
        if retained {
            match &scope.selected {
                None => return Ok(false),
                Some((_, false)) => {
                    return Err(MicrosandboxError::ImageInUse(
                        "sandbox or snapshot dependency".into(),
                    ));
                }
                Some((_, true)) => {} // Force may untag; durable backing remains protected below.
            }
        }
        let parsed: Reference = reference
            .reference
            .parse::<Reference>()
            .map_err(|error| MicrosandboxError::InvalidConfig(error.to_string()))?;
        let path = cache.image_metadata_path(&parsed);
        if !self.select(vec![path.clone()]) {
            if scope.selected.is_some() && !self.more {
                return Err(MicrosandboxError::ImageInUse(
                    "active storage operation".into(),
                ));
            }
            return Ok(false);
        }
        if retained {
            // Snapshot export still reads the cached config by its original
            // reference. Untagging must preserve that metadata as well as disks.
            self.files.retain(|candidate| candidate != &path);
        }
        image_ref::Entity::delete_by_id(reference.id)
            .exec(txn)
            .await?;
        Ok(true)
    }

    async fn preserve_metadata(
        &mut self,
        txn: &DatabaseTransaction,
        cache: &GlobalCache,
        owners: &CleanupOwners,
        retained_paths: &HashSet<PathBuf>,
    ) -> MicrosandboxResult<()> {
        // A metadata pathname can have several catalog aliases, including aliases
        // whose older catalog digest differs from the currently published bytes.
        let remaining_paths = image_ref::Entity::find()
            .all(txn)
            .await?
            .into_iter()
            .filter_map(|reference| reference.reference.parse::<Reference>().ok())
            .map(|reference| cache.image_metadata_path(&reference))
            .collect::<HashSet<_>>();
        let retained_digests = manifest::Entity::find()
            .all(txn)
            .await?
            .into_iter()
            .filter(|manifest| owners.retains(manifest))
            .map(|manifest| manifest.digest)
            .collect::<HashSet<_>>();
        let mut deletable = Vec::new();
        for path in &self.files {
            let retained_generation = self
                .metadata_digests
                .get(path)
                .and_then(Option::as_ref)
                .is_some_and(|digest| retained_digests.contains(digest));
            if !remaining_paths.contains(path)
                && !retained_paths.contains(path)
                && !retained_generation
            {
                deletable.push(path.clone());
            }
        }
        self.files = deletable;
        Ok(())
    }

    async fn remove_manifests(
        &mut self,
        txn: &DatabaseTransaction,
        cache: &GlobalCache,
        owners: &CleanupOwners,
        scope: &CleanupScope,
        report: &mut ImagePruneReport,
    ) -> MicrosandboxResult<()> {
        let mut removed = HashSet::new();
        for manifest in manifest::Entity::find().all(txn).await? {
            if scope.selected.is_some() && !scope.selected_manifests.contains(&manifest.id) {
                continue;
            }

            if owners.retains(&manifest) {
                continue;
            }

            if image_ref::Entity::find()
                .filter(image_ref::Column::ManifestId.eq(manifest.id))
                .count(txn)
                .await?
                != 0
            {
                continue;
            }

            let digest: Digest = manifest.digest.parse()?;
            let mut paths = vec![cache.fsmeta_erofs_path(&digest), cache.vmdk_path(&digest)];
            paths.extend(
                self.flat
                    .get(&manifest.id)
                    .map(|claim| claim.reference.clone()),
            );
            if !self.select(paths) {
                continue;
            }

            manifest::Entity::delete_by_id(manifest.id)
                .exec(txn)
                .await?;
            removed.insert(manifest.id);

            report.manifests_removed += 1;
        }

        // A blob goes only with every claimed manifest whose ref names it. Preparation claimed
        // it assuming they would all go, but the transaction may keep some of them.
        let blobs = self
            .flat
            .iter()
            .filter(|(id, claim)| claim.claims_blob && removed.contains(*id))
            .filter_map(|(_, claim)| claim.blob.clone())
            .collect::<Vec<_>>();
        for blob in blobs {
            if self
                .flat
                .iter()
                .all(|(id, claim)| claim.blob.as_ref() != Some(&blob) || removed.contains(id))
            {
                self.select(vec![blob]);
            }
        }

        // No catalog row owns a blob that no ref names.
        for blob in self.unnamed_blobs.clone() {
            self.select(vec![blob]);
        }
        Ok(())
    }

    async fn remove_layers(
        &mut self,
        txn: &DatabaseTransaction,
        cache: &GlobalCache,
        scope: &CleanupScope,
        report: &mut ImagePruneReport,
    ) -> MicrosandboxResult<()> {
        let orphaned = layer::Entity::find()
            .left_join(manifest_layer::Entity)
            .filter(manifest_layer::Column::Id.is_null())
            .all(txn)
            .await?;
        for layer in orphaned {
            if scope.selected.is_some() && !scope.selected_layers.contains(&layer.id) {
                continue;
            }
            if !self.select(vec![cache.layer_erofs_path(&layer.diff_id.parse()?)]) {
                continue;
            }
            layer::Entity::delete_by_id(layer.id).exec(txn).await?;
            report.layers_removed += 1;
        }
        Ok(())
    }

    /// Consume the batch in its blocking worker so claims survive caller cancellation.
    fn finish(
        self,
        journal: Option<Journal>,
        mut report: ImagePruneReport,
    ) -> MicrosandboxResult<(ImagePruneReport, bool, HashSet<PathBuf>)> {
        let _leases = self.leases;
        let removed = match journal {
            Some(journal) => journal.finish(&self.files)?,
            None => Vec::new(),
        };

        for (path, size) in removed {
            report.bytes_reclaimed = Some(report.bytes_reclaimed.unwrap_or(0).saturating_add(size));
            match path
                .parent()
                .and_then(|parent| parent.file_name())
                .and_then(|name| name.to_str())
            {
                Some("fsmeta") => report.fsmeta_removed += 1,
                Some("vmdk") => report.vmdk_removed += 1,
                _ => {}
            }
        }

        Ok((report, self.more, self.skipped))
    }
}

impl CleanupOwners {
    async fn load(txn: &DatabaseTransaction) -> MicrosandboxResult<Self> {
        let sandbox_refs = microsandbox_db::catalog::rootfs_query(txn)
            .await?
            .all(txn)
            .await?
            .into_iter()
            .filter_map(|root| root.manifest_id)
            .collect::<HashSet<_>>();
        let snapshot_refs = snapshot::Entity::find()
            .all(txn)
            .await?
            .into_iter()
            .map(|row| row.image_manifest_digest)
            .collect::<HashSet<_>>();
        Ok(Self {
            sandbox_refs,
            snapshot_refs,
        })
    }

    fn retains(&self, manifest: &manifest::Model) -> bool {
        self.sandbox_refs.contains(&manifest.id) || self.snapshot_refs.contains(&manifest.digest)
    }
}

impl CleanupCatalog {
    async fn load(db: &DbReadConnection) -> MicrosandboxResult<Self> {
        Ok(Self {
            references: image_ref::Entity::find()
                .find_also_related(manifest::Entity)
                .all(db)
                .await?,
            manifests: manifest::Entity::find().all(db).await?,
            layers: layer::Entity::find().all(db).await?,
            manifest_layers: manifest_layer::Entity::find().all(db).await?,
            sandbox_refs: microsandbox_db::catalog::rootfs_query(db)
                .await?
                .all(db)
                .await?
                .into_iter()
                .filter_map(|root| root.manifest_id)
                .collect(),
            snapshot_refs: snapshot::Entity::find()
                .all(db)
                .await?
                .into_iter()
                .map(|row| row.image_manifest_digest)
                .collect(),
        })
    }

    fn retains(&self, manifest: &manifest::Model) -> bool {
        self.sandbox_refs.contains(&manifest.id) || self.snapshot_refs.contains(&manifest.digest)
    }
}

impl PreparedCleanup {
    async fn prepare(
        root: PathBuf,
        cache: GlobalCache,
        catalog: CleanupCatalog,
        selected: Option<(String, bool)>,
        selected_manifests: HashSet<i32>,
        selected_layers: HashSet<i32>,
        skipped: HashSet<PathBuf>,
    ) -> MicrosandboxResult<Self> {
        tokio::task::spawn_blocking(move || {
            let mut cleanup = Cleanup {
                skipped,
                ..Default::default()
            };
            let mut report = ImagePruneReport::default();
            let mut claimed_references = HashSet::new();
            for (reference, manifest) in &catalog.references {
                if let Some((name, _)) = &selected
                    && &reference.reference != name
                {
                    continue;
                }
                let Some(manifest) = manifest else {
                    continue;
                };
                if selected.is_some() && !selected_manifests.contains(&manifest.id) {
                    return Err(MicrosandboxError::ImageInUse(
                        "image reference changed during removal; retry".into(),
                    ));
                }
                if catalog.retains(manifest) {
                    match &selected {
                        None => continue,
                        Some((_, false)) => {
                            return Err(MicrosandboxError::ImageInUse(
                                "sandbox or snapshot dependency".into(),
                            ));
                        }
                        Some((_, true)) => {}
                    }
                }
                let reference_path = cache.image_metadata_path(
                    &reference
                        .reference
                        .parse::<Reference>()
                        .map_err(|error| MicrosandboxError::InvalidConfig(error.to_string()))?,
                );
                if cleanup.claim(vec![reference_path], &mut report)? {
                    claimed_references.insert(reference.id);
                }
            }

            // Simulate only the removals whose entries were claimed. This prevents busy
            // references from filling later batches with backing that cannot be removed.
            let referenced_manifests: HashSet<_> = catalog
                .references
                .iter()
                .filter(|(reference, _)| !claimed_references.contains(&reference.id))
                .map(|(reference, _)| reference.manifest_id)
                .collect();
            let mut claimed_manifests = HashSet::new();
            for manifest in &catalog.manifests {
                if (selected.is_some() && !selected_manifests.contains(&manifest.id))
                    || catalog.retains(manifest)
                    || referenced_manifests.contains(&manifest.id)
                {
                    continue;
                }
                let digest: Digest = manifest.digest.parse()?;
                let deleting = cleanup.files.iter().cloned().collect();
                let flat_paths = cache.flat_rootfs_paths(&digest, &deleting)?;
                if let Some(claim) =
                    cleanup.claim_manifest(&cache, &digest, flat_paths, &mut report)?
                {
                    claimed_manifests.insert(manifest.id);
                    cleanup.flat.insert(manifest.id, claim);
                }
            }

            // Removals prepared concurrently can each keep a blob that the other's ref names,
            // and a replaced ref can leave its old blob behind. No manifest leads to such a blob
            // again, so prune reclaims every blob that no ref names. Its claim settles that no
            // publisher is about to name it; check the names again under it.
            if selected.is_none() {
                let mut candidates = Vec::new();
                for blob in cache.unnamed_flat_blob_paths()? {
                    if !cleanup.leases.contains_key(&blob)
                        && cleanup.claim(vec![blob.clone()], &mut report)?
                    {
                        candidates.push(blob);
                    }
                }
                let named = cache.named_flat_blob_paths(&HashSet::new())?;
                for blob in candidates {
                    if named.contains(&blob) {
                        cleanup.release(&blob);
                    } else {
                        cleanup.unnamed_blobs.push(blob);
                    }
                }
            }

            let retained_layers: HashSet<_> = catalog
                .manifest_layers
                .iter()
                .filter(|link| !claimed_manifests.contains(&link.manifest_id))
                .map(|link| link.layer_id)
                .collect();
            for layer in &catalog.layers {
                if (selected.is_some() && !selected_layers.contains(&layer.id))
                    || retained_layers.contains(&layer.id)
                {
                    continue;
                }
                cleanup.claim(
                    vec![cache.layer_erofs_path(&layer.diff_id.parse()?)],
                    &mut report,
                )?;
            }

            for path in &cleanup.files {
                if path.parent() != Some(cache.manifests_dir()) {
                    continue;
                }
                let bytes = match std::fs::read(path) {
                    Ok(bytes) => Some(bytes),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                    Err(error) => return Err(error.into()),
                };
                let digest = bytes
                    .as_deref()
                    .and_then(|bytes| serde_json::from_slice::<CachedImageMetadata>(bytes).ok())
                    .map(|metadata| metadata.manifest_digest);
                cleanup.metadata_digests.insert(path.clone(), digest);
            }
            let journal = Journal::prepare(&root, &cleanup.files)?;
            Ok(Self {
                cleanup,
                journal,
                skipped_in_use: report.skipped_in_use,
            })
        })
        .await
        .map_err(|error| MicrosandboxError::Custom(format!("image cleanup preparation: {error}")))?
    }

    /// Recheck owners and commit only prepared entries, then finish filesystem cleanup.
    async fn commit(
        self,
        local: &LocalBackend,
        cache: GlobalCache,
        selected: Option<(String, bool)>,
        selected_manifests: HashSet<i32>,
        selected_layers: HashSet<i32>,
    ) -> MicrosandboxResult<(ImagePruneReport, bool, HashSet<PathBuf>)> {
        let scope = CleanupScope {
            selected,
            selected_manifests,
            selected_layers,
        };
        let pools = local.db().await?;
        let (report, cleanup) = pools
            .write()
            .transaction(|txn| {
                let scope = scope.clone();
                let cache = cache.clone();
                let mut cleanup = self.cleanup.clone();
                cleanup.files.clear();
                cleanup.more = false;
                let skipped_in_use = self.skipped_in_use;

                async move {
                    let owners = CleanupOwners::load(&txn).await?;
                    let mut report = ImagePruneReport {
                        skipped_in_use,
                        ..Default::default()
                    };
                    cleanup
                        .remove_references(&txn, &cache, &owners, &scope, &mut report)
                        .await?;
                    cleanup
                        .remove_manifests(&txn, &cache, &owners, &scope, &mut report)
                        .await?;
                    cleanup
                        .remove_layers(&txn, &cache, &scope, &mut report)
                        .await?;

                    Ok::<_, MicrosandboxError>((txn, (report, cleanup)))
                }
            })
            .await?;

        // Catalog ownership is gone before unlink; the worker owns claims through disposal.
        let journal = self.journal;
        tokio::task::spawn_blocking(move || cleanup.finish(journal, report))
            .await
            .map_err(|error| MicrosandboxError::Custom(format!("image cleanup task: {error}")))?
    }
}

//--------------------------------------------------------------------------------------------------
// Tests
//--------------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use std::process::Command;
    use std::sync::Arc;
    use std::time::Duration;

    use microsandbox_image::{
        CachedImageMetadata, CachedLayerMetadata, FlatRootfsRef, ImageConfig,
    };
    use sea_orm::ConnectionTrait;

    use super::*;
    use crate::Image;

    async fn local(home: &std::path::Path) -> LocalBackend {
        crate::test_support::local_backend_builder(home)
            .build()
            .await
            .unwrap()
    }

    fn metadata(id: char, layer: char) -> CachedImageMetadata {
        CachedImageMetadata {
            manifest_digest: format!("sha256:{}", id.to_string().repeat(64)),
            config_digest: format!("sha256:{}", id.to_string().repeat(64)),
            raw_manifest_json: "{}".into(),
            raw_config_json: "{}".into(),
            config: ImageConfig::default(),
            layers: vec![CachedLayerMetadata {
                digest: format!("sha256:{}", layer.to_string().repeat(64)),
                media_type: None,
                size_bytes: Some(7),
                diff_id: format!("sha256:{}", layer.to_string().repeat(64)),
            }],
        }
    }

    async fn install(
        local: &LocalBackend,
        reference: &str,
        metadata: &CachedImageMetadata,
    ) -> GlobalCache {
        let cache = GlobalCache::new(&local.cache_dir()).unwrap();
        for path in cache.metadata_paths(metadata).unwrap() {
            std::fs::write(path, b"payload").unwrap();
        }
        cache
            .write_image_metadata_async(&reference.parse().unwrap(), metadata)
            .await
            .unwrap();
        Image::persist(local, reference, metadata.clone())
            .await
            .unwrap();
        cache
    }

    /// Publish a flat ref for `metadata`'s manifest and the blob it names.
    fn publish_flat(
        cache: &GlobalCache,
        metadata: &CachedImageMetadata,
    ) -> (std::path::PathBuf, std::path::PathBuf) {
        let manifest: Digest = metadata.manifest_digest.parse().unwrap();
        let artifact: Digest = format!("sha256:{}", "f".repeat(64)).parse().unwrap();
        let blob = cache.flat_blob_path(&artifact);
        std::fs::write(&blob, vec![1u8; 8192]).unwrap();
        let reference = FlatRootfsRef {
            schema: 1,
            manifest_digest: metadata.manifest_digest.clone(),
            derivation_digest: format!("sha256:{}", "e".repeat(64)),
            artifact_digest: artifact.to_string(),
            materializer_abi: 1,
            uuid: "00".repeat(16),
            virtual_size_bytes: 8192,
            inode_count: 2,
            content_bytes: 0,
        };
        cache.write_flat_ref(&manifest, &reference).unwrap();
        (cache.flat_ref_path(&manifest), blob)
    }

    /// A removal whose catalog transaction committed, but whose process died before it unlinked
    /// the files it journaled.
    async fn abandon_committed_removal(local: &LocalBackend, files: &[std::path::PathBuf]) {
        drop(Journal::prepare(&local.cache_dir(), files).unwrap());
        let db = local.db().await.unwrap();
        image_ref::Entity::delete_many()
            .exec(db.write())
            .await
            .unwrap();
        manifest::Entity::delete_many()
            .exec(db.write())
            .await
            .unwrap();
        layer::Entity::delete_many().exec(db.write()).await.unwrap();
    }

    /// Rerun `test` in a child process, so that no forked sibling test retains its leases.
    #[cfg(unix)]
    fn run_isolated(test: &str, variable: &str) {
        let status = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", test, "--nocapture"])
            .env(variable, "1")
            .status()
            .unwrap();
        assert!(status.success(), "isolated {test} failed: {status}");
    }

    #[tokio::test]
    async fn busy_flat_blob_is_skipped_by_normal_removal() {
        #[cfg(unix)]
        if std::env::var_os("MSB_TEST_BUSY_FLAT_BLOB").is_none() {
            run_isolated(
                "image::cleanup::tests::busy_flat_blob_is_skipped_by_normal_removal",
                "MSB_TEST_BUSY_FLAT_BLOB",
            );
            return;
        }

        let home = tempfile::tempdir().unwrap();
        let local = local(home.path()).await;
        let reference = "example.com/a:latest";
        let metadata = metadata('a', 'c');
        let cache = install(&local, reference, &metadata).await;
        let (flat_ref, blob) = publish_flat(&cache, &metadata);
        let operation = cache.operation();
        operation.lease_paths(vec![blob.clone()]).unwrap();
        // Removal untags the image but keeps the manifest whose flat root is busy.
        let report = Image::prune_local(&local).await.unwrap();
        assert_eq!(report.image_refs_removed, 1);
        assert_eq!(report.manifests_removed, 0);
        assert_eq!(report.skipped_in_use, 1);
        assert!(flat_ref.exists());
        assert!(blob.exists());
        drop(operation);
        let report = Image::prune_local(&local).await.unwrap();
        assert_eq!(report.manifests_removed, 1);
        assert!(!flat_ref.exists());
        assert!(!blob.exists());
    }

    #[tokio::test]
    async fn journal_recovery_keeps_a_leased_flat_root_until_it_is_released() {
        #[cfg(unix)]
        if std::env::var_os("MSB_TEST_JOURNALED_FLAT_ROOT").is_none() {
            run_isolated(
                "image::cleanup::tests::journal_recovery_keeps_a_leased_flat_root_until_it_is_released",
                "MSB_TEST_JOURNALED_FLAT_ROOT",
            );
            return;
        }

        let home = tempfile::tempdir().unwrap();
        let local = local(home.path()).await;
        let reference = "example.com/a:latest";
        let metadata = metadata('a', 'c');
        let cache = install(&local, reference, &metadata).await;
        let (flat_ref, blob) = publish_flat(&cache, &metadata);
        let files = [flat_ref.clone(), blob.clone()];
        abandon_committed_removal(&local, &files).await;

        // The image is imported again, reusing the files, before its catalog rows commit.
        let operation = cache.operation();
        operation.lease_paths(files.to_vec()).unwrap();
        Image::prune_local(&local).await.unwrap();
        assert!(flat_ref.exists());
        assert!(blob.exists());
        Image::persist(&local, reference, metadata.clone())
            .await
            .unwrap();
        drop(operation);
        Journal::recover(&local).await.unwrap();
        assert!(flat_ref.exists());
        assert!(blob.exists());

        // Without a publisher, recovery reclaims what the committed removal journaled.
        abandon_committed_removal(&local, &files).await;
        Journal::recover(&local).await.unwrap();
        assert!(!flat_ref.exists());
        assert!(!blob.exists());
    }

    #[tokio::test]
    async fn shared_flat_blob_stays_while_a_rechecked_owner_keeps_one_of_its_refs() {
        use sea_orm::Set;

        let home = tempfile::tempdir().unwrap();
        let local = local(home.path()).await;
        let kept = "example.com/a:latest";
        let kept_metadata = metadata('a', 'c');
        let cache = install(&local, kept, &kept_metadata).await;
        let removed_metadata = metadata('b', 'd');
        install(&local, "example.com/b:latest", &removed_metadata).await;
        let (kept_ref, blob) = publish_flat(&cache, &kept_metadata);
        let (removed_ref, same_blob) = publish_flat(&cache, &removed_metadata);
        assert_eq!(blob, same_blob);
        let catalog = CleanupCatalog::load(local.db().await.unwrap().read())
            .await
            .unwrap();

        // Preparation claims the blob with the second manifest, assuming both go. An owner
        // added after the advisory read keeps the first through the transaction's recheck.
        let now = chrono::Utc::now().naive_utc();
        snapshot::Entity::insert(snapshot::ActiveModel {
            artifact_path: Set(home.path().join("saved").display().to_string()),
            digest: Set("saved-descriptor".into()),
            scope: Set("disk".into()),
            state_kind: Set("file".into()),
            image_ref: Set(kept.into()),
            image_manifest_digest: Set(kept_metadata.manifest_digest.clone()),
            locality: Set("embedded".into()),
            availability: Set("available".into()),
            migration_state: Set("current".into()),
            created_at: Set(now),
            indexed_at: Set(now),
            child_count: Set(0),
            ..Default::default()
        })
        .exec(local.db().await.unwrap().write())
        .await
        .unwrap();

        let prepared = PreparedCleanup::prepare(
            local.cache_dir(),
            cache.clone(),
            catalog,
            None,
            HashSet::new(),
            HashSet::new(),
            HashSet::new(),
        )
        .await
        .unwrap();
        assert!(
            prepared.cleanup.files.contains(&blob),
            "preparation should claim the shared blob"
        );
        let (report, _, _) = prepared
            .commit(&local, cache.clone(), None, HashSet::new(), HashSet::new())
            .await
            .unwrap();

        assert_eq!(report.manifests_removed, 1);
        assert!(!removed_ref.exists());
        assert!(kept_ref.exists());
        assert!(blob.exists(), "a kept ref still names the blob");
    }

    #[tokio::test]
    async fn aliases_share_one_deletion_lease_and_leave_no_broken_reference() {
        let home = tempfile::tempdir().unwrap();
        let local = local(home.path()).await;
        let data = metadata('a', 'c');
        let cache = install(&local, "audit-tiny:latest", &data).await;
        install(&local, "docker.io/library/audit-tiny:latest", &data).await;
        let report = Image::prune_local(&local).await.unwrap();
        assert_eq!(report.image_refs_removed, 2);
        assert_eq!(report.skipped_in_use, 0);
        assert_eq!(report.layers_removed, 1);
        assert!(
            !cache
                .image_metadata_path(&"audit-tiny:latest".parse().unwrap())
                .exists()
        );
    }

    #[tokio::test]
    async fn cleanup_spans_bounded_batches() {
        #[cfg(unix)]
        if std::env::var_os("MSB_TEST_BOUNDED_PRUNE").is_none() {
            use std::os::unix::process::CommandExt;
            let mut child = std::process::Command::new(std::env::current_exe().unwrap());
            child
                .args([
                    "--exact",
                    "image::cleanup::tests::cleanup_spans_bounded_batches",
                    "--nocapture",
                ])
                .env("MSB_TEST_BOUNDED_PRUNE", "1");
            // Isolate the process-wide descriptor limit from other parallel tests.
            unsafe {
                child.pre_exec(|| {
                    let limit = libc::rlimit {
                        rlim_cur: 256,
                        rlim_max: 256,
                    };
                    if libc::setrlimit(libc::RLIMIT_NOFILE, &limit) != 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
            assert!(child.status().unwrap().success());
            return;
        }
        let home = tempfile::tempdir().unwrap();
        let local = local(home.path()).await;
        for index in 0..80 {
            let mut data = metadata('a', 'c');
            data.manifest_digest = format!("sha256:{index:064x}");
            install(&local, &format!("example.com/image-{index}:latest"), &data).await;
        }
        let report = Image::prune_local(&local).await.unwrap();
        assert_eq!(report.image_refs_removed, 80);
        assert_eq!(report.manifests_removed, 80);
        assert_eq!(report.layers_removed, 1);
        assert_eq!(report.skipped_in_use, 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn journal_preparation_does_not_hold_the_catalog_writer() {
        let home = tempfile::tempdir().unwrap();
        let local = Arc::new(local(home.path()).await);
        let reference = "example.com/a:latest";
        let cache = install(&local, reference, &metadata('a', 'c')).await;
        let path = cache.image_metadata_path(&reference.parse().unwrap());
        let directory = local.cache_dir().join(".image-deletions");
        std::fs::create_dir_all(&directory).unwrap();
        let coordinator =
            microsandbox_utils::process_lock::open_lock_file(&directory.join("coordination.lock"))
                .unwrap();
        microsandbox_utils::process_lock::lock_exclusive(&coordinator).unwrap();
        let pruning_local = Arc::clone(&local);
        let pruning = tokio::spawn(async move {
            loop {
                let report = Image::prune_local(&pruning_local).await.unwrap();
                if report.skipped_in_use == 0 {
                    return report;
                }
                // The nonblocking probe below can briefly win the entry claim.
                tokio::task::yield_now().await;
            }
        });

        let claimed = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if StorageLease::try_exclusive(&path).unwrap().is_none() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await;
        let write =
            tokio::time::timeout(
                Duration::from_secs(1),
                local.db().await.unwrap().write().execute_unprepared(
                    "CREATE TABLE unrelated_prune_writer (id INTEGER PRIMARY KEY)",
                ),
            )
            .await;

        // Always unblock the journal and join cleanup before asserting failures.
        drop(coordinator);
        let report = tokio::time::timeout(Duration::from_secs(5), pruning)
            .await
            .unwrap()
            .unwrap();

        assert!(claimed.is_ok(), "prune must reach journal preparation");
        assert!(
            write.is_ok(),
            "journal waits must not occupy the catalog writer"
        );
        write.unwrap().unwrap();
        assert_eq!(report.image_refs_removed, 1);
    }

    #[tokio::test]
    async fn prune_failure_reports_completed_batches_without_claiming_success() {
        for image_count in [1, 80] {
            let home = tempfile::tempdir().unwrap();
            let local = local(home.path()).await;
            let data = metadata('a', 'c');
            let mut metadata_files = Vec::new();
            for index in 0..image_count {
                let reference = format!("example.com/image-{index}:latest");
                let cache = install(&local, &reference, &data).await;
                let path = cache.image_metadata_path(&reference.parse().unwrap());
                let size = std::fs::metadata(&path).unwrap().len();
                metadata_files.push((path, size));
            }
            let pools = local.db().await.unwrap();
            // Shared backing cannot be removed until every reference has gone. With
            // many references, earlier batches commit before this deletion is attempted.
            pools
                .write()
                .execute_unprepared(
                    "CREATE TRIGGER fail_manifest_delete BEFORE DELETE ON manifest
                     BEGIN SELECT RAISE(ABORT, 'injected prune failure'); END",
                )
                .await
                .unwrap();

            let error = Image::prune_local(&local).await.unwrap_err();
            let remaining = image_ref::Entity::find().count(pools.read()).await.unwrap();
            let removed = image_count - remaining;
            let message = error.to_string();

            assert!(message.contains("injected prune failure"), "{message}");
            assert_eq!(
                manifest::Entity::find().count(pools.read()).await.unwrap(),
                1
            );
            assert_eq!(layer::Entity::find().count(pools.read()).await.unwrap(), 1);
            let removed_files: Vec<_> = metadata_files
                .iter()
                .filter(|(path, _)| !path.exists())
                .collect();
            assert_eq!(removed_files.len() as u64, removed);

            if image_count == 1 {
                assert_eq!(removed, 0);
                assert!(matches!(error, MicrosandboxError::Database(_)));
            } else {
                assert!(removed > 0 && remaining > 0);
                let reclaimed: u64 = removed_files.iter().map(|(_, size)| *size).sum();
                assert!(
                    message.contains(&format!("{removed} image references")),
                    "{message}"
                );
                assert!(message.contains("0 manifests, 0 layers"), "{message}");
                assert!(
                    message.contains(&format!("{reclaimed} bytes reclaimed")),
                    "{message}"
                );
                assert!(message.contains("failing batch may have made additional progress"));
            }
        }
    }

    #[tokio::test]
    async fn interrupted_journal_disposal_without_active_lock_is_recovered() {
        let home = tempfile::tempdir().unwrap();
        let local = local(home.path()).await;
        let journal = local
            .cache_dir()
            .join(".image-deletions/delete-interrupted");
        std::fs::create_dir_all(&journal).unwrap();
        std::fs::write(journal.join("0"), b"retained orphan").unwrap();
        std::fs::write(journal.join("files.json"), b"[]").unwrap();
        Journal::recover(&local).await.unwrap();
        assert!(!journal.exists());
    }

    #[tokio::test]
    async fn busy_image_does_not_block_unrelated_pruning_or_lose_shared_layer() {
        #[cfg(unix)]
        if std::env::var_os("MSB_TEST_BUSY_IMAGE_PRUNE").is_none() {
            // Parallel tests can fork with our lease descriptors open and retain their
            // locks until exec, even after drop(operation). Create the leases in an
            // isolated process so the final prune can assert immediate reclamation.
            let status = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "image::cleanup::tests::busy_image_does_not_block_unrelated_pruning_or_lose_shared_layer",
                    "--nocapture",
                ])
                .env("MSB_TEST_BUSY_IMAGE_PRUNE", "1")
                .status()
                .unwrap();
            assert!(
                status.success(),
                "isolated image prune test failed: {status}"
            );
            return;
        }

        let home = tempfile::tempdir().unwrap();
        let local = local(home.path()).await;
        let a = "example.com/a:latest";
        let b = "example.com/b:latest";
        let metadata_a = metadata('a', 'c');
        let cache = install(&local, a, &metadata_a).await;
        install(&local, b, &metadata('b', 'c')).await;
        let operation = cache.operation();
        operation
            .lease_paths(vec![cache.image_metadata_path(&a.parse().unwrap())])
            .unwrap();
        let report = Image::prune_local(&local).await.unwrap();
        assert_eq!(report.image_refs_removed, 1);
        assert_eq!(report.skipped_in_use, 1);
        assert_eq!(report.layers_removed, 0);
        assert!(Image::get_local(&local, a).await.is_ok());
        assert!(Image::get_local(&local, b).await.is_err());
        assert!(
            cache
                .layer_erofs_path(&metadata_a.layers[0].diff_id.parse().unwrap())
                .exists()
        );
        assert!(
            Image::remove_local(&local, a, true)
                .await
                .unwrap_err()
                .to_string()
                .contains("active storage operation")
        );
        drop(operation);
        let report = Image::prune_local(&local).await.unwrap();
        assert_eq!(report.layers_removed, 1);
    }

    #[tokio::test]
    async fn shared_layer_publisher_is_protected_before_new_catalog_reference_exists() {
        let home = tempfile::tempdir().unwrap();
        let local = local(home.path()).await;
        let old = metadata('a', 'c');
        let cache = install(&local, "example.com/old:latest", &old).await;
        let operation = cache.operation();
        operation
            .lease_paths(cache.metadata_paths(&old).unwrap())
            .unwrap();
        let report = Image::prune_local(&local).await.unwrap();
        assert!(report.skipped_in_use > 0);
        let new = metadata('b', 'c');
        Image::persist(&local, "example.com/new:latest", new)
            .await
            .unwrap();
        drop(operation);
        assert!(
            cache
                .layer_erofs_path(&old.layers[0].diff_id.parse().unwrap())
                .exists()
        );
        Image::prune_local(&local).await.unwrap();
    }

    #[tokio::test]
    async fn pending_snapshot_publication_roots_survive_prune_until_abandoned() {
        use sea_orm::Set;
        let home = tempfile::tempdir().unwrap();
        let local = local(home.path()).await;
        let metadata = metadata('a', 'c');
        let reference = "example.com/a:latest";
        let cache = install(&local, reference, &metadata).await;
        let path = home.path().join("not-published-yet");
        let publisher = microsandbox_image::storage_lease::StorageLease::shared(&path).unwrap();
        let now = chrono::Utc::now().naive_utc();
        // Publication installs this durable root before renaming any snapshot payload.
        snapshot::Entity::insert(snapshot::ActiveModel {
            artifact_path: Set(path.display().to_string()),
            digest: Set("pending-descriptor".into()),
            scope: Set("disk".into()),
            state_kind: Set("file".into()),
            image_ref: Set(reference.into()),
            image_manifest_digest: Set(metadata.manifest_digest.clone()),
            locality: Set("embedded".into()),
            availability: Set("publishing".into()),
            migration_state: Set("current".into()),
            created_at: Set(now),
            indexed_at: Set(now),
            child_count: Set(0),
            ..Default::default()
        })
        .exec(local.db().await.unwrap().write())
        .await
        .unwrap();
        assert_eq!(
            Image::prune_local(&local).await.unwrap().image_refs_removed,
            0
        );
        for path in cache.metadata_paths(&metadata).unwrap() {
            assert!(path.exists(), "{}", path.display());
        }
        // A dead publisher with no installed artifact must not leak an image root forever.
        drop(publisher);
        let report = Image::prune_local(&local).await.unwrap();
        assert_eq!(report.image_refs_removed, 1);
        assert_eq!(report.layers_removed, 1);
        assert!(
            snapshot::Entity::find()
                .all(local.db().await.unwrap().read())
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn force_untags_but_preserves_snapshot_backing_until_last_dependency_is_gone() {
        use sea_orm::Set;
        let home = tempfile::tempdir().unwrap();
        let local = local(home.path()).await;
        let metadata = metadata('a', 'c');
        let reference = "example.com/a:latest";
        let cache = install(&local, reference, &metadata).await;
        let now = chrono::Utc::now().naive_utc();
        snapshot::Entity::insert(snapshot::ActiveModel {
            artifact_path: Set(home.path().join("saved").display().to_string()),
            digest: Set("saved-descriptor".into()),
            scope: Set("disk".into()),
            state_kind: Set("file".into()),
            image_ref: Set(reference.into()),
            image_manifest_digest: Set(metadata.manifest_digest.clone()),
            locality: Set("embedded".into()),
            availability: Set("available".into()),
            migration_state: Set("current".into()),
            created_at: Set(now),
            indexed_at: Set(now),
            child_count: Set(0),
            ..Default::default()
        })
        .exec(local.db().await.unwrap().write())
        .await
        .unwrap();
        assert_eq!(
            Image::prune_local(&local).await.unwrap().image_refs_removed,
            0
        );
        assert!(Image::remove_local(&local, reference, false).await.is_err());
        Image::remove_local(&local, reference, true).await.unwrap();
        assert_eq!(
            cache
                .read_image_metadata_async(&reference.parse().unwrap())
                .await
                .unwrap()
                .unwrap()
                .manifest_digest,
            metadata.manifest_digest,
        );
        for path in cache.metadata_paths(&metadata).unwrap() {
            assert!(path.exists(), "{}", path.display());
        }
        assert_eq!(Image::prune_local(&local).await.unwrap().layers_removed, 0);
        snapshot::Entity::delete_many()
            .exec(local.db().await.unwrap().write())
            .await
            .unwrap();
        assert_eq!(Image::prune_local(&local).await.unwrap().layers_removed, 1);
    }

    #[tokio::test]
    async fn cancelling_cleanup_keeps_worker_claims_until_journal_disposal() {
        let home = tempfile::tempdir().unwrap();
        let local = Arc::new(local(home.path()).await);
        let reference = "example.com/a:latest";
        let cache = install(&local, reference, &metadata('a', 'c')).await;
        let path = cache.image_metadata_path(&reference.parse().unwrap());
        let catalog = CleanupCatalog::load(local.db().await.unwrap().read())
            .await
            .unwrap();
        let prepared = PreparedCleanup::prepare(
            local.cache_dir(),
            cache.clone(),
            catalog,
            None,
            HashSet::new(),
            HashSet::new(),
            HashSet::new(),
        )
        .await
        .unwrap();
        let directory = local.cache_dir().join(".image-deletions");
        let coordinator =
            microsandbox_utils::process_lock::open_lock_file(&directory.join("coordination.lock"))
                .unwrap();
        microsandbox_utils::process_lock::lock_exclusive(&coordinator).unwrap();
        let cleaning_local = Arc::clone(&local);
        let cleaning = tokio::spawn(async move {
            prepared
                .commit(&cleaning_local, cache, None, HashSet::new(), HashSet::new())
                .await
        });

        // The worker unlinks first, then waits on coordination to dispose its journal.
        let unlinked = tokio::time::timeout(Duration::from_secs(2), async {
            while path.exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await;
        cleaning.abort();
        let cancelled = cleaning.await;
        let claim = StorageLease::try_exclusive(&path).unwrap();

        // Always let the detached worker finish, including when an assertion will fail.
        drop(coordinator);
        let disposed = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let pending = std::fs::read_dir(&directory).unwrap().any(|entry| {
                    entry
                        .unwrap()
                        .file_name()
                        .to_string_lossy()
                        .starts_with("delete-")
                });
                if !pending {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await;

        assert!(unlinked.is_ok(), "cleanup must reach journal disposal");
        assert!(cancelled.unwrap_err().is_cancelled());
        assert!(
            claim.is_none(),
            "detached cleanup must retain its entry claims"
        );
        assert!(disposed.is_ok(), "detached cleanup must finish its journal");
    }

    #[tokio::test]
    async fn prepared_cleanup_rechecks_new_owners_before_unlinking() {
        use sea_orm::Set;

        let home = tempfile::tempdir().unwrap();
        let local = local(home.path()).await;
        let metadata = metadata('a', 'c');
        let reference = "example.com/a:latest";
        let cache = install(&local, reference, &metadata).await;
        let catalog = CleanupCatalog::load(local.db().await.unwrap().read())
            .await
            .unwrap();

        // Publication can add an owner after the advisory read but before entry claims.
        let now = chrono::Utc::now().naive_utc();
        snapshot::Entity::insert(snapshot::ActiveModel {
            artifact_path: Set(home.path().join("saved").display().to_string()),
            digest: Set("saved-descriptor".into()),
            scope: Set("disk".into()),
            state_kind: Set("file".into()),
            image_ref: Set(reference.into()),
            image_manifest_digest: Set(metadata.manifest_digest.clone()),
            locality: Set("embedded".into()),
            availability: Set("available".into()),
            migration_state: Set("current".into()),
            created_at: Set(now),
            indexed_at: Set(now),
            child_count: Set(0),
            ..Default::default()
        })
        .exec(local.db().await.unwrap().write())
        .await
        .unwrap();

        let prepared = PreparedCleanup::prepare(
            local.cache_dir(),
            cache.clone(),
            catalog,
            None,
            HashSet::new(),
            HashSet::new(),
            HashSet::new(),
        )
        .await
        .unwrap();
        let (report, _, _) = prepared
            .commit(&local, cache.clone(), None, HashSet::new(), HashSet::new())
            .await
            .unwrap();

        assert_eq!(report.image_refs_removed, 0);
        assert_eq!(report.manifests_removed, 0);
        assert_eq!(report.layers_removed, 0);
        assert!(
            cache
                .image_metadata_path(&reference.parse().unwrap())
                .exists()
        );
        for path in cache.metadata_paths(&metadata).unwrap() {
            assert!(path.exists(), "{}", path.display());
        }
        assert!(Image::get_local(&local, reference).await.is_ok());
    }

    #[tokio::test]
    async fn interrupted_cleanup_rechecks_catalog_and_never_unlinks_replacement() {
        let home = tempfile::tempdir().unwrap();
        let local = local(home.path()).await;
        let metadata = metadata('a', 'c');
        let cache = install(&local, "example.com/a:latest", &metadata).await;
        let path = cache.layer_erofs_path(&metadata.layers[0].diff_id.parse().unwrap());
        // A crash after preparing the full batch but before commit must preserve owners.
        let catalog = CleanupCatalog::load(local.db().await.unwrap().read())
            .await
            .unwrap();
        let prepared = PreparedCleanup::prepare(
            local.cache_dir(),
            cache.clone(),
            catalog,
            None,
            HashSet::new(),
            HashSet::new(),
            HashSet::new(),
        )
        .await
        .unwrap();
        drop(prepared);
        Journal::recover(&local).await.unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"payload");
        assert!(
            cache
                .image_metadata_path(&"example.com/a:latest".parse().unwrap())
                .exists()
        );
        for path in cache.metadata_paths(&metadata).unwrap() {
            assert!(path.exists(), "{}", path.display());
        }
        // A committed deletion followed by a new publication at the same name. Recovery
        // owns an old hard link, not permission to unlink the new generation.
        let journal = Journal::prepare(&local.cache_dir(), std::slice::from_ref(&path)).unwrap();
        drop(journal);
        image_ref::Entity::delete_many()
            .exec(local.db().await.unwrap().write())
            .await
            .unwrap();
        manifest::Entity::delete_many()
            .exec(local.db().await.unwrap().write())
            .await
            .unwrap();
        layer::Entity::delete_many()
            .exec(local.db().await.unwrap().write())
            .await
            .unwrap();
        std::fs::remove_file(&path).unwrap();
        std::fs::write(&path, b"replacement").unwrap();
        Journal::recover(&local).await.unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"replacement");
        // With no replacement, the admitted orphan is removed on the next recovery.
        let journal = Journal::prepare(&local.cache_dir(), std::slice::from_ref(&path)).unwrap();
        drop(journal);
        Journal::recover(&local).await.unwrap();
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn recovery_discards_abandoned_preparation_without_unlinking_sources() {
        let home = tempfile::tempdir().unwrap();
        let local = local(home.path()).await;
        let metadata = metadata('a', 'c');
        let cache = install(&local, "example.com/a:latest", &metadata).await;
        let source = cache.layer_erofs_path(&metadata.layers[0].diff_id.parse().unwrap());
        let stage = local
            .cache_dir()
            .join(".image-deletions/.prepare-interrupted");
        std::fs::create_dir_all(&stage).unwrap();
        let active =
            microsandbox_utils::process_lock::open_lock_file(&stage.join("active.lock")).unwrap();
        microsandbox_utils::process_lock::lock_exclusive(&active).unwrap();
        std::fs::hard_link(&source, stage.join("0")).unwrap();
        Journal::recover(&local).await.unwrap();
        assert!(stage.exists());
        drop(active);
        Journal::recover(&local).await.unwrap();
        assert!(!stage.exists());
        assert_eq!(std::fs::read(source).unwrap(), b"payload");
    }

    #[tokio::test]
    async fn concurrent_pruners_preserve_stable_lock_files() {
        let home = tempfile::tempdir().unwrap();
        let local = local(home.path()).await;
        let metadata = metadata('a', 'c');
        let cache = install(&local, "example.com/a:latest", &metadata).await;
        let materializer =
            cache.layer_erofs_lock_path(&metadata.layers[0].diff_id.parse().unwrap());
        let lock = microsandbox_utils::process_lock::open_lock_file(&materializer).unwrap();
        let (first, second) = tokio::join!(Image::prune_local(&local), Image::prune_local(&local));
        assert!(first.is_ok(), "{first:?}");
        assert!(second.is_ok(), "{second:?}");
        assert!(materializer.exists());
        assert_eq!(lock.metadata().unwrap().len(), 0);
    }

    #[test]
    fn manifest_claim_keeps_a_blob_that_a_ref_came_to_name_before_the_claim() {
        let home = tempfile::tempdir().unwrap();
        let cache = GlobalCache::new(home.path()).unwrap();
        let removed = metadata('a', 'c');
        let digest: Digest = removed.manifest_digest.parse().unwrap();
        let (reference, blob) = publish_flat(&cache, &removed);
        let flat_paths = cache.flat_rootfs_paths(&digest, &HashSet::new()).unwrap();
        assert!(flat_paths.contains(&blob));

        // Another image publishes a ref naming the same blob after preparation read the names.
        publish_flat(&cache, &metadata('b', 'd'));
        let mut cleanup = Cleanup::default();
        let mut report = ImagePruneReport::default();
        let claim = cleanup
            .claim_manifest(&cache, &digest, flat_paths, &mut report)
            .unwrap()
            .unwrap();

        assert_eq!(claim.blob.as_ref(), Some(&blob));
        assert!(!claim.claims_blob);
        assert!(cleanup.files.contains(&reference));
        assert!(!cleanup.files.contains(&blob));
        assert!(!cleanup.leases.contains_key(&blob));
    }

    #[test]
    fn manifest_claim_follows_a_ref_replaced_before_the_claim() {
        let home = tempfile::tempdir().unwrap();
        let cache = GlobalCache::new(home.path()).unwrap();
        let removed = metadata('a', 'c');
        let digest: Digest = removed.manifest_digest.parse().unwrap();
        let (_, old_blob) = publish_flat(&cache, &removed);
        let flat_paths = cache.flat_rootfs_paths(&digest, &HashSet::new()).unwrap();

        // A forced re-pull replaces the ref with one naming a new blob, and removes the old one.
        let artifact: Digest = format!("sha256:{}", "e".repeat(64)).parse().unwrap();
        let new_blob = cache.flat_blob_path(&artifact);
        std::fs::write(&new_blob, vec![2u8; 8192]).unwrap();
        let mut root = cache.read_flat_ref(&digest).unwrap().unwrap();
        root.artifact_digest = artifact.to_string();
        cache.write_flat_ref(&digest, &root).unwrap();
        std::fs::remove_file(&old_blob).unwrap();
        let mut cleanup = Cleanup::default();
        let mut report = ImagePruneReport::default();
        let claim = cleanup
            .claim_manifest(&cache, &digest, flat_paths, &mut report)
            .unwrap()
            .unwrap();

        assert_eq!(claim.blob.as_ref(), Some(&new_blob));
        assert!(claim.claims_blob);
        assert!(cleanup.files.contains(&new_blob));
        assert!(cleanup.leases.contains_key(&new_blob));
        assert!(!cleanup.files.contains(&old_blob));
        assert!(!cleanup.leases.contains_key(&old_blob));
    }

    #[test]
    fn manifest_claim_leaves_a_busy_blob_that_a_replaced_ref_names() {
        let home = tempfile::tempdir().unwrap();
        let cache = GlobalCache::new(home.path()).unwrap();
        let removed = metadata('a', 'c');
        let digest: Digest = removed.manifest_digest.parse().unwrap();
        let (reference, old_blob) = publish_flat(&cache, &removed);
        let flat_paths = cache.flat_rootfs_paths(&digest, &HashSet::new()).unwrap();

        // The ref is replaced before the claim, and a reader still leases the new blob.
        let artifact: Digest = format!("sha256:{}", "e".repeat(64)).parse().unwrap();
        let new_blob = cache.flat_blob_path(&artifact);
        std::fs::write(&new_blob, vec![2u8; 8192]).unwrap();
        let mut root = cache.read_flat_ref(&digest).unwrap().unwrap();
        root.artifact_digest = artifact.to_string();
        cache.write_flat_ref(&digest, &root).unwrap();
        std::fs::remove_file(&old_blob).unwrap();
        let reader = cache.operation();
        reader.lease_paths(vec![new_blob.clone()]).unwrap();
        let mut cleanup = Cleanup::default();
        let mut report = ImagePruneReport::default();
        let claim = cleanup
            .claim_manifest(&cache, &digest, flat_paths, &mut report)
            .unwrap()
            .unwrap();

        assert!(!claim.claims_blob);
        assert!(cleanup.files.contains(&reference));
        assert!(!cleanup.files.contains(&new_blob));
        assert!(!cleanup.leases.contains_key(&new_blob));
        assert_eq!(report.skipped_in_use, 1);
    }

    #[tokio::test]
    async fn prune_reclaims_a_blob_that_concurrent_removals_each_kept() {
        #[cfg(unix)]
        if std::env::var_os("MSB_TEST_CONCURRENT_FLAT_REMOVALS").is_none() {
            run_isolated(
                "image::cleanup::tests::prune_reclaims_a_blob_that_concurrent_removals_each_kept",
                "MSB_TEST_CONCURRENT_FLAT_REMOVALS",
            );
            return;
        }

        let home = tempfile::tempdir().unwrap();
        let local = local(home.path()).await;
        let first_metadata = metadata('a', 'c');
        let second_metadata = metadata('b', 'd');
        let first_name = "example.com/a:latest";
        let second_name = "example.com/b:latest";
        let cache = install(&local, first_name, &first_metadata).await;
        install(&local, second_name, &second_metadata).await;
        let (first_ref, blob) = publish_flat(&cache, &first_metadata);
        let (second_ref, _) = publish_flat(&cache, &second_metadata);
        let first_catalog = CleanupCatalog::load(local.db().await.unwrap().read())
            .await
            .unwrap();
        let second_catalog = CleanupCatalog::load(local.db().await.unwrap().read())
            .await
            .unwrap();
        let manifest_id = |digest: &str| {
            first_catalog
                .manifests
                .iter()
                .find(|manifest| manifest.digest == digest)
                .unwrap()
                .id
        };
        let first_scope = HashSet::from([manifest_id(&first_metadata.manifest_digest)]);
        let second_scope = HashSet::from([manifest_id(&second_metadata.manifest_digest)]);

        // Each removal is prepared while the other's ref still names the shared blob.
        let first = PreparedCleanup::prepare(
            local.cache_dir(),
            cache.clone(),
            first_catalog,
            Some((first_name.into(), false)),
            first_scope.clone(),
            HashSet::new(),
            HashSet::new(),
        )
        .await
        .unwrap();
        let second = PreparedCleanup::prepare(
            local.cache_dir(),
            cache.clone(),
            second_catalog,
            Some((second_name.into(), false)),
            second_scope.clone(),
            HashSet::new(),
            HashSet::new(),
        )
        .await
        .unwrap();
        first
            .commit(
                &local,
                cache.clone(),
                Some((first_name.into(), false)),
                first_scope,
                HashSet::new(),
            )
            .await
            .unwrap();
        second
            .commit(
                &local,
                cache.clone(),
                Some((second_name.into(), false)),
                second_scope,
                HashSet::new(),
            )
            .await
            .unwrap();
        assert!(!first_ref.exists());
        assert!(!second_ref.exists());
        assert!(blob.exists());

        Image::prune_local(&local).await.unwrap();
        assert!(!blob.exists());
    }

    #[tokio::test]
    async fn prune_reclaims_unnamed_flat_blobs_but_not_one_being_published() {
        #[cfg(unix)]
        if std::env::var_os("MSB_TEST_UNNAMED_FLAT_BLOBS").is_none() {
            run_isolated(
                "image::cleanup::tests::prune_reclaims_unnamed_flat_blobs_but_not_one_being_published",
                "MSB_TEST_UNNAMED_FLAT_BLOBS",
            );
            return;
        }

        let home = tempfile::tempdir().unwrap();
        let local = local(home.path()).await;
        let cache = GlobalCache::new(&local.cache_dir()).unwrap();
        // A ref keeps the blob it names even without a catalog manifest.
        let (named_ref, named_blob) = publish_flat(&cache, &metadata('a', 'c'));
        let blob = |seed: &str| {
            let path =
                cache.flat_blob_path(&format!("sha256:{}", seed.repeat(64)).parse().unwrap());
            std::fs::write(&path, vec![3u8; 8192]).unwrap();
            path
        };
        let orphaned = blob("1");
        let publishing = blob("2");
        // A publisher leases a blob before the ref that names it exists.
        let publisher = cache.operation();
        publisher.lease_paths(vec![publishing.clone()]).unwrap();

        let report = Image::prune_local(&local).await.unwrap();
        assert!(!orphaned.exists());
        assert!(publishing.exists());
        assert_eq!(report.skipped_in_use, 1);
        assert!(named_ref.exists());
        assert!(named_blob.exists());

        drop(publisher);
        Image::prune_local(&local).await.unwrap();
        assert!(!publishing.exists());
        assert!(named_blob.exists());
    }
}
