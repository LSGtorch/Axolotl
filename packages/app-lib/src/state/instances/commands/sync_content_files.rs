use crate::State;
use crate::state::instances::adapters::{filesystem, sqlite};
use crate::state::instances::{Instance, InstanceFile};
use crate::state::{
    CacheBehaviour, CachedEntry, CachedFileUpdate, ContentProvider,
    ContentProviderRef, DirectoryInfo, ModrinthVersionId, ProjectType,
};
use crate::util::fetch::{self, FetchSemaphore};
use crate::util::io;
use chrono::Utc;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use uuid::Uuid;

/// Decides where this instance's game content (`mods`, `resourcepacks`,
/// `saves`, ...) physically lives, without touching the filesystem.
///
/// Order:
/// 1. directly associated instances resolve through the launcher dialect so
///    PCL version isolation (`versions/<id>` gameDir) is honored and HMCL /
///    generic installations point at their shared `.minecraft`
///    (`launcher::linked_game_dir`);
/// 2. a non-empty recorded linked `.minecraft` root is the fallback when the
///    dialect resolution cannot be completed;
/// 3. ordinary instances own their profile directory under Axolotl's
///    instances folder, honouring a per-instance `game_dir_override`
///    (`DirectoryInfo::instance_game_dir`).
///
/// This is the shared decision behind `instance_content_root` (canonicalized,
/// read side) and the install-write paths that must be able to create the
/// target directory, so it never canonicalizes and cannot fail.
pub(crate) fn content_game_dir(
    directories: &DirectoryInfo,
    instance: &Instance,
) -> PathBuf {
    crate::launcher::linked_game_dir(instance)
        .or_else(|| {
            instance
                .linked_dot_minecraft
                .as_deref()
                .map(str::trim)
                .filter(|linked| !linked.is_empty())
                .map(PathBuf::from)
        })
        .unwrap_or_else(|| directories.instance_game_dir(instance))
}

/// Resolves the directory whose game content (`mods`, `resourcepacks`, ...)
/// belongs to this instance.
///
/// Ordinary instances own their profile directory under Axolotl's instances
/// folder, honouring a per-instance `game_dir_override`
/// (`DirectoryInfo::instance_game_dir`). Directly associated instances have no
/// profile directory: their content lives inside the externally managed
/// installation, resolved through the launcher dialect so PCL version
/// isolation (`versions/<id>` gameDir) is honored; the shared linked
/// `.minecraft` root is the fallback when the dialect resolution cannot be
/// completed (see `launcher::linked_game_dir`). See also `content_game_dir`
/// for the non-canonicalizing decision.
pub(crate) fn instance_content_root(
    directories: &DirectoryInfo,
    instance: &Instance,
) -> crate::Result<PathBuf> {
    Ok(io::canonicalize(content_game_dir(directories, instance))?)
}

/// The path prefix embedded in hash-cache keys (`{size}-{prefix}/{relative}`)
/// for this instance's scanned content.
///
/// The hash-cache layer resolves key paths against Axolotl's own instances
/// folder (`state/cache.rs`). For directly associated instances the absolute
/// linked content root is used as the prefix: joining an absolute path
/// replaces the base, so the cache layer hashes the linked files instead of
/// failing on the nonexistent profile path. Managed instances keep the
/// relative profile path so the cache layer can resolve a
/// `game_dir_override` target from the database.
pub(crate) fn content_scan_cache_key_path(
    directories: &DirectoryInfo,
    instance: &Instance,
) -> crate::Result<String> {
    if instance.is_direct_linked() {
        Ok(instance_content_root(directories, instance)?
            .to_string_lossy()
            .into_owned())
    } else {
        Ok(instance.path.clone())
    }
}

pub(crate) async fn sync_content_files(
    instance_id: &str,
    state: &State,
) -> crate::Result<Vec<InstanceFile>> {
    let instance =
        sqlite::instance_rows::get_instance_by_id(instance_id, &state.pool)
            .await?
            .ok_or_else(|| {
                crate::ErrorKind::InputError("Unknown instance".to_string())
            })?;

    sync_instance_content_files(&instance, state).await
}

