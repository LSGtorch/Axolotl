use crate::State;
use crate::event::InstancePayloadType;
use crate::event::emit::{emit_instance, emit_minecraft_crash_warning};
use crate::state::{
    DirectoryInfo, InstanceInstallStage, ProjectType, attached_world_data,
};
use crate::worlds::WorldType;
use notify::{RecommendedWatcher, RecursiveMode};
use notify_debouncer_mini::{DebounceEventResult, Debouncer, new_debouncer};
use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::sync::{RwLock, mpsc::channel};

use super::adapters::sqlite::instance_rows;
use super::config_sync::{CONFIG_FILE_NAME, CONFIG_FILE_TEMP_NAME};

pub struct FileWatcher {
    watcher: RwLock<Debouncer<RecommendedWatcher>>,
    instance_ids: Arc<RwLock<HashMap<String, String>>>,
    /// External content roots of directly associated instances (the linked
    /// HMCL/PCL game directory), mapped to every instance id sharing that
    /// root. One `.minecraft` may back several instances; an event under the
    /// root notifies all of them.
    external_root_instances: Arc<RwLock<HashMap<String, HashSet<String>>>>,
    content_changes: Arc<RwLock<HashMap<String, InstanceContentChangeState>>>,
    manual_import_directory: Arc<RwLock<Option<PathBuf>>>,
    manual_import_generation: Arc<AtomicU64>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct InstanceContentWatchSnapshot {
    pub epoch: u64,
    pub generation: u64,
    pub dirty_paths: HashSet<String>,
    pub directory_dirty: bool,
}

#[derive(Default)]
struct InstanceContentChangeState {
    epoch: u64,
    generation: u64,
    dirty_paths: HashSet<String>,
    directory_dirty: bool,
    tracked_paths: HashSet<String>,
}

static NEXT_CONTENT_EPOCH: AtomicU64 = AtomicU64::new(1);

pub async fn init_watcher() -> crate::Result<FileWatcher> {
    let (tx, mut rx) = channel(1);
    let instance_ids = Arc::new(RwLock::new(HashMap::<String, String>::new()));
    let event_instance_ids = instance_ids.clone();
    let external_root_instances =
        Arc::new(RwLock::new(HashMap::<String, HashSet<String>>::new()));
    let event_external_root_instances = external_root_instances.clone();
    let content_changes = Arc::new(RwLock::new(HashMap::<
        String,
        InstanceContentChangeState,
    >::new()));
    let event_content_changes = content_changes.clone();
    let manual_import_directory = Arc::new(RwLock::new(None::<PathBuf>));
    let event_manual_import_directory = manual_import_directory.clone();
    let manual_import_generation = Arc::new(AtomicU64::new(0));

    let file_watcher = new_debouncer(
        Duration::from_secs_f32(1.0),
        move |res: DebounceEventResult| {
            tx.blocking_send(res).ok();
        },
    )?;

    tokio::task::spawn(async move {
        let span = tracing::span!(tracing::Level::INFO, "init_watcher");
        tracing::info!(parent: &span, "Initing watcher");
        while let Some(res) = rx.recv().await {
            let _span = span.enter();

            match res {
                Ok(events) => {
                    let instance_ids = event_instance_ids.read().await;
                    let external_root_instances =
                        event_external_root_instances.read().await;
                    let manual_import_directory =
                        event_manual_import_directory.read().await.clone();
                    let mut visited_instances = Vec::new();
                    let mut scan_manual_downloads = false;

                    for e in &events {
                        let mut instance_path = None;

                        let mut found = false;
                        for component in e.path.components() {
                            if found {
                                instance_path = Some(component.as_os_str());
                                break;
                            }

                            if component.as_os_str()
                                == crate::state::dirs::INSTANCES_FOLDER_NAME
                            {
                                found = true;
                            }
                        }

                        let mut handled = false;
                        if let Some(instance_path) = instance_path {
                            let instance_path_str =
                                instance_path.to_string_lossy().to_string();
                            if let Some(instance_id) =
                                instance_ids.get(&instance_path_str).cloned()
                            {
                                let relative_path = e
                                    .path
                                    .components()
                                    .skip_while(|x| {
                                        x.as_os_str() != instance_path
                                    })
                                    .skip(1)
                                    .map(|component| {
                                        component.as_os_str().to_string_lossy()
                                    })
                                    .collect::<Vec<_>>()
                                    .join("/");
                                process_instance_event(
                                    &instance_id,
                                    &relative_path,
                                    &e.path,
                                    &event_content_changes,
                                    &mut visited_instances,
                                )
                                .await;
                                handled = true;
                            }
                        }

                        // Events from directly associated instances arrive
                        // under their external linked root, a path that never
                        // contains the managed instances-folder component;
                        // attribute them through the registered roots, which
                        // may cover several instances sharing one `.minecraft`.
                        if !handled {
                            let targets = external_root_event_targets(
                                &e.path,
                                &external_root_instances,
                            );
                            if !targets.is_empty() {
                                for (instance_id, relative_path) in targets {
                                    process_instance_event(
                                        instance_id,
                                        &relative_path,
                                        &e.path,
                                        &event_content_changes,
                                        &mut visited_instances,
                                    )
                                    .await;
                                }
                                handled = true;
                            }
                        }

                        if !handled
                            && manual_import_directory.as_ref().is_some_and(
                                |directory| e.path.starts_with(directory),
                            )
                        {
                            scan_manual_downloads = true;
                        }
                    }
                    if scan_manual_downloads
                        && let Some(directory) = manual_import_directory
                    {
                        tokio::spawn(async move {
                            if let Err(error) =
                                crate::api::curseforge::scan_pending_manual_downloads_in(
                                    &directory,
                                )
                                .await
                            {
                                tracing::warn!(
                                    "Unable to scan pending manual downloads: {error}"
                                );
                            }
                        });
                    }
                }
                Err(error) => tracing::warn!("Unable to watch file: {error}"),
            }
        }
    });

    Ok(FileWatcher {
        watcher: RwLock::new(file_watcher),
        instance_ids,
        external_root_instances,
        content_changes,
        manual_import_directory,
        manual_import_generation,
    })
}

impl FileWatcher {
    pub(crate) async fn track_upgrade_source(
        &self,
        instance_id: &str,
        paths: impl IntoIterator<Item = String>,
    ) -> Option<InstanceContentWatchSnapshot> {
        let mut changes = self.content_changes.write().await;
        let change = changes.get_mut(instance_id)?;
        change.tracked_paths.extend(paths);
        Some(change.snapshot())
    }