pub(crate) async fn sync_instance_content_files(
    instance: &Instance,
    state: &State,
) -> crate::Result<Vec<InstanceFile>> {
    // Keep the filesystem snapshot stable until its database rows commit.
    let _instance_lock = state.lock_instance_content(&instance.id).await;
    let content_root = instance_content_root(&state.directories, instance)?;
    let cache_key_path =
        content_scan_cache_key_path(&state.directories, instance)?;
    let scanned =
        filesystem::scan_content_files_from(&content_root, &cache_key_path)?;
    let scanned_paths = scanned
        .iter()
        .map(|file| file.relative_path.clone())
        .collect::<HashSet<_>>();
    let cache_keys = scanned
        .iter()
        .map(|file| file.hash_cache_key.as_str())
        .collect::<Vec<_>>();
    let hashes = CachedEntry::get_file_hash_many(
        &cache_keys,
        None,
        &state.pool,
        &state.api_semaphore,
    )
    .await?;
    let hashes_by_key = hashes
        .into_iter()
        .map(|hash| {
            (
                format!(
                    "{}-{}",
                    hash.size,
                    hash.path.trim_end_matches(".disabled")
                ),
                hash,
            )
        })
        .collect::<HashMap<_, _>>();
    let existing_files =
        sqlite::content_rows::get_instance_files(&instance.id, &state.pool)
            .await?;
    let mut existing_files_by_path = HashMap::new();
    let mut existing_files_by_sha1: HashMap<String, Vec<InstanceFile>> =
        HashMap::new();
    for file in existing_files {
        existing_files_by_sha1
            .entry(file.sha1.clone())
            .or_default()
            .push(file.clone());
        existing_files_by_path.insert(file.relative_path.clone(), file);
    }
    let content_set = sqlite::content_rows::get_applied_content_set(
        &instance.id,
        &state.pool,
    )
    .await?;
    let entry_file_ids = match content_set.as_ref() {
        Some(content_set) => sqlite::content_rows::get_content_entries(
            &content_set.id,
            &state.pool,
        )
        .await?
        .into_iter()
        .filter_map(|entry| entry.file_id)
        .collect(),
        None => HashSet::new(),
    };

    let now = Utc::now();
    let mut files: Vec<InstanceFile> = Vec::new();
    let mut reclaims: HashMap<String, String> = HashMap::new();
    let mut merges: HashMap<String, String> = HashMap::new();
    let mut claimed_reclaim_ids = HashSet::new();
    let mut externally_changed_file_ids = HashSet::new();

    for file in scanned {
        let hash_key = file.hash_cache_key.trim_end_matches(".disabled");
        let existing_file = existing_files_by_path.get(&file.relative_path);
        let (scanned_sha1, scanned_size) = if existing_file.is_some() {
            // A tracked row exists but its stored hash may be stale (the file
            // changed since it was installed), so re-hash the physical file.
            // It lives under `content_root` — for directly associated
            // instances that is the linked installation / PCL-isolated
            // version directory, never Axolotl's managed profile folder.
            let path = content_root.join(&file.relative_path);
            let (_, sha1) = fetch::sha1_file_async(&path).await?;
            (sha1, file.size)
        } else {
            let Some(hash) = hashes_by_key.get(hash_key) else {
                continue;
            };
            (hash.hash.clone(), hash.size)
        };
        let reclaim_candidate = if existing_file.is_some() {
            None
        } else {
            reclaimable_existing_file(
                &scanned_sha1,
                &file.relative_path,
                &existing_files_by_sha1,
                &scanned_paths,
                &claimed_reclaim_ids,
            )
        };
        let merge_candidate = existing_file
            .filter(|file| !entry_file_ids.contains(&file.id))
            .and_then(|file| {
                mergeable_tracked_file(
                    &scanned_sha1,
                    &file.relative_path,
                    &file.id,
                    &existing_files_by_sha1,
                    &entry_file_ids,
                    &scanned_paths,
                    &claimed_reclaim_ids,
                )
            });
        let source_file = existing_file.or(reclaim_candidate);
        if let Some(existing_file) = existing_file
            && physical_file_identity_changed(
                existing_file,
                &scanned_sha1,
                scanned_size,
            )
        {
            externally_changed_file_ids.insert(existing_file.id.clone());
        }
        if let Some(candidate) = reclaim_candidate {
            claimed_reclaim_ids.insert(candidate.id.clone());
            reclaims.insert(
                file.relative_path.clone(),
                candidate.relative_path.clone(),
            );
        }
        if let Some(candidate) = merge_candidate {
            claimed_reclaim_ids.insert(candidate.id.clone());
            merges.insert(file.relative_path.clone(), candidate.id.clone());
        }

        files.push(InstanceFile {
            id: source_file
                .map(|file| file.id.clone())
                .unwrap_or_else(instance_file_id),
            instance_id: instance.id.clone(),
            relative_path: file.relative_path,
            file_name: file.file_name,
            enabled: file.enabled,
            sha1: scanned_sha1,
            size: scanned_size,
            missing: false,
            added_at: source_file.map(|file| file.added_at).unwrap_or(now),
            modified_at: now,
            local_mod_data: source_file.and_then(|f| f.local_mod_data.clone()),
            icon_path: source_file.and_then(|f| f.icon_path.clone()),
        });
    }

    // Extract local mod metadata (Mod JARs) and cached icons (Mod JARs and
    // resource packs) for files that don't have them yet. This also backfills
    // rows created before these features existed; `icon_path` distinguishes
    // not-attempted (NULL), no-icon (empty string), and cached (path).
    // `content_root` already resolves the override / linked root consistently
    // for both managed and directly associated instances.
    let instance_dir = content_root;
    let icon_cache_dir = state.directories.caches_dir().join("icons");
    for file in &mut files {
        let Some(project_type) = project_type_for_file(file) else {
            continue;
        };
        // Re-extract metadata written before dependency extraction existed:
        // legacy JSON parses with `dependencies: None` and needs one pass
        // through the updated extractor.
        let extract_metadata = project_type == ProjectType::Mod
            && file
                .local_mod_data
                .as_ref()
                .and_then(|json| {
                    serde_json::from_str::<
                        crate::mod_metadata::LocalModMetadata,
                    >(json)
                    .ok()
                })
                .and_then(|metadata| metadata.dependencies)
                .is_none();
        let extract_icon = file.icon_path.is_none()
            && matches!(
                project_type,
                ProjectType::Mod | ProjectType::ResourcePack
            );
        if !extract_metadata && !extract_icon {
            continue;
        }

        let path = instance_dir.join(&file.relative_path);

        // Resource packs are read entry-wise so large archives are not
        // materialized in memory just to fetch `pack.png`.
        if extract_icon && project_type == ProjectType::ResourcePack {
            let icon =
                crate::mod_metadata::icon::extract_resource_pack_icon(&path);
            file.icon_path = Some(
                cache_extracted_icon(icon, &file.sha1, &icon_cache_dir, state)
                    .await,
            );
            continue;
        }

        // Mods: one in-memory read serves both metadata and icon extraction.
        let bytes = match tokio::fs::read(&path).await {
            Ok(data) => bytes::Bytes::from(data),
            Err(_) => {
                // File temporarily inaccessible; skip silently.
                continue;
            }
        };

        if extract_metadata
            && let Some(meta) =
                crate::mod_metadata::extract_mod_metadata(&bytes)
            && let Ok(json) = serde_json::to_string(&meta)
        {
            file.local_mod_data = Some(json);
        }

        if extract_icon {
            let meta = file.local_mod_data.as_ref().and_then(|json| {
                serde_json::from_str::<crate::mod_metadata::LocalModMetadata>(
                    json,
                )
                .ok()
            });
            let icon = crate::mod_metadata::icon::extract_mod_icon(
                &bytes,
                meta.as_ref(),
            );
            file.icon_path = Some(
                cache_extracted_icon(icon, &file.sha1, &icon_cache_dir, state)
                    .await,
            );
        }
    }

    let mut tx = state.pool.begin_with("BEGIN IMMEDIATE").await?;
    sqlite::content_rows::ensure_instance_exists(&instance.id, &mut tx).await?;
    sqlite::content_rows::mark_instance_files_missing(&instance.id, &mut tx)
        .await?;
    let mut invalidated_provider_identity = false;
    if let Some(content_set) = content_set.as_ref() {
        for file_id in &externally_changed_file_ids {
            invalidated_provider_identity |= sqlite::content_rows::invalidate_exact_provider_refs_for_file_in_transaction(
                &content_set.id,
                file_id,
                &mut tx,
            )
            .await?;
        }
    }

    // Upsert with a fresh id lookup inside the transaction. The ids assigned
    // during the scan may be stale if a concurrent operation (e.g. batch
    // disable renaming files to `.disabled`) moved a row after the snapshot;
    // reusing a stale id against the moved row would trip the UNIQUE
    // constraint on `instance_files.id` (code 1555).
    let mut synced_files: Vec<InstanceFile> = Vec::with_capacity(files.len());
    for file in &files {
        let synced =
            if let Some(tracked_file_id) = merges.get(&file.relative_path) {
                sqlite::content_rows::adopt_untracked_file_in_transaction(
                    &instance.id,
                    &file.relative_path,
                    tracked_file_id,
                    &mut tx,
                )
                .await?;
                upsert_scanned_file(&instance.id, file, &mut tx).await?
            } else if let Some(old_relative_path) =
                reclaims.get(&file.relative_path)
            {
                match sqlite::content_rows::move_instance_file_in_transaction(
                    &instance.id,
                    old_relative_path,
                    &file.relative_path,
                    &file.file_name,
                    file.enabled,
                    &file.sha1,
                    file.size,
                    file.local_mod_data.as_deref(),
                    file.icon_path.as_deref(),
                    &mut tx,
                )
                .await?
                {
                    Some(file) => file,
                    None => {
                        upsert_scanned_file(&instance.id, file, &mut tx).await?
                    }
                }
            } else {
                upsert_scanned_file(&instance.id, file, &mut tx).await?
            };
        synced_files.push(synced);
    }

    if invalidated_provider_identity
        && let Some(content_set) = content_set.as_ref()
    {
        sqlite::content_rows::bump_content_set_revision_in_transaction(
            &content_set.id,
            &mut tx,
        )
        .await?;
    }

    tx.commit().await?;

    Ok(synced_files)
}

async fn cache_extracted_icon(
    icon: Option<(String, Vec<u8>)>,
    sha1: &str,
    icon_cache_dir: &Path,
    state: &State,
) -> String {
    let Some((entry_name, icon_bytes)) = icon else {
        return String::new();
    };

    let extension = icon_extension(&entry_name);
    let cache_path = icon_cache_dir.join(format!("{sha1}.{extension}"));
    match fetch::write(&cache_path, &icon_bytes, &state.io_semaphore).await {
        Ok(()) => crate::util::io::canonicalize(&cache_path)
            .map(|path| path.to_string_lossy().into_owned())
            .unwrap_or_else(|_| cache_path.to_string_lossy().into_owned()),
        Err(_) => String::new(),
    }
}

fn icon_extension(entry_name: &str) -> &str {
    let extension = Path::new(entry_name)
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or_default();
    if matches!(
        extension.to_ascii_lowercase().as_str(),
        "png" | "jpg" | "jpeg"
    ) {
        extension
    } else {
        "png"
    }
}

pub(crate) fn project_type_for_file(
    file: &InstanceFile,
) -> Option<ProjectType> {
    filesystem::project_type_from_relative_path(&file.relative_path)
}

pub(crate) fn installed_modrinth_version_id(
    provider_refs: &[ContentProviderRef],
) -> Option<ModrinthVersionId> {
    provider_refs.iter().find_map(|reference| match reference {
        ContentProviderRef::Modrinth { version_id, .. } => {
            version_id.as_ref().cloned()
        }
        ContentProviderRef::CurseForge { .. }
        | ContentProviderRef::McArchive { .. } => None,
    })
}