    pub(crate) async fn content_watch_snapshot(
        &self,
        instance_id: &str,
    ) -> Option<InstanceContentWatchSnapshot> {
        self.content_changes
            .read()
            .await
            .get(instance_id)
            .map(InstanceContentChangeState::snapshot)
    }

    #[cfg(test)]
    pub(crate) async fn record_upgrade_content_change(
        &self,
        instance_id: &str,
        relative_path: &str,
    ) {
        record_upgrade_content_change(
            &self.content_changes,
            instance_id,
            relative_path,
        )
        .await;
    }

    pub(crate) async fn configure_manual_import_directory(
        &self,
        directory: Option<PathBuf>,
    ) -> crate::Result<()> {
        let current = self.manual_import_directory.read().await.clone();
        if current == directory {
            return Ok(());
        }

        let mut debouncer = self.watcher.write().await;
        if let Some(directory) = directory.as_ref() {
            debouncer
                .watcher()
                .watch(directory, RecursiveMode::NonRecursive)?;
        }
        if let Some(current) = current.as_ref() {
            let _ = debouncer.watcher().unwatch(current);
        }
        *self.manual_import_directory.write().await = directory;
        let generation = self
            .manual_import_generation
            .fetch_add(1, Ordering::Relaxed)
            + 1;
        let Some(directory) = self.manual_import_directory.read().await.clone()
        else {
            return Ok(());
        };
        let active_generation = self.manual_import_generation.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(3));
            interval.set_missed_tick_behavior(
                tokio::time::MissedTickBehavior::Skip,
            );
            loop {
                interval.tick().await;
                if active_generation.load(Ordering::Relaxed) != generation {
                    break;
                }
                if let Err(error) =
                    crate::api::curseforge::scan_pending_manual_downloads_in(
                        &directory,
                    )
                    .await
                {
                    tracing::warn!(
                        "Unable to poll pending manual downloads: {error}"
                    );
                }
            }
        });
        Ok(())
    }
}