pub(crate) async fn fetch_content_file_updates(
    update_key_refs: &[&str],
    cache_behaviour: Option<CacheBehaviour>,
    refresh: bool,
    pool: &sqlx::SqlitePool,
    fetch_semaphore: &FetchSemaphore,
) -> crate::Result<Vec<CachedFileUpdate>> {
    let update_behaviour = if refresh {
        Some(CacheBehaviour::Bypass)
    } else {
        cache_behaviour
    };

    match CachedEntry::get_file_update_many(
        update_key_refs,
        update_behaviour,
        pool,
        fetch_semaphore,
    )
    .await
    {
        Ok(updates) => Ok(updates),
        Err(error) if refresh => {
            tracing::warn!(
                "Content update refresh failed, using cached update data: {error}"
            );
            CachedEntry::get_file_update_many(
                update_key_refs,
                Some(CacheBehaviour::CacheOnly),
                pool,
                fetch_semaphore,
            )
            .await
        }
        Err(error) => Err(error),
    }
}

/// Whether a file may receive Modrinth update suggestions.
///
/// Files installed from Modrinth (origin `Modrinth`) always qualify. Untracked
/// or locally recorded files (no origin) qualify as long as no CurseForge
/// reference ties them to a different provider; CurseForge-origin files are
/// handled by the CurseForge update path instead.
pub(crate) fn modrinth_update_enabled(
    origin_provider: Option<ContentProvider>,
    provider_refs: &[ContentProviderRef],
) -> bool {
    match origin_provider {
        Some(ContentProvider::Modrinth) => true,
        Some(ContentProvider::CurseForge)
        | Some(ContentProvider::McArchive)
        | Some(ContentProvider::Local) => false,
        None => provider_refs.iter().all(|reference| {
            matches!(reference, ContentProviderRef::Modrinth { .. })
        }),
    }
}

fn instance_file_id() -> String {
    format!("instance-file:{}", Uuid::new_v4())
}

fn physical_file_identity_changed(
    existing: &InstanceFile,
    scanned_sha1: &str,
    scanned_size: u64,
) -> bool {
    existing.sha1 != scanned_sha1 || existing.size != scanned_size
}

async fn upsert_scanned_file(
    instance_id: &str,
    file: &InstanceFile,
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
) -> crate::Result<InstanceFile> {
    sqlite::content_rows::upsert_instance_file_from_parts_in_transaction(
        sqlite::content_rows::UpsertInstanceFile {
            instance_id,
            relative_path: &file.relative_path,
            file_name: &file.file_name,
            enabled: file.enabled,
            sha1: &file.sha1,
            size: file.size,
            missing: false,
            local_mod_data: file.local_mod_data.as_deref(),
            icon_path: file.icon_path.as_deref(),
        },
        tx,
    )
    .await
}

/// Finds the single missing file row that can safely inherit a scanned file's
/// installed identity. A rename should keep its provider refs, entry, and
/// install history; ambiguous duplicates and cross-type moves stay untracked.
fn reclaimable_existing_file<'a>(
    sha1: &str,
    new_relative_path: &str,
    existing_files_by_sha1: &'a HashMap<String, Vec<InstanceFile>>,
    scanned_paths: &HashSet<String>,
    claimed_ids: &HashSet<String>,
) -> Option<&'a InstanceFile> {
    let candidates = existing_files_by_sha1.get(sha1)?;
    if candidates
        .iter()
        .any(|file| scanned_paths.contains(file.relative_path.as_str()))
    {
        return None;
    }
    let missing = candidates
        .iter()
        .filter(|file| {
            file.missing || !scanned_paths.contains(&file.relative_path)
        })
        .collect::<Vec<_>>();
    if missing.len() != 1 || claimed_ids.contains(&missing[0].id) {
        return None;
    }
    let candidate = missing[0];
    let new_type =
        filesystem::project_type_from_relative_path(new_relative_path)?;
    let old_type =
        filesystem::project_type_from_relative_path(&candidate.relative_path)?;
    (new_type == old_type).then_some(candidate)
}

/// Finds the single tracked row that should hand its installed identity to an
/// untracked row at a new path. This repairs instances that were already
/// broken by a rename before reclaim-on-move existed.
fn mergeable_tracked_file<'a>(
    sha1: &str,
    new_relative_path: &str,
    untracked_file_id: &str,
    existing_files_by_sha1: &'a HashMap<String, Vec<InstanceFile>>,
    entry_file_ids: &HashSet<String>,
    scanned_paths: &HashSet<String>,
    claimed_ids: &HashSet<String>,
) -> Option<&'a InstanceFile> {
    let candidates = existing_files_by_sha1.get(sha1)?;
    let tracked = candidates
        .iter()
        .filter(|file| entry_file_ids.contains(&file.id))
        .collect::<Vec<_>>();
    if tracked.is_empty()
        || tracked.iter().any(|file| {
            file.id != untracked_file_id
                && scanned_paths.contains(file.relative_path.as_str())
        })
    {
        return None;
    }
    let missing = tracked
        .iter()
        .filter(|file| {
            file.id != untracked_file_id
                && (file.missing
                    || !scanned_paths.contains(file.relative_path.as_str()))
        })
        .collect::<Vec<_>>();
    if missing.len() != 1 || claimed_ids.contains(&missing[0].id) {
        return None;
    }
    let candidate = missing[0];
    let new_type =
        filesystem::project_type_from_relative_path(new_relative_path)?;
    let old_type =
        filesystem::project_type_from_relative_path(&candidate.relative_path)?;
    (new_type == old_type).then_some(candidate)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{
        CurseForgeFileId, CurseForgeProjectId, ModrinthProjectId,
        ModrinthVersionId,
    };
    use std::fs;
    use std::sync::Arc;

    fn modrinth_ref() -> ContentProviderRef {
        ContentProviderRef::Modrinth {
            project_id: ModrinthProjectId::new("project").unwrap(),
            version_id: Some(ModrinthVersionId::new("version").unwrap()),
        }
    }

    fn curseforge_ref() -> ContentProviderRef {
        ContentProviderRef::CurseForge {
            project_id: CurseForgeProjectId::new(42).unwrap(),
            file_id: Some(CurseForgeFileId::new(7).unwrap()),
        }
    }

    #[test]
    fn untracked_files_qualify_for_modrinth_updates() {
        assert!(modrinth_update_enabled(None, &[]));
        assert!(modrinth_update_enabled(None, &[modrinth_ref()]));
    }

    #[test]
    fn curseforge_tracked_files_do_not_qualify_for_modrinth_updates() {
        assert!(!modrinth_update_enabled(
            Some(ContentProvider::CurseForge),
            &[curseforge_ref()],
        ));
        assert!(!modrinth_update_enabled(None, &[curseforge_ref()]));
    }

    #[test]
    fn modrinth_origin_always_qualifies() {
        assert!(modrinth_update_enabled(
            Some(ContentProvider::Modrinth),
            &[curseforge_ref(), modrinth_ref()],
        ));
    }

    #[test]
    fn content_scan_preserves_install_temporary_files() {
        let root = tempfile::tempdir().unwrap();
        let mods = root.path().join("instance/mods");
        fs::create_dir_all(&mods).unwrap();
        let temporary_names = [
            "example.jar.installing.download",
            "example.jar.installing",
            "example.jar.installing.previous",
        ];
        for name in temporary_names {
            fs::write(mods.join(name), name).unwrap();
        }

        let scanned =
            filesystem::scan_content_files(root.path(), "instance").unwrap();

        assert!(scanned.is_empty());
        for name in temporary_names {
            assert!(mods.join(name).is_file());
        }
    }

    // -----------------------------------------------------------------------
    // Directly associated instances scan their linked installation
    // -----------------------------------------------------------------------

    /// The launcher state is a process-wide singleton; initialize it once and
    /// reuse it so `State::get()` (used by the hash cache) resolves inside
    /// these APIs. The state root is intentionally leaked (`.keep()`) because
    /// the shared state outlives this function.
    async fn global_state() -> Arc<State> {
        if !State::initialized() {
            let root = tempfile::TempDir::new().unwrap().keep();
            let _ =
                State::init_for_test(root.to_string_lossy().to_string()).await;
        }
        State::get().await.unwrap()
    }

    fn write_self_contained_version(minecraft: &Path, version_id: &str) {
        let version_dir = minecraft.join("versions").join(version_id);
        fs::create_dir_all(&version_dir).unwrap();
        fs::write(
            version_dir.join(format!("{version_id}.json")),
            serde_json::to_vec(&serde_json::json!({
                "id": version_id,
                "mainClass": "net.minecraft.client.main.Main"
            }))
            .unwrap(),
        )
        .unwrap();
    }

    #[tokio::test]
    async fn direct_link_refresh_scans_linked_dot_minecraft_content() {
        let state = global_state().await;
        let minecraft = tempfile::TempDir::new().unwrap();
        write_self_contained_version(minecraft.path(), "1.12.2-linked");
        fs::create_dir_all(minecraft.path().join("mods")).unwrap();
        fs::write(
            minecraft.path().join("mods/linked-mod.jar"),
            b"linked mod bytes",
        )
        .unwrap();

        let instance = crate::state::create_direct_link_instance(
            crate::state::CreateDirectLinkInstance {
                name: None,
                launcher_type:
                    crate::api::pack::import::ImportLauncherType::Generic,
                base_path: minecraft.path().to_path_buf(),
                instance_folder: "versions/1.12.2-linked".to_string(),
                instance_path: None,
            },
            &state,
        )
        .await
        .unwrap();

        let files = sync_instance_content_files(&instance, &state)
            .await
            .unwrap();

        assert!(
            files
                .iter()
                .any(|file| file.relative_path == "mods/linked-mod.jar"),
            "refresh must discover mods inside the linked `.minecraft`, got: {:?}",
            files
                .iter()
                .map(|file| &file.relative_path)
                .collect::<Vec<_>>(),
        );
    }

    #[tokio::test]
    async fn pcl_isolated_direct_link_scans_version_isolated_content() {
        let state = global_state().await;
        let minecraft = tempfile::TempDir::new().unwrap();
        write_self_contained_version(minecraft.path(), "1.12.2-pcl");
        // Version isolation on: PCL resolves this version's gameDir to
        // versions/<id>, so its mods folder lives beside the version JSON.
        let version_dir = minecraft.path().join("versions/1.12.2-pcl");
        fs::create_dir_all(version_dir.join("PCL")).unwrap();
        fs::write(
            version_dir.join("PCL/Setup.ini"),
            "VersionArgumentIndieV2: true\n",
        )
        .unwrap();
        fs::create_dir_all(version_dir.join("mods")).unwrap();
        fs::write(
            version_dir.join("mods/isolated-mod.jar"),
            b"isolated mod bytes",
        )
        .unwrap();

        let instance = crate::state::create_direct_link_instance(
            crate::state::CreateDirectLinkInstance {
                name: None,
                launcher_type:
                    crate::api::pack::import::ImportLauncherType::PCL2,
                base_path: minecraft.path().to_path_buf(),
                instance_folder: "versions/1.12.2-pcl".to_string(),
                instance_path: None,
            },
            &state,
        )
        .await
        .unwrap();

        let files = sync_instance_content_files(&instance, &state)
            .await
            .unwrap();

        assert!(
            files
                .iter()
                .any(|file| file.relative_path == "mods/isolated-mod.jar"),
            "refresh must resolve the PCL-isolated gameDir, got: {:?}",
            files
                .iter()
                .map(|file| &file.relative_path)
                .collect::<Vec<_>>(),
        );
    }

    #[tokio::test]
    async fn ordinary_instance_refresh_still_scans_its_profile_directory() {
        let state = global_state().await;
        let metadata = crate::api::instance::create(
            format!("sync-normal {}", uuid::Uuid::new_v4()),
            "1.20.1".to_string(),
            crate::state::ModLoader::Vanilla,
            None,
            None,
            crate::state::InstanceLink::Unmanaged,
            None,
            None,
        )
        .await
        .unwrap();

        let mods = state
            .directories
            .instances_dir()
            .join(&metadata.instance.path)
            .join("mods");
        fs::create_dir_all(&mods).unwrap();
        fs::write(mods.join("profile-mod.jar"), b"profile mod bytes").unwrap();

        let files = sync_instance_content_files(&metadata.instance, &state)
            .await
            .unwrap();

        assert!(
            files
                .iter()
                .any(|file| file.relative_path == "mods/profile-mod.jar"),
            "ordinary instances must keep scanning their profile directory, \
             got: {:?}",
            files
                .iter()
                .map(|file| &file.relative_path)
                .collect::<Vec<_>>(),
        );
    }

    // -----------------------------------------------------------------------
    // Discover-install writes land in the real game directory
    // -----------------------------------------------------------------------

    /// Applies a locally-staged download to the instance, mirroring what the
    /// Discover page does for Modrinth installs, without any network access.
    async fn apply_local_project_download(
        instance_id: &str,
        relative_path: &str,
        bytes: &[u8],
        state: &State,
    ) {
        let source_dir = tempfile::TempDir::new().unwrap();
        let source = source_dir.path().join("staged-download");
        fs::write(&source, bytes).unwrap();
        let sha1 = crate::util::fetch::sha1_async(
            bytes::Bytes::copy_from_slice(bytes),
        )
        .await
        .unwrap();
        crate::state::instances::commands::apply_downloaded_project_version_at_path(
            instance_id,
            relative_path,
            crate::state::instances::commands::DownloadedProjectVersion {
                file_name: Path::new(relative_path)
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned(),
                path: source,
                sha1,
                size: bytes.len() as u64,
                project_type: ProjectType::Mod,
                project_id: "discover-project".to_string(),
                version_id: "discover-version".to_string(),
            },
            crate::state::instances::ContentSourceKind::Local,
            crate::state::instances::ContentOwnershipKind::UserAdded,
            state,
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn hmcl_generic_direct_link_install_writes_into_linked_minecraft() {
        let state = global_state().await;
        let minecraft = tempfile::TempDir::new().unwrap();
        write_self_contained_version(minecraft.path(), "1.12.2-install");

        let instance = crate::state::create_direct_link_instance(
            crate::state::CreateDirectLinkInstance {
                name: None,
                launcher_type:
                    crate::api::pack::import::ImportLauncherType::Generic,
                base_path: minecraft.path().to_path_buf(),
                instance_folder: "versions/1.12.2-install".to_string(),
                instance_path: None,
            },
            &state,
        )
        .await
        .unwrap();

        let bytes = b"discover-installed mod bytes";
        apply_local_project_download(
            &instance.id,
            "mods/discover-installed-mod.jar",
            bytes,
            &state,
        )
        .await;

        let installed =
            minecraft.path().join("mods/discover-installed-mod.jar");
        assert!(
            installed.is_file(),
            "Discover install must write into the linked `.minecraft`, got \
             missing file at {}",
            installed.display()
        );
        assert_eq!(fs::read(&installed).unwrap(), bytes);
        assert!(
            !state
                .directories
                .instances_dir()
                .join(&instance.path)
                .exists(),
            "installing through Discover must never create the managed \
             profile folder for a direct-link instance"
        );
    }

    #[tokio::test]
    async fn pcl_direct_link_install_writes_into_version_isolated_directory() {
        let state = global_state().await;
        let minecraft = tempfile::TempDir::new().unwrap();
        let version_id = "1.12.2-pcl-install";
        write_self_contained_version(minecraft.path(), version_id);
        // Version isolation on: PCL resolves this version's gameDir to
        // versions/<id>, so Discover installs must land beside the version
        // JSON, never in the shared `.minecraft` root.
        let version_dir = minecraft.path().join("versions").join(version_id);
        fs::create_dir_all(version_dir.join("PCL")).unwrap();
        fs::write(
            version_dir.join("PCL/Setup.ini"),
            "VersionArgumentIndieV2: true\n",
        )
        .unwrap();

        let instance = crate::state::create_direct_link_instance(
            crate::state::CreateDirectLinkInstance {
                name: None,
                launcher_type:
                    crate::api::pack::import::ImportLauncherType::PCL2,
                base_path: minecraft.path().to_path_buf(),
                instance_folder: format!("versions/{version_id}"),
                instance_path: None,
            },
            &state,
        )
        .await
        .unwrap();

        let bytes = b"pcl isolated discover mod bytes";
        apply_local_project_download(
            &instance.id,
            "mods/discover-isolated-mod.jar",
            bytes,
            &state,
        )
        .await;

        let installed = version_dir.join("mods/discover-isolated-mod.jar");
        assert!(
            installed.is_file(),
            "Discover install must write into the PCL-isolated version \
             directory, got missing file at {}",
            installed.display()
        );
        assert_eq!(fs::read(&installed).unwrap(), bytes);
        assert!(
            !minecraft
                .path()
                .join("mods/discover-isolated-mod.jar")
                .exists(),
            "a PCL-isolated direct link must not write into the shared \
             `.minecraft` root"
        );
    }

    #[tokio::test]
    async fn direct_link_second_refresh_hashes_linked_content_not_managed_profile()
     {
        let state = global_state().await;
        let minecraft = tempfile::TempDir::new().unwrap();
        write_self_contained_version(minecraft.path(), "1.12.2-sync-twice");
        fs::create_dir_all(minecraft.path().join("mods")).unwrap();
        fs::write(
            minecraft.path().join("mods/linked-mod.jar"),
            b"linked mod bytes",
        )
        .unwrap();

        let instance = crate::state::create_direct_link_instance(
            crate::state::CreateDirectLinkInstance {
                name: None,
                launcher_type:
                    crate::api::pack::import::ImportLauncherType::Generic,
                base_path: minecraft.path().to_path_buf(),
                instance_folder: "versions/1.12.2-sync-twice".to_string(),
                instance_path: None,
            },
            &state,
        )
        .await
        .unwrap();

        // First refresh indexes the linked file; the second refresh re-hashes
        // the already-tracked file from disk. That physical re-hash must read
        // from the linked `.minecraft`, not from the (nonexistent) managed
        // profile folder, or the refresh fails with a missing-file error.
        let first = sync_instance_content_files(&instance, &state)
            .await
            .unwrap();
        assert!(
            first
                .iter()
                .any(|file| file.relative_path == "mods/linked-mod.jar")
        );

        let second = sync_instance_content_files(&instance, &state)
            .await
            .unwrap();
        let row = second
            .iter()
            .find(|file| file.relative_path == "mods/linked-mod.jar")
            .expect("second refresh must still report the linked mod");
        assert!(
            !row.missing,
            "second refresh must not mark the linked mod missing"
        );
        let (_, physical_sha1) = crate::util::fetch::sha1_file_async(
            &minecraft.path().join("mods/linked-mod.jar"),
        )
        .await
        .unwrap();
        assert_eq!(
            row.sha1, physical_sha1,
            "second refresh must hash the file inside the linked `.minecraft`"
        );
    }

    /// Regression probe: an instance whose `game_dir_override` points to an
    /// external `.minecraft` root must have its content scanned from that root,
    /// not from the (empty) managed instance folder. This is the split-brain
    /// the content page empty-state used to hit.
    #[cfg(not(feature = "tauri"))]
    #[tokio::test]
    async fn content_scan_uses_game_dir_override() {
        crate::event::EventState::init().await.unwrap();
        let root = tempfile::tempdir().unwrap().keep();
        let state =
            crate::State::init_for_test(root.to_string_lossy().to_string())
                .await
                .unwrap();

        // Create an external .minecraft root with a mod, outside the managed
        // profiles dir.
        let mc_root = tempfile::tempdir().unwrap();
        let mods_dir = mc_root.path().join("mods");
        fs::create_dir_all(&mods_dir).unwrap();
        fs::write(mods_dir.join("my-mod.jar"), "mod").unwrap();

        let created = crate::api::instance::create(
            "Override Instance".to_string(),
            "1.20.1".to_string(),
            crate::state::ModLoader::Vanilla,
            None,
            None,
            crate::state::InstanceLink::Unmanaged,
            None,
            Some(mc_root.path().to_string_lossy().to_string()),
        )
        .await
        .unwrap();
        crate::state::instances::commands::set_instance_install_stage(
            &created.instance.id,
            crate::state::InstanceInstallStage::Installed,
            &state.pool,
        )
        .await
        .unwrap();

        let instance =
            crate::state::instances::adapters::sqlite::instance_rows::get_instance_by_id(
                &created.instance.id,
                &state.pool,
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            state.directories.instance_game_dir(&instance),
            mc_root.path(),
            "instance_game_dir must resolve to the override root"
        );
        let direct =
            filesystem::scan_content_files_from(mc_root.path(), &instance.path)
                .unwrap();
        assert_eq!(
            direct.len(),
            1,
            "direct scan of override root should find the mod"
        );
        let files = sync_instance_content_files(&instance, &state)
            .await
            .unwrap();

        assert_eq!(
            files.len(),
            1,
            "expected the override-root mod to be scanned"
        );
        assert!(files[0].relative_path.ends_with("mods/my-mod.jar"));
    }

    #[test]
    fn external_hash_or_size_change_invalidates_physical_identity() {
        let now = Utc::now();
        let file = InstanceFile {
            id: "file".to_string(),
            instance_id: "instance".to_string(),
            relative_path: "mods/lithium.jar".to_string(),
            file_name: "lithium.jar".to_string(),
            enabled: true,
            sha1: "official".to_string(),
            size: 10,
            missing: false,
            added_at: now,
            modified_at: now,
            local_mod_data: None,
            icon_path: None,
        };

        assert!(!physical_file_identity_changed(&file, "official", 10));
        assert!(physical_file_identity_changed(&file, "external", 10));
        assert!(physical_file_identity_changed(&file, "official", 11));
    }
}