pub(crate) async fn watch_instances_init(
    watcher: &FileWatcher,
    dirs: &DirectoryInfo,
    pool: &sqlx::SqlitePool,
) {
    let Ok(instances) = instance_rows::list_instances(pool).await else {
        return;
    };

    for instance in instances {
        // Directly associated instances have no profile directory: their
        // content lives inside the external linked installation, so watch
        // that actual game directory (and never create folders inside it).
        let content_root = super::content_game_dir(dirs, &instance);
        watch_instance_folder(
            &instance.id,
            &instance.path,
            &content_root,
            watcher,
            !instance.is_direct_linked(),
        )
        .await;
    }
}

/// Registers one instance's content root with the file watcher.
///
/// `full_instance_path` is the directory the instance actually runs from: the
/// managed profile folder for ordinary instances, the external linked HMCL/PCL
/// game directory for directly associated instances. When `create_missing_dirs`
/// is true (ordinary instances), missing content subfolders (`mods`, `saves`,
/// ...) are created before watching. Directly associated instances never
/// create anything inside their external launcher directory — only existing
/// paths are watched — and their root is additionally registered in the
/// external-root map so events under it are attributed back to every instance
/// sharing the root.
pub(crate) async fn watch_instance_folder(
    instance_id: &str,
    instance_path: &str,
    full_instance_path: &Path,
    watcher: &FileWatcher,
    create_missing_dirs: bool,
) {
    let Ok(metadata) = tokio::fs::metadata(full_instance_path).await else {
        return;
    };

    if !metadata.is_dir() {
        return;
    }

    let mut to_watch = Vec::new();
    for full_path in instance_watch_paths(full_instance_path) {
        if &full_path == full_instance_path {
            // The root is watched non-recursively after the subfolders.
            continue;
        }
        let meta = tokio::fs::symlink_metadata(&full_path).await;
        let exists = meta.is_ok();
        let is_symlink = meta.ok().is_some_and(|m| m.file_type().is_symlink());
        let sub_path = full_path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();

        if create_missing_dirs
            && !exists
            && !is_symlink
            && !sub_path.contains('.')
        {
            if let Err(e) = crate::util::io::create_dir_all(&full_path).await {
                tracing::error!(
                    "Failed to create directory for watcher {full_path:?}: {e}"
                );
                return;
            }
        }

        // Only watch directories that exist after the (optional) creation:
        // `notify` cannot watch a missing path, and directly associated
        // instances must never create content folders inside their external
        // linked root, so their missing subfolders are simply skipped.
        if tokio::fs::metadata(&full_path)
            .await
            .is_ok_and(|meta| meta.is_dir())
        {
            to_watch.push(full_path);
        }
    }

    let mut debouncer = watcher.watcher.write().await;
    for full_path in &to_watch {
        if let Err(e) = debouncer
            .watcher()
            .watch(full_path, RecursiveMode::Recursive)
        {
            tracing::error!(
                "Failed to watch directory for watcher {full_path:?}: {e}"
            );
            return;
        }
    }

    if let Err(e) = debouncer
        .watcher()
        .watch(full_instance_path, RecursiveMode::NonRecursive)
    {
        tracing::error!(
            "Failed to watch root instance directory for watcher {full_instance_path:?}: {e}"
        );
    }

    watcher
        .instance_ids
        .write()
        .await
        .insert(instance_path.to_string(), instance_id.to_string());
    watcher
        .content_changes
        .write()
        .await
        .insert(instance_id.to_string(), new_instance_content_change_state());
    if !create_missing_dirs {
        watcher
            .external_root_instances
            .write()
            .await
            .entry(full_instance_path.to_string_lossy().into_owned())
            .or_default()
            .insert(instance_id.to_string());
    }
}

/// Stops watching an instance folder and forgets its instance-id mapping.
///
/// Used when the instance folder is about to be renamed or replaced. On
/// Windows an active watch keeps an open directory handle, which blocks
/// renaming the folder with `ERROR_ACCESS_DENIED`; the folder must be
/// unwatched first and re-registered afterwards.
///
/// Directly associated instances watch their external linked HMCL/PCL game
/// directory, which may back several instances: the filesystem watch on that
/// root is released only when the removed instance was the last one
/// registered under it, so deleting one of several instances sharing a
/// `.minecraft` keeps delivering events to the remaining instances. Managed
/// instances own their profile folder exclusively and are always unwatched.
pub(crate) async fn unwatch_instance_folder(
    instance_path: &str,
    full_instance_path: &Path,
    watcher: &FileWatcher,
) {
    let instance_id = watcher.instance_ids.write().await.remove(instance_path);

    // Release the filesystem watch unless other directly associated
    // instances still share this external root.
    let mut release_watch = true;
    if let Some(instance_id) = &instance_id {
        let mut external_roots = watcher.external_root_instances.write().await;
        release_watch = deregister_external_root(
            &mut external_roots,
            &full_instance_path.to_string_lossy(),
            instance_id,
        );
    }

    if release_watch {
        let mut debouncer = watcher.watcher.write().await;
        for full_path in instance_watch_paths(full_instance_path) {
            let _ = debouncer.watcher().unwatch(&full_path);
        }
    }

    if let Some(instance_id) = instance_id {
        watcher.content_changes.write().await.remove(&instance_id);
    }
}

/// Removes `instance_id` from the instances registered under the external
/// root keyed by `root_key` and reports whether the filesystem watch on that
/// root has to be released.
///
/// Directly associated instances share one linked `.minecraft`: the watch
/// stays alive while any other instance still backs the root, and the root
/// entry is dropped only when the removed instance was the last one. Returns
/// true when the root was not registered at all (e.g. it did not exist when
/// the instance was watched), so callers release the paths unconditionally in
/// that case.
fn deregister_external_root(
    external_roots: &mut HashMap<String, HashSet<String>>,
    root_key: &str,
    instance_id: &str,
) -> bool {
    let Some(sharing) = external_roots.get_mut(root_key) else {
        return true;
    };
    sharing.remove(instance_id);
    if sharing.is_empty() {
        external_roots.remove(root_key);
        true
    } else {
        false
    }
}

impl InstanceContentChangeState {
    fn snapshot(&self) -> InstanceContentWatchSnapshot {
        InstanceContentWatchSnapshot {
            epoch: self.epoch,
            generation: self.generation,
            dirty_paths: self.dirty_paths.clone(),
            directory_dirty: self.directory_dirty,
        }
    }
}

fn new_instance_content_change_state() -> InstanceContentChangeState {
    InstanceContentChangeState {
        epoch: NEXT_CONTENT_EPOCH.fetch_add(1, Ordering::Relaxed),
        ..InstanceContentChangeState::default()
    }
}

fn is_upgrade_content_change(
    relative_path: &str,
    tracked_paths: &HashSet<String>,
) -> bool {
    let normalized = relative_path.replace('\\', "/");
    let top_level = normalized.split('/').next().unwrap_or_default();
    matches!(
        top_level,
        "mods" | "resourcepacks" | "shaderpacks" | "datapacks"
    ) || tracked_paths.contains(&normalized)
}

async fn record_upgrade_content_change(
    content_changes: &RwLock<HashMap<String, InstanceContentChangeState>>,
    instance_id: &str,
    relative_path: &str,
) {
    let mut content_changes = content_changes.write().await;
    if let Some(change) = content_changes.get_mut(instance_id)
        && is_upgrade_content_change(relative_path, &change.tracked_paths)
    {
        change.generation = change.generation.wrapping_add(1);
        change.dirty_paths.insert(relative_path.replace('\\', "/"));
        change.directory_dirty = true;
    }
}

/// Maps an event path to every directly associated instance whose external
/// content root contains it, paired with the path relative to that root. One
/// linked root (e.g. a single HMCL `.minecraft`) may back several instances,
/// so each of them receives the event. Returns nothing when the event lies
/// outside every registered root. Pure and synchronous so event attribution
/// can be unit-tested without a live `notify` watcher.
fn external_root_event_targets<'a>(
    event_path: &Path,
    external_roots: &'a HashMap<String, HashSet<String>>,
) -> Vec<(&'a str, String)> {
    let mut targets = Vec::new();
    for (root, instance_ids) in external_roots {
        let Ok(relative_path) = event_path.strip_prefix(Path::new(root)) else {
            continue;
        };
        let relative_path = relative_path
            .components()
            .map(|component| component.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("/");
        for instance_id in instance_ids {
            targets.push((instance_id.as_str(), relative_path.clone()));
        }
    }
    targets
}

/// Runs the per-instance watcher pipeline for one event already attributed to
/// an instance through its content root (managed profile path or external
/// linked root): records upgrade content changes, detects crash reports, and
/// emits frontend events. `visited_instances` deduplicates `Synced`-style
/// emissions within one debounced batch; crash warnings are always emitted.
async fn process_instance_event(
    instance_id: &str,
    relative_path: &str,
    event_path: &Path,
    content_changes: &RwLock<HashMap<String, InstanceContentChangeState>>,
    visited_instances: &mut Vec<String>,
) {
    if !relative_path.is_empty() {
        record_upgrade_content_change(
            content_changes,
            instance_id,
            relative_path,
        )
        .await;
    }
    let first_file_name = relative_path.split('/').next().unwrap_or("");
    if is_config_sync_file_name(std::ffi::OsStr::new(first_file_name)) {
        return;
    }
    let relative = Path::new(relative_path);
    let is_crash_report = first_file_name == "crash-reports"
        && relative
            .extension()
            .is_some_and(|extension| extension == "txt");
    let is_jvm_crash = first_file_name.starts_with("hs_err_pid")
        && relative
            .extension()
            .is_some_and(|extension| extension == "log");
    if is_crash_report || is_jvm_crash {
        crash_task(instance_id.to_string());
        return;
    }
    if visited_instances
        .iter()
        .any(|visited| visited == instance_id)
    {
        return;
    }
    let event = if first_file_name == "servers.dat" {
        Some(InstancePayloadType::ServersUpdated)
    } else if first_file_name == "saves"
        && relative.file_name().is_some_and(|name| name == "level.dat")
    {
        tracing::info!("World updated: {}", event_path.display());
        let world = relative
            .parent()
            .and_then(Path::file_name)
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_default();
        if !event_path.is_file() {
            let instance_id = instance_id.to_string();
            let world = world.clone();
            tokio::spawn(async move {
                if let Ok(state) = State::get().await
                    && let Err(error) =
                        attached_world_data::AttachedWorldData::remove_for_world(
                            &instance_id,
                            WorldType::Singleplayer,
                            &world,
                            &state.pool,
                        )
                        .await
                {
                    tracing::warn!(
                        "Failed to remove AttachedWorldData for '{world}': {error}"
                    )
                }
            });
        }
        Some(InstancePayloadType::WorldUpdated { world })
    } else if first_file_name != "saves" {
        Some(InstancePayloadType::Synced)
    } else {
        None
    };
    if let Some(event) = event {
        let emit_instance_id = instance_id.to_string();
        tokio::spawn(async move {
            let _ = emit_instance(&emit_instance_id, event).await;
        });
        visited_instances.push(instance_id.to_string());
    }
}

/// All paths `watch_instance_folder` registers for a single instance,
/// including the root, so `unwatch_instance_folder` can release them again.
fn instance_watch_paths(full_instance_path: &Path) -> Vec<PathBuf> {
    // `saves` is both a ProjectType folder and part of the crash-report
    // extras; deduplicate so watch/unwatch stay symmetric (a leftover watch
    // handle on a subfolder keeps Windows from renaming the instance root).
    let mut seen = HashSet::new();
    let mut paths = Vec::new();
    for sub in ProjectType::iterator()
        .map(|x| x.get_folder())
        .chain(["crash-reports", "saves"])
    {
        let full_path = full_instance_path.join(sub);
        if seen.insert(full_path.clone()) {
            paths.push(full_path);
        }
    }
    paths.push(full_instance_path.to_path_buf());
    paths
}

fn crash_task(instance_id: String) {
    tokio::task::spawn(async move {
        let res = async {
            let state = State::get().await?;
            let Some(instance) =
                instance_rows::get_instance_by_id(&instance_id, &state.pool)
                    .await?
            else {
                return Ok(());
            };

            if instance.install_stage == InstanceInstallStage::Installed {
                emit_minecraft_crash_warning(&instance_id, &instance.name)
                    .await?;
            }

            Ok::<(), crate::Error>(())
        }
        .await;

        match res {
            Ok(()) => {}
            Err(err) => {
                tracing::warn!("Unable to send crash report to frontend: {err}")
            }
        };
    });
}

fn is_config_sync_file_name(name: &std::ffi::OsStr) -> bool {
    let name = name.to_string_lossy();
    name == CONFIG_FILE_NAME || name == CONFIG_FILE_TEMP_NAME
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[tokio::test]
    async fn watched_instance_folder_cannot_be_renamed_on_windows() {
        let temp = tempfile::tempdir().unwrap();
        let dirs = DirectoryInfo {
            settings_dir: temp.path().to_path_buf(),
            config_dir: temp.path().to_path_buf(),
            app_identifier: "test".to_string(),
        };
        let watcher = init_watcher().await.unwrap();
        let instance_path = "watched-instance";
        let full_path = dirs.instances_dir().join(instance_path);
        std::fs::create_dir_all(&full_path).unwrap();

        watch_instance_folder(
            "instance-1",
            instance_path,
            &full_path,
            &watcher,
            true,
        )
        .await;

        // On Windows, an active watch keeps a directory handle open and blocks
        // renaming the instance folder (ERROR_ACCESS_DENIED). This is the
        // failure the symlink import used to hit.
        let rename_result = std::fs::rename(
            &full_path,
            temp.path().join("watched-instance.bak"),
        );
        assert!(
            rename_result.is_err(),
            "a watched folder must not be renameable on Windows"
        );

        // The import flow unwatches the folder first; after that the rename
        // must succeed (the watcher closes its handles asynchronously, so a
        // short retry window is needed).
        unwatch_instance_folder(instance_path, &full_path, &watcher).await;

        let mut renamed = false;
        for _ in 0..20 {
            match std::fs::rename(
                &full_path,
                temp.path().join("watched-instance.bak"),
            ) {
                Ok(()) => {
                    renamed = true;
                    break;
                }
                Err(error)
                    if error.kind() == std::io::ErrorKind::PermissionDenied =>
                {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
                Err(error) => panic!("unexpected rename error: {error:?}"),
            }
        }
        assert!(
            renamed,
            "rename should succeed after the folder is unwatched"
        );

        drop(watcher);
    }
}

#[cfg(test)]
mod config_file_name_tests {
    use super::*;
    use std::ffi::OsStr;

    #[test]
    fn recognizes_sync_config_files_but_not_other_instance_events() {
        assert!(is_config_sync_file_name(OsStr::new("axolotl_config.json")));
        assert!(is_config_sync_file_name(OsStr::new(
            "axolotl_config.json.tmp"
        )));
        assert!(!is_config_sync_file_name(OsStr::new("mods")));
        assert!(!is_config_sync_file_name(OsStr::new("servers.dat")));
    }

    #[tokio::test]
    async fn content_change_tracking_ignores_unrelated_paths() {
        let watcher = init_watcher().await.unwrap();
        watcher.content_changes.write().await.insert(
            "instance".to_string(),
            new_instance_content_change_state(),
        );
        watcher
            .track_upgrade_source(
                "instance",
                ["schematics/existing.schem".to_string()],
            )
            .await;

        watcher
            .record_upgrade_content_change("instance", "config/options.txt")
            .await;
        watcher
            .record_upgrade_content_change(
                "instance",
                "schematics/existing.schem",
            )
            .await;
        watcher
            .record_upgrade_content_change("instance", "mods/new.jar")
            .await;

        let snapshot =
            watcher.content_watch_snapshot("instance").await.unwrap();
        assert_eq!(snapshot.generation, 2);
        assert!(snapshot.dirty_paths.contains("mods/new.jar"));
        assert!(snapshot.dirty_paths.contains("schematics/existing.schem"));
        assert!(!snapshot.dirty_paths.contains("config/options.txt"));
    }

    #[tokio::test]
    async fn concurrent_content_notifications_do_not_lose_generation() {
        let watcher = Arc::new(init_watcher().await.unwrap());
        watcher.content_changes.write().await.insert(
            "instance".to_string(),
            new_instance_content_change_state(),
        );
        let mut tasks = Vec::new();
        for index in 0..32 {
            let watcher = Arc::clone(&watcher);
            tasks.push(tokio::spawn(async move {
                watcher
                    .record_upgrade_content_change(
                        "instance",
                        &format!("mods/{index}.jar"),
                    )
                    .await;
            }));
        }
        for task in tasks {
            task.await.unwrap();
        }

        let snapshot =
            watcher.content_watch_snapshot("instance").await.unwrap();
        assert_eq!(snapshot.generation, 32);
        assert_eq!(snapshot.dirty_paths.len(), 32);
    }

    #[tokio::test]
    async fn watcher_reinitialization_changes_content_epoch() {
        let first = init_watcher().await.unwrap();
        let second = init_watcher().await.unwrap();
        first.content_changes.write().await.insert(
            "instance".to_string(),
            new_instance_content_change_state(),
        );
        second.content_changes.write().await.insert(
            "instance".to_string(),
            new_instance_content_change_state(),
        );
        assert_ne!(
            first
                .content_watch_snapshot("instance")
                .await
                .unwrap()
                .epoch,
            second
                .content_watch_snapshot("instance")
                .await
                .unwrap()
                .epoch
        );
    }

    #[test]
    fn external_root_events_notify_every_instance_sharing_the_root() {
        let mut roots = HashMap::new();
        roots.insert(
            "/minecraft".to_string(),
            HashSet::from(["hmcl-a".to_string(), "hmcl-b".to_string()]),
        );
        let mut targets = external_root_event_targets(
            Path::new("/minecraft/mods/new.jar"),
            &roots,
        );
        targets.sort();
        assert_eq!(
            targets,
            vec![
                ("hmcl-a", "mods/new.jar".to_string()),
                ("hmcl-b", "mods/new.jar".to_string()),
            ]
        );
    }

    #[test]
    fn external_root_events_use_each_roots_own_relative_path() {
        let mut roots = HashMap::new();
        roots.insert(
            "/minecraft".to_string(),
            HashSet::from(["shared".to_string()]),
        );
        roots.insert(
            "/minecraft/versions/isolated".to_string(),
            HashSet::from(["isolated".to_string()]),
        );
        let mut targets = external_root_event_targets(
            Path::new("/minecraft/versions/isolated/mods/x.jar"),
            &roots,
        );
        targets.sort();
        assert_eq!(
            targets,
            vec![
                ("isolated", "mods/x.jar".to_string()),
                ("shared", "versions/isolated/mods/x.jar".to_string()),
            ]
        );
    }

    #[test]
    fn external_root_events_ignore_unrelated_and_managed_paths() {
        let mut roots = HashMap::new();
        roots.insert(
            "/minecraft".to_string(),
            HashSet::from(["instance".to_string()]),
        );
        assert!(
            external_root_event_targets(
                Path::new("/profiles/my-instance/mods/x.jar"),
                &roots,
            )
            .is_empty(),
            "managed profile events must not be attributed to external roots"
        );
        assert!(
            external_root_event_targets(
                Path::new("/elsewhere/mods/x.jar"),
                &roots
            )
            .is_empty()
        );
        assert_eq!(
            external_root_event_targets(Path::new("/minecraft"), &roots),
            vec![("instance", String::new())],
            "an event on the root path itself maps to an empty relative path"
        );
    }

    #[test]
    fn external_root_watch_is_released_only_by_its_last_instance() {
        let mut roots = HashMap::new();
        roots.insert(
            "/minecraft".to_string(),
            HashSet::from(["hmcl-a".to_string(), "hmcl-b".to_string()]),
        );

        // Removing one of two instances sharing the root must not release
        // the watch: the remaining instance still needs its events.
        assert!(!deregister_external_root(
            &mut roots,
            "/minecraft",
            "hmcl-a"
        ));
        assert_eq!(
            roots.get("/minecraft"),
            Some(&HashSet::from(["hmcl-b".to_string()]))
        );

        // Removing the last instance releases the root.
        assert!(deregister_external_root(&mut roots, "/minecraft", "hmcl-b"));
        assert!(roots.is_empty());

        // An unregistered root (e.g. the directory did not exist when the
        // instance was watched) falls back to releasing the paths.
        assert!(deregister_external_root(&mut roots, "/minecraft", "ghost"));
    }

    #[tokio::test]
    async fn direct_link_root_watch_never_creates_directories_and_registers_the_root()
     {
        let temp = tempfile::tempdir().unwrap();
        let watcher = init_watcher().await.unwrap();
        let minecraft = temp.path().join("minecraft");
        std::fs::create_dir_all(&minecraft).unwrap();

        watch_instance_folder(
            "instance-1",
            "virtual-profile-path",
            &minecraft,
            &watcher,
            false, // direct link: never create folders inside the linked root
        )
        .await;
        assert!(
            !minecraft.join("mods").exists(),
            "watching a linked root must not create mods/"
        );
        assert!(
            !minecraft.join("saves").exists(),
            "watching a linked root must not create saves/"
        );

        // Two instances sharing one `.minecraft` both register against it.
        watch_instance_folder(
            "instance-2",
            "virtual-profile-path-2",
            &minecraft,
            &watcher,
            false,
        )
        .await;
        let root_key = minecraft.to_string_lossy().to_string();
        let ids = watcher
            .external_root_instances
            .read()
            .await
            .get(&root_key)
            .expect("linked root registered")
            .clone();
        assert_eq!(
            ids,
            HashSet::from(["instance-1".to_string(), "instance-2".to_string()])
        );

        // Unwatching one instance keeps the root for the other.
        unwatch_instance_folder("virtual-profile-path", &minecraft, &watcher)
            .await;
        let ids = watcher
            .external_root_instances
            .read()
            .await
            .get(&root_key)
            .expect("linked root still registered")
            .clone();
        assert_eq!(ids, HashSet::from(["instance-2".to_string()]));

        // Unwatching the last instance drops the root entirely.
        unwatch_instance_folder("virtual-profile-path-2", &minecraft, &watcher)
            .await;
        assert!(watcher.external_root_instances.read().await.is_empty());
    }

    #[tokio::test]
    async fn managed_instance_watch_creates_missing_directories() {
        let temp = tempfile::tempdir().unwrap();
        let dirs = DirectoryInfo {
            settings_dir: temp.path().to_path_buf(),
            config_dir: temp.path().to_path_buf(),
            app_identifier: "test".to_string(),
        };
        let watcher = init_watcher().await.unwrap();
        let full_path = dirs.instances_dir().join("managed");
        std::fs::create_dir_all(&full_path).unwrap();

        watch_instance_folder(
            "instance-1",
            "managed",
            &full_path,
            &watcher,
            true,
        )
        .await;
        assert!(
            full_path.join("mods").exists(),
            "managed instances keep creating missing content subfolders"
        );
        assert!(
            watcher.instance_ids.read().await.contains_key("managed"),
            "managed instances stay keyed by their relative profile path"
        );
        assert!(
            watcher.external_root_instances.read().await.is_empty(),
            "managed instances never register an external root"
        );
    }
}
