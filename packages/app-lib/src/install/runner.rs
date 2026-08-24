use super::events::{InstallProgressReporter, emit_install_job};
use super::model::{
    InstallCleanup, InstallContinuationState, InstallErrorContext,
    InstallErrorView, InstallJavaStep, InstallJobDisplay, InstallJobEventKind,
    InstallJobSnapshot, InstallJobState, InstallJobStatus, InstallPauseReason,
    InstallPhaseDetails, InstallPhaseId, InstallPostInstallEdit,
    InstallProgress, InstallRequest, InstallRollbackState, InstallTarget,
};
use super::{diagnostics, recovery, store};
use crate::ErrorKind;
use crate::api::pack::install_from::{
    CreatePackLocation, generate_pack_from_file,
    generate_pack_from_version_id_with_reporter, get_instance_from_pack,
};
use crate::api::pack::install_mrpack::{
    MrpackInstallOutcome, install_zipped_mrpack_files_with_reporter,
    related_file_paths,
};
use crate::event::InstancePayloadType;
use crate::event::emit::emit_instance;
use crate::state::{
    ContentProviderRef, InstanceInstallStage, InstanceLink, LoaderComponent,
    LoaderComponentKind, LoaderComponentRole, ModLoader, State,
};
use crate::util::fetch::DownloadReason;
use std::collections::HashSet;
use std::path::PathBuf;
use uuid::Uuid;

enum InstallExecutionOutcome<T> {
    Completed(T),
    WaitingForUser(InstallPauseReason),
}

pub async fn create_instance(
    name: String,
    game_version: String,
    loader: ModLoader,
    loader_version: Option<String>,
    icon_path: Option<String>,
    link: InstanceLink,
) -> crate::Result<InstallJobSnapshot> {
    create_instance_with_adjuncts(
        name,
        game_version,
        loader,
        loader_version,
        Vec::new(),
        icon_path,
        link,
    )
    .await
}

pub async fn create_instance_with_adjuncts(
    name: String,
    game_version: String,
    loader: ModLoader,
    loader_version: Option<String>,
    adjuncts: Vec<crate::state::LoaderComponent>,
    icon_path: Option<String>,
    link: InstanceLink,
) -> crate::Result<InstallJobSnapshot> {
    start(InstallRequest::CreateInstance {
        name,
        game_version,
        loader,
        loader_version,
        adjuncts,
        icon_path,
        link,
    })
    .await
}

pub async fn create_modpack_instance(
    location: CreatePackLocation,
    post_install_edit: Option<InstallPostInstallEdit>,
) -> crate::Result<InstallJobSnapshot> {
    start(InstallRequest::CreateModpackInstance {
        location,
        post_install_edit,
    })
    .await
}

pub async fn import_instance(
    launcher_type: crate::api::pack::import::ImportLauncherType,
    base_path: PathBuf,
    instance_folder: String,
    symlink: bool,
) -> crate::Result<InstallJobSnapshot> {
    start(InstallRequest::ImportInstance {
        launcher_type,
        base_path,
        instance_folder,
        instance_path: None,
        symlink,
        game_version: None,
        loader: None,
        loader_version: None,
    })
    .await
}

/// Like [`import_instance`] but with a pre-resolved filesystem path.
/// Used by the frontend when the path is already known from scanning,
/// avoiding redundant config/registry re-resolution.
pub async fn import_instance_with_path(
    launcher_type: crate::api::pack::import::ImportLauncherType,
    base_path: PathBuf,
    instance_folder: String,
    instance_path: Option<String>,
    symlink: bool,
) -> crate::Result<InstallJobSnapshot> {
    start(InstallRequest::ImportInstance {
        launcher_type,
        base_path,
        instance_folder,
        instance_path,
        symlink,
        game_version: None,
        loader: None,
        loader_version: None,
    })
    .await
}

pub async fn import_instance_with_plan(
    launcher_type: crate::api::pack::import::ImportLauncherType,
    base_path: PathBuf,
    instance_folder: String,
    instance_path: Option<String>,
    symlink: bool,
    game_version: Option<String>,
    loader: Option<crate::state::ModLoader>,
    loader_version: Option<String>,
) -> crate::Result<InstallJobSnapshot> {
    start(InstallRequest::ImportInstance {
        launcher_type,
        base_path,
        instance_folder,
        instance_path,
        symlink,
        game_version,
        loader,
        loader_version,
    })
    .await
}

pub async fn duplicate_instance(
    source_instance_id: String,
) -> crate::Result<InstallJobSnapshot> {
    // Directly associated instances own no files to copy: duplicating one
    // would clone the linked launcher's `.minecraft` into Axolotl.
    let state = State::get().await?;
    if let Some(metadata) =
        crate::state::get_instance(&source_instance_id, &state.pool).await?
        && metadata.instance.is_direct_linked()
    {
        return Err(crate::ErrorKind::InputError(format!(
            "\"{}\" is directly associated with an external launcher and \
             cannot be duplicated; its files are managed by that launcher",
            metadata.instance.name
        ))
        .into());
    }
    drop(state);

    start(InstallRequest::DuplicateInstance { source_instance_id }).await
}

pub async fn install_existing_instance(
    instance_id: String,
    force: bool,
) -> crate::Result<InstallJobSnapshot> {
    start(InstallRequest::InstallExistingInstance { instance_id, force }).await
}

pub async fn install_content(
    instance_id: String,
    project_id: String,
    version_id: Option<String>,
    content_type: modrinth_content_management::ContentType,
    selected: modrinth_content_management::ResolutionPreferences,
    excluded_project_ids: Vec<String>,
    display_title: String,
    display_icon: Option<String>,
) -> crate::Result<InstallJobSnapshot> {
    start(InstallRequest::InstallContent {
        instance_id,
        project_id,
        version_id,
        content_type,
        selected,
        excluded_project_ids,
        display_title,
        display_icon,
    })
    .await
}

pub async fn install_curseforge_content(
    request: crate::api::curseforge::CurseForgeInstallRequest,
    display_title: String,
    display_icon: Option<String>,
) -> crate::Result<InstallJobSnapshot> {
    start(InstallRequest::InstallCurseForgeContent {
        request,
        display_title,
        display_icon,
    })
    .await
}

pub async fn install_curseforge_world(
    request: crate::api::curseforge::CurseForgeWorldInstallRequest,
    display_title: String,
    display_icon: Option<String>,
) -> crate::Result<InstallJobSnapshot> {
    start(InstallRequest::InstallCurseForgeWorld {
        request,
        display_title,
        display_icon,
    })
    .await
}

pub async fn download_java(
    vendor: String,
    version: u32,
) -> crate::Result<InstallJobSnapshot> {
    start(InstallRequest::DownloadJava { vendor, version }).await
}

pub async fn install_pack_to_existing_instance(
    instance_id: String,
    location: CreatePackLocation,
    post_install_edit: Option<InstallPostInstallEdit>,
) -> crate::Result<InstallJobSnapshot> {
    start(InstallRequest::InstallPackToExistingInstance {
        instance_id,
        location,
        post_install_edit,
    })
    .await
}

pub async fn update_managed_curseforge_modpack(
    instance_id: String,
    file_id: u32,
) -> crate::Result<InstallJobSnapshot> {
    start(InstallRequest::UpdateManagedCurseForgeModpack {
        instance_id,
        file_id,
    })
    .await
}

pub async fn list_jobs(
    include_finished: bool,
) -> crate::Result<Vec<InstallJobSnapshot>> {
    let state = State::get().await?;
    Ok(store::list(include_finished, &state)
        .await?
        .into_iter()
        .map(|job| job.snapshot())
        .collect())
}

pub async fn get_job(job_id: Uuid) -> crate::Result<InstallJobSnapshot> {
    let state = State::get().await?;
    Ok(store::get_required(job_id, &state).await?.snapshot())
}

pub async fn job_support_details(job_id: Uuid) -> crate::Result<String> {
    let state = State::get().await?;
    let job = store::get_required(job_id, &state).await?;
    diagnostics::build_job_support_details(&job, &state).await
}

pub async fn retry_job(job_id: Uuid) -> crate::Result<InstallJobSnapshot> {
    let state = State::get().await?;
    let mut job = store::get_required(job_id, &state).await?;

    if !matches!(
        job.status,
        InstallJobStatus::Failed | InstallJobStatus::Interrupted
    ) {
        return Err(crate::ErrorKind::InputError(
            "Only failed or interrupted install jobs can be retried"
                .to_string(),
        )
        .into());
    }

    job.state.target = job.state.request.target();
    job.state.cleanup = job.state.request.cleanup();
    job.state.rollback = None;
    job.state.error = None;
    job.state.rollback_error = None;
    job.state.pause_reason = None;
    job.state.continuation = None;
    job.state.context = None;
    job.state.progress.phase = InstallPhaseId::PreparingInstance;
    job.state.progress.progress = None;
    job.state.progress.details = InstallPhaseDetails::Empty;
    job.state.progress.parallel = None;
    prepare_initial_instance(&mut job.state, &state).await?;
    job.state.record_event(InstallJobEventKind::JobQueued {
        kind: job.state.request.kind(),
    });

    let record = store::update_status(
        job_id,
        InstallJobStatus::Queued,
        &job.state,
        &state,
    )
    .await?;
    emit_install_job(&record.snapshot()).await?;
    spawn_job(job_id);

    // The spawned job may already have progressed (or finished) by the time
    // the command returns; hand the caller the freshest stored state.
    Ok(store::get_required(job_id, &state).await?.snapshot())
}

pub async fn repair_cache_and_retry_job(
    job_id: Uuid,
) -> crate::Result<InstallJobSnapshot> {
    let state = State::get().await?;
    let initial_job = store::get_required(job_id, &state).await?;
    let _ = validated_cache_repair_types(&initial_job)?;

    let operation_lock = state
        .install_job_operation_locks
        .entry(job_id)
        .or_default()
        .clone();
    let mut operation = operation_lock.lock().await;
    if operation.cache_repair_started {
        return Ok(store::get_required(job_id, &state).await?.snapshot());
    }

    let job = store::get_required(job_id, &state).await?;
    let cache_types = validated_cache_repair_types(&job)?;
    operation.cache_repair_started = true;

    if let Err(error) =
        crate::state::CachedEntry::purge_cache_types(&cache_types, &state.pool)
            .await
    {
        operation.cache_repair_started = false;
        return Err(crate::ErrorKind::OtherError(format!(
            "Project cache cleanup failed; retry was not started: {error}"
        ))
        .into());
    }

    retry_job(job_id).await.map_err(|error| {
        crate::ErrorKind::OtherError(format!(
            "Project cache was cleared, but retry could not be started: {error}"
        ))
        .into()
    })
}

fn validated_cache_repair_types(
    job: &store::InstallJobRecord,
) -> crate::Result<Vec<crate::state::CacheValueType>> {
    validated_cache_repair_types_for(job.status, job.state.error.as_ref())
}

fn validated_cache_repair_types_for(
    status: InstallJobStatus,
    error: Option<&InstallErrorView>,
) -> crate::Result<Vec<crate::state::CacheValueType>> {
    if !matches!(
        status,
        InstallJobStatus::Failed | InstallJobStatus::Interrupted
    ) {
        return Err(crate::ErrorKind::InputError(
            "Only failed or interrupted install jobs can repair cache"
                .to_string(),
        )
        .into());
    }
    let error = error.ok_or_else(|| {
        crate::ErrorKind::InputError(
            "Install job has no cache repair error".to_string(),
        )
    })?;
    if error.code != "cache_repair_required" {
        return Err(crate::ErrorKind::InputError(
            "Install job does not require cache repair".to_string(),
        )
        .into());
    }
    let cache_types = error
        .context
        .as_ref()
        .map(|context| context.cache_types.as_slice())
        .unwrap_or_default();
    if cache_types.is_empty() {
        return Err(crate::ErrorKind::InputError(
            "Install job has no repairable cache types".to_string(),
        )
        .into());
    }

    let mut validated = Vec::new();
    for cache_type in cache_types {
        let cache_type =
            crate::state::CacheValueType::from_repairable_str(cache_type)
                .ok_or_else(|| {
                    crate::ErrorKind::InputError(format!(
                        "Cache type is not repairable: {cache_type}"
                    ))
                })?;
        if !validated.contains(&cache_type) {
            validated.push(cache_type);
        }
    }
    Ok(validated)
}

pub async fn resume_job(job_id: Uuid) -> crate::Result<InstallJobSnapshot> {
    let state = State::get().await?;
    let job = store::get_required(job_id, &state).await?;
    if job.status != InstallJobStatus::WaitingForUser {
        return Err(crate::ErrorKind::InputError(
            "Only install jobs waiting for user action can be resumed"
                .to_string(),
        )
        .into());
    }

    queue_waiting_job(job_id, job.state, &state).await
}

pub async fn skip_missing_content_and_resume_job(
    job_id: Uuid,
) -> crate::Result<InstallJobSnapshot> {
    let state = State::get().await?;
    let job = store::get_required(job_id, &state).await?;
    if job.status != InstallJobStatus::WaitingForUser {
        return Err(crate::ErrorKind::InputError(
            "Only install jobs waiting for user action can skip missing content"
                .to_string(),
        )
        .into());
    }
    if matches!(
        job.state.request,
        InstallRequest::UpdateManagedCurseForgeModpack { .. }
    ) {
        return Err(crate::ErrorKind::InputError(
            "CurseForge modpack version updates cannot skip required manual downloads"
                .to_string(),
        )
        .into());
    }

    let mut current_missing_paths = job
        .snapshot()
        .items
        .into_iter()
        .filter(|item| {
            item.status == super::model::DownloadItemStatus::Failed
                || (item.status == super::model::DownloadItemStatus::Skipped
                    && item.manual_url.is_some())
        })
        .map(|item| item.id)
        .collect::<Vec<_>>();
    let mut job_state = job.state;
    let InstallPauseReason::MissingRequiredContent { paths, .. } =
        job_state.pause_reason.as_ref().ok_or_else(|| {
            crate::ErrorKind::InputError(
                "Install job has no missing content to skip".to_string(),
            )
        })?;
    if current_missing_paths.is_empty() {
        current_missing_paths = paths.clone();
    }
    if current_missing_paths.is_empty() {
        return Err(crate::ErrorKind::InputError(
            "Install job has no missing content to skip".to_string(),
        )
        .into());
    }
    job_state
        .skipped_missing_content_paths
        .extend(current_missing_paths);
    job_state.skipped_missing_content_paths.sort_unstable();
    job_state.skipped_missing_content_paths.dedup();

    queue_waiting_job(job_id, job_state, &state).await
}

async fn queue_waiting_job(
    job_id: Uuid,
    mut job_state: InstallJobState,
    state: &State,
) -> crate::Result<InstallJobSnapshot> {
    prepare_resumed_job(&mut job_state);
    let Some(record) = store::update_status_if(
        job_id,
        InstallJobStatus::WaitingForUser,
        InstallJobStatus::Queued,
        &job_state,
        &state,
    )
    .await?
    else {
        return Err(crate::ErrorKind::InputError(
            "Install job is no longer waiting for user action".to_string(),
        )
        .into());
    };
    InstallProgressReporter::reset_job(job_id);
    emit_install_job(&record.snapshot()).await?;
    spawn_job(job_id);
    Ok(store::get_required(job_id, &state).await?.snapshot())
}

fn prepare_resumed_job(job_state: &mut InstallJobState) {
    job_state.pause_reason = None;
    job_state.error = None;
    job_state.rollback_error = None;
    job_state.context = None;
    job_state.active_downloads.clear();
    job_state.record_event(InstallJobEventKind::JobQueued {
        kind: job_state.request.kind(),
    });
}

pub async fn retry_job_as_new(
    job_id: Uuid,
) -> crate::Result<InstallJobSnapshot> {
    let state = State::get().await?;
    let job = store::get_required(job_id, &state).await?;
    if !matches!(
        job.status,
        InstallJobStatus::Failed
            | InstallJobStatus::Interrupted
            | InstallJobStatus::Canceled
    ) {
        return Err(crate::ErrorKind::InputError(
            "Only failed, interrupted, or canceled downloads can be retried"
                .to_string(),
        )
        .into());
    }
    let new_job = start(job.state.request).await?;
    // The spawned job may already have progressed (or finished) by the time
    // the command returns; hand the caller the freshest stored state.
    Ok(store::get_required(new_job.job_id, &state)
        .await?
        .snapshot())
}

pub async fn cancel_job(job_id: Uuid) -> crate::Result<InstallJobSnapshot> {
    let state = State::get().await?;
    let mut job = loop {
        let mut job = store::get_required(job_id, &state).await?;
        match job.status {
            InstallJobStatus::Running => {
                let Some(record) = store::update_status_if(
                    job_id,
                    InstallJobStatus::Running,
                    InstallJobStatus::Canceling,
                    &job.state,
                    &state,
                )
                .await?
                else {
                    continue;
                };
                if let Some(token) =
                    state.install_job_cancellations.get(&job_id)
                {
                    token.cancel();
                }
                emit_install_job(&record.snapshot()).await?;
                return Ok(record.snapshot());
            }
            InstallJobStatus::Canceling => return Ok(job.snapshot()),
            InstallJobStatus::Queued | InstallJobStatus::WaitingForUser => {
                let expected = job.status;
                begin_canceling_job(&mut job.state);
                let Some(record) = store::update_status_if(
                    job_id,
                    expected,
                    InstallJobStatus::Canceling,
                    &job.state,
                    &state,
                )
                .await?
                else {
                    continue;
                };
                emit_install_job(&record.snapshot()).await?;
                break record;
            }
            _ => {
                return Err(crate::ErrorKind::InputError(
                    "Only queued, running, or waiting install jobs can be canceled"
                        .to_string(),
                )
                .into());
            }
        }
    };

    let cleanup_succeeded =
        match recovery::apply_cleanup(&mut job.state, &state).await {
            Ok(()) => {
                job.state
                    .record_event(InstallJobEventKind::RollbackCompleted);
                true
            }
            Err(error) => {
                job.state.rollback_error = Some(InstallErrorView::from_error(
                    "rollback_error",
                    InstallPhaseId::RollingBack,
                    &error,
                    None,
                ));
                job.state.record_event(InstallJobEventKind::RollbackFailed {
                    message: error.to_string(),
                });
                false
            }
        };
    if cleanup_succeeded {
        clear_deleted_new_instance_id(&mut job.state);
    }
    let record = store::update_status(
        job_id,
        InstallJobStatus::Canceled,
        &job.state,
        &state,
    )
    .await?;
    emit_install_job(&record.snapshot()).await?;

    Ok(record.snapshot())
}

fn begin_canceling_job(job_state: &mut InstallJobState) {
    let canceled_phase = job_state.progress.phase;
    job_state.error = Some(InstallErrorView::from_message(
        "canceled",
        canceled_phase,
        "Install was canceled",
    ));
    job_state.pause_reason = None;
    job_state.record_event(InstallJobEventKind::JobCanceled {
        phase: canceled_phase,
    });
    job_state.progress.phase = InstallPhaseId::RollingBack;
    job_state.progress.progress = None;
    job_state.progress.details = InstallPhaseDetails::Empty;
    job_state.progress.parallel = None;
    job_state.record_event(InstallJobEventKind::RollbackStarted {
        cleanup: job_state.cleanup.clone(),
    });
}

pub async fn dismiss_job(job_id: Uuid) -> crate::Result<()> {
    let state = State::get().await?;
    store::dismiss(job_id, &state).await
}

pub async fn clear_job_history() -> crate::Result<u64> {
    let state = State::get().await?;
    store::clear_finished(&state).await
}

async fn start(request: InstallRequest) -> crate::Result<InstallJobSnapshot> {
    let state = State::get().await?;
    let id = Uuid::new_v4();
    let mut job_state = InstallJobState::new(request);
    prepare_initial_instance(&mut job_state, &state).await?;
    let record =
        store::insert(id, &job_state, InstallJobStatus::Queued, &state).await?;
    emit_install_job(&record.snapshot()).await?;
    spawn_job(id);
    Ok(record.snapshot())
}

async fn prepare_initial_instance(
    job_state: &mut InstallJobState,
    state: &State,
) -> crate::Result<()> {
    match job_state.request.clone() {
        InstallRequest::CreateInstance {
            name,
            mut game_version,
            mut loader,
            mut loader_version,
            mut adjuncts,
            icon_path,
            link,
        } => {
            if let InstanceLink::CurseForgeModpack {
                project_id,
                version_id,
            } = &link
            {
                let project_id = project_id.parse::<u32>().map_err(|_| {
                    ErrorKind::InputError(
                        "CurseForge project ID is invalid".to_string(),
                    )
                })?;
                let file_id = version_id.parse::<u32>().map_err(|_| {
                    ErrorKind::InputError(
                        "CurseForge file ID is invalid".to_string(),
                    )
                })?;
                let target = crate::api::curseforge::get_modpack_target(
                    project_id, file_id,
                )
                .await?;
                game_version = target.game_version;
                loader = target.loader;
                loader_version = target.loader_version;
                adjuncts.clear();
                job_state.request = InstallRequest::CreateInstance {
                    name: name.clone(),
                    game_version: game_version.clone(),
                    loader,
                    loader_version: loader_version.clone(),
                    adjuncts: Vec::new(),
                    icon_path: icon_path.clone(),
                    link: link.clone(),
                };
            }
            resolve_required_adjuncts(
                &game_version,
                loader,
                &mut adjuncts,
                state,
            )
            .await?;
            job_state.request = InstallRequest::CreateInstance {
                name: name.clone(),
                game_version: game_version.clone(),
                loader,
                loader_version: loader_version.clone(),
                adjuncts: adjuncts.clone(),
                icon_path: icon_path.clone(),
                link: link.clone(),
            };
            let metadata = crate::api::instance::create(
                name,
                game_version,
                loader,
                loader_version,
                icon_path,
                link,
                None,
            )
            .await?;
            if !adjuncts.is_empty() {
                let mut components = metadata.loader_components.clone();
                for adjunct in &mut adjuncts {
                    adjunct.instance_id = metadata.instance.id.clone();
                    adjunct.role = crate::state::LoaderComponentRole::Adjunct;
                }
                components.extend(adjuncts);
                validate_loader_components(&components)?;
                crate::state::instances::commands::replace_instance_loader_components(
					&metadata.instance.id,
					&components,
					&state.pool,
				)
				.await?;
            }
            set_display(
                job_state,
                metadata.instance.name,
                metadata.instance.icon_path,
            );
            set_instance_id(job_state, metadata.instance.id);
        }
        InstallRequest::CreateModpackInstance {
            location,
            post_install_edit,
        } => {
            let preview = get_instance_from_pack(location).await?;
            let name = post_install_edit
                .as_ref()
                .and_then(|edit| edit.name.clone())
                .unwrap_or_else(|| preview.name.clone());
            let icon_path = match post_install_edit
                .as_ref()
                .and_then(|edit| edit.icon_path.as_ref())
            {
                Some(icon_path) => icon_path.clone(),
                None => preview
                    .icon
                    .as_ref()
                    .map(|path| path.to_string_lossy().to_string())
                    .or_else(|| preview.icon_url.clone()),
            };
            let link = post_install_edit
                .as_ref()
                .and_then(|edit| edit.link.clone())
                .or_else(|| preview.link.clone())
                .unwrap_or(InstanceLink::Unmanaged);
            let metadata = crate::api::instance::create(
                name,
                preview.game_version,
                preview.modloader,
                preview.loader_version,
                icon_path,
                link,
                None,
            )
            .await?;
            set_display(
                job_state,
                metadata.instance.name,
                metadata.instance.icon_path,
            );
            set_instance_id(job_state, metadata.instance.id);
        }
        InstallRequest::ImportInstance {
            instance_folder,
            symlink: _,
            base_path: _,
            ..
        } => {
            let metadata = crate::api::instance::create(
                instance_folder,
                "unknown".to_string(),
                ModLoader::Vanilla,
                None,
                None,
                InstanceLink::Unmanaged,
                None,
            )
            .await?;
            set_display(
                job_state,
                metadata.instance.name,
                metadata.instance.icon_path,
            );
            set_instance_id(job_state, metadata.instance.id);
        }
        InstallRequest::DuplicateInstance { source_instance_id } => {
            let metadata =
                crate::state::get_instance(&source_instance_id, &state.pool)
                    .await?
                    .ok_or_else(|| {
                        crate::ErrorKind::InputError(
                            "Unknown instance".to_string(),
                        )
                    })?;
            let created = crate::api::instance::create(
                metadata.instance.name,
                metadata.applied_content_set.game_version,
                metadata.applied_content_set.loader,
                metadata.applied_content_set.loader_version,
                metadata.instance.icon_path,
                metadata.link,
                None,
            )
            .await?;
            set_display(
                job_state,
                created.instance.name,
                created.instance.icon_path,
            );
            set_instance_id(job_state, created.instance.id);
        }
        InstallRequest::InstallExistingInstance { instance_id, .. }
        | InstallRequest::InstallPackToExistingInstance {
            instance_id, ..
        }
        | InstallRequest::UpdateManagedCurseForgeModpack {
            instance_id, ..
        } => {
            prepare_existing_rollback(job_state, state, &instance_id).await?;
        }
        InstallRequest::InstallContent {
            instance_id,
            display_title,
            display_icon,
            ..
        } => {
            crate::state::get_instance(&instance_id, &state.pool)
                .await?
                .ok_or_else(|| {
                    crate::ErrorKind::InputError(format!(
                        "Unknown instance {instance_id}"
                    ))
                })?;
            set_display(job_state, display_title, display_icon);
        }
        InstallRequest::InstallCurseForgeContent {
            request,
            display_title,
            display_icon,
        } => {
            crate::state::get_instance(&request.instance_id, &state.pool)
                .await?
                .ok_or_else(|| {
                    crate::ErrorKind::InputError(format!(
                        "Unknown instance {}",
                        request.instance_id
                    ))
                })?;
            set_display(job_state, display_title, display_icon);
        }
        InstallRequest::InstallCurseForgeWorld {
            request,
            display_title,
            display_icon,
        } => {
            crate::state::get_instance(&request.instance_id, &state.pool)
                .await?
                .ok_or_else(|| {
                    crate::ErrorKind::InputError(format!(
                        "Unknown instance {}",
                        request.instance_id
                    ))
                })?;
            set_display(job_state, display_title, display_icon);
        }
        InstallRequest::DownloadJava { vendor, version } => {
            set_display(job_state, format!("Java {version} ({vendor})"), None);
        }
    }

    Ok(())
}

fn spawn_job(job_id: Uuid) {
    tokio::spawn(async move {
        if let Err(error) = run_job(job_id).await {
            tracing::error!("Install job {job_id} failed: {error}");
        }
    });
}

fn begin_failed_job_rollback(
    job_state: &mut InstallJobState,
    error: &crate::Error,
) {
    let failed_phase = job_state.progress.phase;
    let error_view =
        install_error_view(failed_phase, error, job_state.context.clone());
    job_state.record_event(InstallJobEventKind::Failed {
        phase: failed_phase,
        code: error_view.code.clone(),
        message: error_view.message.clone(),
    });
    job_state.error = Some(error_view);
    job_state.progress.phase = InstallPhaseId::RollingBack;
    job_state.progress.progress = None;
    job_state.progress.details = InstallPhaseDetails::Empty;
    job_state.progress.parallel = None;
    job_state.record_event(InstallJobEventKind::RollbackStarted {
        cleanup: job_state.cleanup.clone(),
    });
}

fn begin_waiting_for_user(
    job_state: &mut InstallJobState,
    reason: InstallPauseReason,
) {
    job_state.pause_reason = Some(reason.clone());
    job_state.error = None;
    job_state.rollback_error = None;
    job_state.context = None;
    job_state.progress.parallel = None;
    job_state.record_event(InstallJobEventKind::WaitingForUser { reason });
}

async fn run_job(job_id: Uuid) -> crate::Result<()> {
    let state = State::get().await?;
    let mut job = store::get_required(job_id, &state).await?;

    if job.status != InstallJobStatus::Queued {
        return Ok(());
    }

    let _install_permit = state.install_job_semaphore.acquire().await?;
    job = store::get_required(job_id, &state).await?;

    if job.status != InstallJobStatus::Queued {
        return Ok(());
    }

    let mut job_state = job.state.clone();
    job_state.record_event(InstallJobEventKind::JobStarted);
    let Some(record) = store::update_status_if(
        job_id,
        InstallJobStatus::Queued,
        InstallJobStatus::Running,
        &job_state,
        &state,
    )
    .await?
    else {
        return Ok(());
    };
    let cancellation = tokio_util::sync::CancellationToken::new();
    state
        .install_job_cancellations
        .insert(job_id, cancellation.clone());
    emit_install_job(&record.snapshot()).await?;
    if store::get_required(job_id, &state).await?.status
        == InstallJobStatus::Canceling
    {
        cancellation.cancel();
    }
    let live_reporter = InstallProgressReporter::new(job_id, job_state.clone());

    enum RunResult {
        Completed(crate::Result<InstallExecutionOutcome<Option<String>>>),
        Canceled,
    }

    let result = tokio::select! {
        biased;
        _ = cancellation.cancelled() => RunResult::Canceled,
        result = run_request(job_id, &mut job_state, &state) => RunResult::Completed(result),
    };
    state.install_job_cancellations.remove(&job_id);
    job_state = live_reporter.current_state().await?;

    match result {
        RunResult::Completed(Ok(InstallExecutionOutcome::Completed(
            instance_id,
        ))) => {
            if let Some(instance_id) = instance_id.as_ref() {
                set_instance_id(&mut job_state, instance_id.clone());
            }
            if cancellation.is_cancelled() {
                finish_canceled_job(job_id, &mut job_state, &state).await?;
                return Ok(());
            }
            job_state.record_event(InstallJobEventKind::JobSucceeded {
                instance_id: current_instance_id(&job_state),
            });
            job_state.progress.phase = InstallPhaseId::Finalizing;
            job_state.progress.progress = None;
            job_state.progress.details = InstallPhaseDetails::Empty;
            job_state.progress.parallel = None;
            job_state.error = None;
            job_state.rollback_error = None;
            job_state.pause_reason = None;
            job_state.continuation = None;
            job_state.missing_content = None;
            job_state.skipped_missing_content_paths.clear();
            job_state.context = None;
            let mut completed_state = job_state.clone();
            completed_state.rollback = None;
            let Some(record) =
                store::complete_running_job(job_id, &completed_state, &state)
                    .await?
            else {
                if store::get_required(job_id, &state).await?.status
                    == InstallJobStatus::Canceling
                {
                    finish_canceled_job(job_id, &mut job_state, &state).await?;
                }
                return Ok(());
            };
            if let Err(error) =
                recovery::discard_content_rollback(&mut job_state, &state).await
            {
                tracing::warn!(
                    job_id = %job_id,
                    error = %error,
                    "Install job succeeded, but rollback staging could not be discarded"
                );
            }
            if let Err(error) = emit_install_job(&record.snapshot()).await {
                tracing::warn!(
                    job_id = %job_id,
                    error = %error,
                    "Install job succeeded, but its final event could not be emitted"
                );
            }
            if let Some(instance_id) = instance_id
                && let Err(error) =
                    emit_instance(&instance_id, InstancePayloadType::Edited)
                        .await
            {
                tracing::warn!(
                    job_id = %job_id,
                    instance_id,
                    error = %error,
                    "Install job succeeded, but its final instance event could not be emitted"
                );
            }
        }
        RunResult::Completed(Ok(InstallExecutionOutcome::WaitingForUser(
            reason,
        ))) => {
            let mut waiting_state = job_state.clone();
            begin_waiting_for_user(&mut waiting_state, reason);
            let Some(record) = store::update_status_if(
                job_id,
                InstallJobStatus::Running,
                InstallJobStatus::WaitingForUser,
                &waiting_state,
                &state,
            )
            .await?
            else {
                if store::get_required(job_id, &state).await?.status
                    == InstallJobStatus::Canceling
                {
                    finish_canceled_job(job_id, &mut job_state, &state).await?;
                }
                return Ok(());
            };
            emit_install_job(&record.snapshot()).await?;
        }
        RunResult::Canceled => {
            finish_canceled_job(job_id, &mut job_state, &state).await?;
        }
        RunResult::Completed(Err(error)) => {
            begin_failed_job_rollback(&mut job_state, &error);
            let cleanup_succeeded = match recovery::apply_cleanup(
                &mut job_state,
                &state,
            )
            .await
            {
                Err(rollback_error) => {
                    tracing::error!(
                        "Error rolling back failed install job {job_id}: {rollback_error}"
                    );
                    job_state.rollback_error = Some(install_error_view(
                        InstallPhaseId::RollingBack,
                        &rollback_error,
                        None,
                    ));
                    job_state.record_event(
                        InstallJobEventKind::RollbackFailed {
                            message: rollback_error.to_string(),
                        },
                    );
                    false
                }
                Ok(()) => {
                    job_state
                        .record_event(InstallJobEventKind::RollbackCompleted);
                    true
                }
            };
            if cleanup_succeeded {
                clear_deleted_new_instance_id(&mut job_state);
            }
            let record = store::update_status(
                job_id,
                InstallJobStatus::Failed,
                &job_state,
                &state,
            )
            .await?;
            emit_install_job(&record.snapshot()).await?;
            return Err(error);
        }
    }

    Ok(())
}

async fn finish_canceled_job(
    job_id: Uuid,
    job_state: &mut InstallJobState,
    state: &State,
) -> crate::Result<()> {
    let canceled_phase = job_state.progress.phase;
    job_state.error = Some(InstallErrorView::from_message(
        "canceled",
        canceled_phase,
        "Install was canceled",
    ));
    job_state.pause_reason = None;
    job_state.record_event(InstallJobEventKind::JobCanceled {
        phase: canceled_phase,
    });
    job_state.progress.phase = InstallPhaseId::RollingBack;
    job_state.progress.progress = None;
    job_state.progress.details = InstallPhaseDetails::Empty;
    job_state.record_event(InstallJobEventKind::RollbackStarted {
        cleanup: job_state.cleanup.clone(),
    });
    let cleanup_succeeded =
        match recovery::apply_cleanup(job_state, state).await {
            Err(rollback_error) => {
                job_state.rollback_error = Some(install_error_view(
                    InstallPhaseId::RollingBack,
                    &rollback_error,
                    None,
                ));
                job_state.record_event(InstallJobEventKind::RollbackFailed {
                    message: rollback_error.to_string(),
                });
                false
            }
            Ok(()) => {
                job_state.record_event(InstallJobEventKind::RollbackCompleted);
                true
            }
        };
    if cleanup_succeeded {
        clear_deleted_new_instance_id(job_state);
    }
    let record = store::update_status(
        job_id,
        InstallJobStatus::Canceled,
        job_state,
        state,
    )
    .await?;
    emit_install_job(&record.snapshot()).await
}

async fn run_request(
    job_id: Uuid,
    job_state: &mut InstallJobState,
    state: &State,
) -> crate::Result<InstallExecutionOutcome<Option<String>>> {
    match job_state.request.clone() {
        InstallRequest::CreateInstance {
            name,
            game_version,
            loader,
            loader_version: _,
            adjuncts,
            icon_path: _,
            link,
        } => {
            let Some(instance_id) = current_instance_id(job_state) else {
                return Err(crate::ErrorKind::InputError(
                    "Install job is missing its instance id".to_string(),
                )
                .into());
            };
            update_progress(
                job_id,
                job_state,
                state,
                InstallPhaseId::PreparingInstance,
                InstallPhaseDetails::Instance { name: name.clone() },
            )
            .await?;
            let reporter =
                InstallProgressReporter::new(job_id, job_state.clone());
            if let InstanceLink::CurseForgeModpack {
                project_id,
                version_id,
            } = link
            {
                let project_id = project_id.parse::<u32>().map_err(|_| {
                    ErrorKind::InputError(
                        "CurseForge project ID is invalid".to_string(),
                    )
                })?;
                let file_id = version_id.parse::<u32>().map_err(|_| {
                    ErrorKind::InputError(
                        "CurseForge file ID is invalid".to_string(),
                    )
                })?;
                crate::state::instances::commands::set_instance_install_stage(
                    &instance_id,
                    InstanceInstallStage::PackInstalling,
                    &state.pool,
                )
                .await?;
                emit_instance(&instance_id, InstancePayloadType::Edited)
                    .await?;
                let result = crate::api::curseforge::install_modpack_with_reporter(
                    crate::api::curseforge::CurseForgeModpackInstallRequest {
                        instance_id: instance_id.clone(),
                        project_id,
                        file_id,
                        install_optional: false,
                        allow_target_change: false,
                    },
                    Some(reporter.clone()),
                )
                .await?;
                if let Some(reason) = curseforge_manual_download_pause(
                    &result,
                    &job_state.skipped_missing_content_paths,
                ) {
                    return Ok(InstallExecutionOutcome::WaitingForUser(reason));
                }
            }
            reporter
                .update(
                    InstallPhaseId::DownloadingMinecraft,
                    None,
                    InstallPhaseDetails::Minecraft {
                        game_version: game_version.clone(),
                        loader,
                    },
                )
                .await?;
            let context =
                crate::state::instances::commands::get_instance_launch_context(
                    &instance_id,
                    &state.pool,
                )
                .await?
                .ok_or_else(|| {
                    crate::ErrorKind::InputError("Unknown instance".to_string())
                })?;
            crate::launcher::install_minecraft_with_reporter(
                &context,
                false,
                Some(reporter.clone()),
                crate::launcher::InstanceCompletionPolicy::DeferToInstallJob,
            )
            .await?;
            install_adjunct_components(
                state,
                &instance_id,
                &adjuncts,
                &game_version,
                loader,
                reporter.cancellation_token(),
            )
            .await?;
            Ok(InstallExecutionOutcome::Completed(Some(instance_id)))
        }
        InstallRequest::CreateModpackInstance {
            location,
            post_install_edit,
        } => {
            let Some(instance_id) = current_instance_id(job_state) else {
                return Err(crate::ErrorKind::InputError(
                    "Install job is missing its instance id".to_string(),
                )
                .into());
            };
            update_progress(
                job_id,
                job_state,
                state,
                InstallPhaseId::ResolvingPack,
                modpack_details(&location),
            )
            .await?;
            if let InstallExecutionOutcome::WaitingForUser(reason) =
                install_pack(
                    job_id,
                    job_state,
                    location,
                    instance_id.clone(),
                    DownloadReason::Modpack,
                )
                .await?
            {
                return Ok(InstallExecutionOutcome::WaitingForUser(reason));
            }
            apply_post_install_edit(&instance_id, post_install_edit).await?;
            Ok(InstallExecutionOutcome::Completed(Some(instance_id)))
        }
        InstallRequest::ImportInstance {
            launcher_type,
            base_path,
            instance_folder,
            instance_path,
            symlink,
            game_version,
            loader,
            loader_version,
        } => {
            tracing::debug!(
                "InstallRequest::ImportInstance: launcher_type={launcher_type} base_path={} instance_folder={instance_folder} symlink={symlink}",
                base_path.display()
            );
            let Some(instance_id) = current_instance_id(job_state) else {
                return Err(crate::ErrorKind::InputError(
                    "Install job is missing its instance id".to_string(),
                )
                .into());
            };
            update_progress(
                job_id,
                job_state,
                state,
                InstallPhaseId::PreparingInstance,
                InstallPhaseDetails::Import {
                    launcher_type,
                    instance_folder: instance_folder.clone(),
                },
            )
            .await?;
            crate::api::pack::import::import_instance_with_reporter(
                &instance_id,
                launcher_type,
                base_path,
                instance_folder,
                instance_path,
                crate::api::pack::import::ImportOverrides {
                    game_version,
                    loader,
                    loader_version,
                },
                // TODO(B2): apply overrides to launcher-specific importers
                // (MultiMC/Prism/ATLauncher/GDLauncher/Curseforge/ModrinthApp);
                // generic/PCL/HMCL/Axolotl paths already consume them.
                InstallProgressReporter::new(job_id, job_state.clone()),
                symlink,
            )
            .await?;
            let context =
                crate::state::instances::commands::get_instance_launch_context(
                    &instance_id,
                    &state.pool,
                )
                .await?
                .ok_or_else(|| {
                    crate::ErrorKind::InputError("Unknown instance".to_string())
                })?;
            crate::launcher::install_minecraft_with_reporter(
                &context,
                false,
                Some(InstallProgressReporter::new(job_id, job_state.clone())),
                crate::launcher::InstanceCompletionPolicy::DeferToInstallJob,
            )
            .await?;
            Ok(InstallExecutionOutcome::Completed(Some(instance_id)))
        }
        InstallRequest::DuplicateInstance { source_instance_id } => {
            let Some(instance_id) = current_instance_id(job_state) else {
                return Err(crate::ErrorKind::InputError(
                    "Install job is missing its instance id".to_string(),
                )
                .into());
            };
            update_progress(
                job_id,
                job_state,
                state,
                InstallPhaseId::PreparingInstance,
                InstallPhaseDetails::Empty,
            )
            .await?;
            let state = State::get().await?;
            crate::api::pack::import::copy_dotminecraft_with_reporter(
                &instance_id,
                crate::api::instance::get_full_path(&source_instance_id)
                    .await?,
                &state.io_semaphore,
                InstallProgressReporter::new(job_id, job_state.clone()),
                InstallPhaseDetails::Empty,
            )
            .await?;
            let context =
                crate::state::instances::commands::get_instance_launch_context(
                    &instance_id,
                    &state.pool,
                )
                .await?
                .ok_or_else(|| {
                    crate::ErrorKind::InputError("Unknown instance".to_string())
                })?;
            crate::launcher::install_minecraft_with_reporter(
                &context,
                false,
                Some(InstallProgressReporter::new(job_id, job_state.clone())),
                crate::launcher::InstanceCompletionPolicy::DeferToInstallJob,
            )
            .await?;
            Ok(InstallExecutionOutcome::Completed(Some(instance_id)))
        }
        InstallRequest::InstallExistingInstance { instance_id, force } => {
            prepare_existing_rollback(job_state, state, &instance_id).await?;
            update_progress(
                job_id,
                job_state,
                state,
                InstallPhaseId::DownloadingMinecraft,
                InstallPhaseDetails::Empty,
            )
            .await?;
            let context =
                crate::state::instances::commands::get_instance_launch_context(
                    &instance_id,
                    &state.pool,
                )
                .await?
                .ok_or_else(|| {
                    crate::ErrorKind::InputError("Unknown instance".to_string())
                })?;
            crate::launcher::install_minecraft_with_reporter(
                &context,
                force,
                Some(InstallProgressReporter::new(job_id, job_state.clone())),
                crate::launcher::InstanceCompletionPolicy::DeferToInstallJob,
            )
            .await?;
            Ok(InstallExecutionOutcome::Completed(Some(instance_id)))
        }
        InstallRequest::InstallPackToExistingInstance {
            instance_id,
            location,
            post_install_edit,
        } => {
            prepare_existing_rollback(job_state, state, &instance_id).await?;
            let disabled_project_ids = match job_state.continuation.clone() {
                Some(InstallContinuationState::InstallingPackToExistingInstance {
                    disabled_project_ids,
                }) => disabled_project_ids.into_iter().collect(),
                None => {
                    let disabled_project_ids = remove_existing_pack_content(
                        job_id,
                        job_state,
                        state,
                        &instance_id,
                    )
                    .await?;
                    let mut persisted_ids = disabled_project_ids
                        .iter()
                        .cloned()
                        .collect::<Vec<_>>();
                    persisted_ids.sort_unstable();
                    let continuation = InstallContinuationState::InstallingPackToExistingInstance {
                        disabled_project_ids: persisted_ids,
                    };
                    job_state.continuation = Some(continuation.clone());
                    InstallProgressReporter::new(job_id, job_state.clone())
                        .set_continuation(Some(continuation))
                        .await?;
                    disabled_project_ids
                }
            };
            if let InstallExecutionOutcome::WaitingForUser(reason) =
                install_pack(
                    job_id,
                    job_state,
                    location,
                    instance_id.clone(),
                    DownloadReason::Modpack,
                )
                .await?
            {
                return Ok(InstallExecutionOutcome::WaitingForUser(reason));
            }
            restore_disabled_projects(
                &instance_id,
                disabled_project_ids,
                state,
            )
            .await?;
            job_state.continuation = None;
            InstallProgressReporter::new(job_id, job_state.clone())
                .set_continuation(None)
                .await?;
            apply_post_install_edit(&instance_id, post_install_edit).await?;
            Ok(InstallExecutionOutcome::Completed(Some(instance_id)))
        }
        InstallRequest::InstallContent {
            instance_id,
            project_id,
            version_id,
            content_type,
            selected,
            excluded_project_ids,
            display_title: _,
            display_icon: _,
        } => {
            update_progress(
                job_id,
                job_state,
                state,
                InstallPhaseId::DownloadingContent,
                InstallPhaseDetails::Empty,
            )
            .await?;
            let plan = crate::state::instances::commands::resolve_install_plan(
                &instance_id,
                crate::state::instances::commands::InstanceInstallProjectRequest {
                    project_id: project_id.clone(),
                    version_id,
                    content_type,
                    selected,
                    excluded_project_ids,
                    force_project_ids: Vec::new(),
                },
                state,
            )
            .await?;
            let total = (plan.dependencies.len() + 1) as u64;
            let reporter =
                InstallProgressReporter::new(job_id, job_state.clone());
            reporter
                .update(
                    InstallPhaseId::DownloadingContent,
                    Some(InstallProgress {
                        current: 0,
                        total,
                        secondary: None,
                    }),
                    InstallPhaseDetails::Empty,
                )
                .await?;
            crate::state::instances::commands::install_resolved_content_plan_with_reporter(
                &instance_id,
                &plan,
                Some(reporter.clone()),
                state,
            )
            .await?;
            reporter
                .update(
                    InstallPhaseId::DownloadingContent,
                    Some(InstallProgress {
                        current: total,
                        total,
                        secondary: None,
                    }),
                    InstallPhaseDetails::Empty,
                )
                .await?;
            crate::api::instance::emit_content_changed(&instance_id).await?;
            let dependency_project_ids = plan
                .dependencies
                .iter()
                .map(|dependency| dependency.project_id.clone())
                .collect::<Vec<_>>();
            emit_instance(
                &instance_id,
                InstancePayloadType::ContentInstallFinished {
                    project_ids: std::iter::once(project_id.clone())
                        .chain(dependency_project_ids.iter().cloned())
                        .collect(),
                    dependency_project_ids,
                },
            )
            .await?;
            Ok(InstallExecutionOutcome::Completed(Some(instance_id)))
        }
        InstallRequest::InstallCurseForgeContent {
            request,
            display_title: _,
            display_icon: _,
        } => {
            let instance_id = request.instance_id.clone();
            let primary_project_id = request.project_id;
            update_progress(
                job_id,
                job_state,
                state,
                InstallPhaseId::DownloadingContent,
                InstallPhaseDetails::Empty,
            )
            .await?;
            let reporter =
                InstallProgressReporter::new(job_id, job_state.clone());
            let result = crate::api::curseforge::install_file_with_reporter(
                request, reporter,
            )
            .await?;
            crate::api::instance::emit_content_changed(&instance_id).await?;
            let dependency_project_ids = result
                .installed
                .iter()
                .filter(|installed| installed.dependency)
                .map(|installed| format!("curseforge:{}", installed.project_id))
                .collect::<Vec<_>>();
            emit_instance(
                &instance_id,
                InstancePayloadType::ContentInstallFinished {
                    project_ids: std::iter::once(format!(
                        "curseforge:{primary_project_id}"
                    ))
                    .chain(dependency_project_ids.iter().cloned())
                    .collect(),
                    dependency_project_ids,
                },
            )
            .await?;
            Ok(InstallExecutionOutcome::Completed(Some(instance_id)))
        }
        InstallRequest::InstallCurseForgeWorld {
            request,
            display_title: _,
            display_icon: _,
        } => {
            let instance_id = request.instance_id.clone();
            if curseforge_world_was_imported_manually(job_state, &request) {
                return Ok(InstallExecutionOutcome::Completed(Some(
                    instance_id,
                )));
            }
            update_progress(
                job_id,
                job_state,
                state,
                InstallPhaseId::DownloadingContent,
                InstallPhaseDetails::Empty,
            )
            .await?;
            let reporter =
                InstallProgressReporter::new(job_id, job_state.clone());
            let result = crate::api::curseforge::install_world_with_reporter(
                request.clone(),
                reporter.clone(),
            )
            .await?;
            if let Some(manual_download) = result.manual_download {
                let path = format!("saves/{}", manual_download.file_name);
                let manual_url = manual_download.website_url.clone().or_else(|| {
					Some(format!(
						"https://www.curseforge.com/minecraft/worlds/{}/download/{}",
						manual_download.project_slug, manual_download.file_id
					))
				});
                reporter
                    .record_events(vec![
                        InstallJobEventKind::ContentFileSkipped {
                            path: path.clone(),
                            reason: "CurseForge requires a manual download"
                                .to_string(),
                            project_id: Some(
                                manual_download.project_id.to_string(),
                            ),
                            version_id: Some(
                                manual_download.file_id.to_string(),
                            ),
                            manual_url,
                        },
                    ])
                    .await?;
                return Ok(InstallExecutionOutcome::WaitingForUser(
                    InstallPauseReason::MissingRequiredContent {
                        failed_files: 1,
                        paths: vec![path],
                    },
                ));
            }
            Ok(InstallExecutionOutcome::Completed(Some(instance_id)))
        }
        InstallRequest::UpdateManagedCurseForgeModpack {
            instance_id,
            file_id,
        } => {
            prepare_existing_rollback(job_state, state, &instance_id).await?;
            crate::state::instances::commands::set_instance_install_stage(
                &instance_id,
                InstanceInstallStage::PackInstalling,
                &state.pool,
            )
            .await?;
            emit_instance(&instance_id, InstancePayloadType::Edited).await?;
            update_progress(
                job_id,
                job_state,
                state,
                InstallPhaseId::DownloadingContent,
                InstallPhaseDetails::Empty,
            )
            .await?;
            let reporter =
                InstallProgressReporter::new(job_id, job_state.clone());
            let result =
                crate::api::curseforge::update_managed_modpack_with_reporter(
                    &instance_id,
                    file_id,
                    Some(reporter.clone()),
                )
                .await?;
            if !result.content.failed_downloads.is_empty() {
                return Err(ErrorKind::NetworkError(format!(
                    "{} CurseForge files could not be downloaded automatically",
                    result.content.failed_downloads.len()
                ))
                .into());
            }
            if let Some(reason) = curseforge_manual_download_pause(
                &result,
                &job_state.skipped_missing_content_paths,
            ) {
                return Ok(InstallExecutionOutcome::WaitingForUser(reason));
            }
            Ok(InstallExecutionOutcome::Completed(Some(instance_id)))
        }
        InstallRequest::DownloadJava { vendor, version } => {
            update_progress(
                job_id,
                job_state,
                state,
                InstallPhaseId::PreparingJava,
                InstallPhaseDetails::Java {
                    major_version: version,
                    step: InstallJavaStep::FetchingMetadata,
                },
            )
            .await?;
            let reporter =
                InstallProgressReporter::new(job_id, job_state.clone());
            let path = crate::api::jre::download_java_from_feed_with_reporter(
                &vendor, version, reporter,
            )
            .await?;
            let _ = path;
            Ok(InstallExecutionOutcome::Completed(None))
        }
    }
}

async fn apply_post_install_edit(
    instance_id: &str,
    edit: Option<InstallPostInstallEdit>,
) -> crate::Result<()> {
    let Some(edit) = edit else {
        return Ok(());
    };

    if edit.name.is_none() && edit.icon_path.is_none() && edit.link.is_none() {
        return Ok(());
    }

    crate::api::instance::edit(
        instance_id,
        crate::state::instances::commands::EditInstance {
            name: edit.name,
            icon_path: edit.icon_path,
            link: edit.link,
            ..Default::default()
        },
    )
    .await?;
    Ok(())
}

async fn remove_existing_pack_content(
    job_id: Uuid,
    job_state: &mut InstallJobState,
    state: &State,
    instance_id: &str,
) -> crate::Result<HashSet<String>> {
    let metadata = crate::state::instances::commands::get_instance_metadata(
        instance_id,
        &state.pool,
    )
    .await?
    .ok_or_else(|| {
        crate::ErrorKind::InputError("Unknown instance".to_string())
    })?;
    let (project_id, version_id) = match &metadata.link {
        InstanceLink::ModrinthModpack {
            project_id,
            version_id,
        } => (project_id.clone(), version_id.clone()),
        InstanceLink::ServerProjectModpack {
            content_project_id,
            content_version_id,
            ..
        } => (content_project_id.clone(), content_version_id.clone()),
        InstanceLink::ImportedModpack { .. } => {
            recovery::prepare_existing_content_rollback(
                job_id,
                job_state,
                state,
                Vec::new(),
            )
            .await?;
            return Ok(HashSet::new());
        }
        _ => return Ok(HashSet::new()),
    };

    let disabled_project_ids =
        crate::state::instances::commands::list_project_files(
            instance_id,
            state,
        )
        .await?
        .into_iter()
        .filter_map(|file| {
            (!file.enabled).then(|| {
                file.provider_refs
                    .iter()
                    .find_map(|provider| match provider {
                        ContentProviderRef::Modrinth { project_id, .. } => {
                            Some(project_id.to_string())
                        }
                        ContentProviderRef::CurseForge { .. } => None,
                    })
            })?
        })
        .collect::<HashSet<_>>();
    let reporter = InstallProgressReporter::new(job_id, job_state.clone());
    let old_pack = generate_pack_from_version_id_with_reporter(
        project_id.clone(),
        version_id.clone(),
        metadata.instance.name.clone(),
        None,
        instance_id.to_string(),
        DownloadReason::Update,
        reporter,
    )
    .await?;

    let related_paths = related_file_paths(&old_pack.file).await?;
    recovery::prepare_existing_content_rollback(
        job_id,
        job_state,
        state,
        related_paths,
    )
    .await?;

    Ok(disabled_project_ids)
}

async fn restore_disabled_projects(
    instance_id: &str,
    disabled_project_ids: HashSet<String>,
    state: &State,
) -> crate::Result<()> {
    if disabled_project_ids.is_empty() {
        return Ok(());
    }

    for file in crate::state::instances::commands::list_project_files(
        instance_id,
        state,
    )
    .await?
    {
        let is_disabled_modrinth_project = file.provider_refs.iter().any(
            |provider| {
                matches!(
                    provider,
                    ContentProviderRef::Modrinth { project_id, .. }
                        if disabled_project_ids.contains(&project_id.to_string())
                )
            },
        );
        if file.enabled && is_disabled_modrinth_project {
            crate::state::instances::commands::toggle_disable_project(
                instance_id,
                &file.relative_path,
                Some(false),
                state,
            )
            .await?;
        }
    }

    Ok(())
}

async fn install_pack(
    job_id: Uuid,
    job_state: &mut InstallJobState,
    location: CreatePackLocation,
    instance_id: String,
    reason: DownloadReason,
) -> crate::Result<InstallExecutionOutcome<()>> {
    let reporter = InstallProgressReporter::new(job_id, job_state.clone());
    reporter
        .update(
            InstallPhaseId::DownloadingPackFile,
            None,
            modpack_details(&location),
        )
        .await?;

    let create_pack = match location {
        CreatePackLocation::FromVersionId {
            project_id,
            version_id,
            title,
            icon_url,
        } => {
            reporter
                .set_context(
                    InstallErrorContext::new("download modpack file")
                        .project_id(project_id.clone())
                        .version_id(version_id.clone())
                        .build(),
                )
                .await?;
            generate_pack_from_version_id_with_reporter(
                project_id,
                version_id,
                title,
                icon_url,
                instance_id.clone(),
                reason,
                reporter.clone(),
            )
            .await?
        }
        CreatePackLocation::FromFile { path } => {
            reporter
                .set_context(
                    InstallErrorContext::new("read local modpack file")
                        .source_path(path.display().to_string())
                        .build(),
                )
                .await?;
            match crate::api::pack::detect::detect_local_pack(&path).await {
                Ok(detected) => {
                    if detected.format
                        != crate::api::pack::detect::LocalPackFormat::Mrpack
                    {
                        // Non-mrpack format — dispatch to format-specific
                        // installer via install_local_pack_file.
                        return install_local_pack_file(
                            detected,
                            path,
                            instance_id,
                            reporter,
                        )
                        .await;
                    }
                    // Mrpack — fall through to standard mrpack install.
                    generate_pack_from_file(path, instance_id.clone()).await?
                }
                Err(detect_error) => {
                    // No format recognised — try recursive extraction
                    // (3-level deep search for sub-archives, bundled
                    // packs, etc.) before giving up.
                    tracing::debug!(
                        "Local pack format detection failed, trying recursive extraction: {detect_error}"
                    );
                    return install_local_pack_file_recursive(
                        path,
                        instance_id,
                        reporter,
                        0,
                        3,
                    )
                    .await;
                }
            }
        }
    };

    let outcome = install_zipped_mrpack_files_with_reporter(
        create_pack,
        false,
        reason,
        reporter,
    )
    .await?;
    Ok(match outcome {
        MrpackInstallOutcome::Completed(_) => {
            InstallExecutionOutcome::Completed(())
        }
        MrpackInstallOutcome::WaitingForUser(reason) => {
            InstallExecutionOutcome::WaitingForUser(reason)
        }
    })
}

/// Recursively tries to detect and install a modpack, up to max_depth levels.
#[async_recursion::async_recursion]
async fn install_local_pack_file_recursive(
    path: PathBuf,
    instance_id: String,
    reporter: InstallProgressReporter,
    current_depth: usize,
    max_depth: usize,
) -> crate::Result<InstallExecutionOutcome<()>> {
    // First try standard detection - this will already include our InstanceFolder fallback
    if let Ok(detected) =
        crate::api::pack::detect::detect_local_pack(&path).await
    {
        // If it's a standard format (including InstanceFolder), just install it
        return install_local_pack_file(detected, path, instance_id, reporter)
            .await;
    }

    // If standard detection failed and we're not at max depth, try to look for
    // sub-compressed files to extract and check
    if current_depth < max_depth {
        // Report progress: scanning/extracting phase (high-latency operation)
        let filename = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "archive".to_string());
        reporter
            .update(
                InstallPhaseId::ResolvingPack,
                None,
                InstallPhaseDetails::Modpack {
                    project_id: None,
                    version_id: None,
                    title: Some(format!("Scanning {filename} (level {current_depth}/{max_depth})")),
                },
            )
            .await?;

        let state = State::get().await?;
        let scratch =
            crate::api::pack::archive_util::create_import_scratch_dir(&state)
                .await?;

        // Extract the entire archive to check for sub-packs
        // First, let's list all entries to find potential sub-compressed files
        let file = std::fs::File::open(&path)?;
        let mut archive = match zip::ZipArchive::new(file) {
            Ok(a) => a,
            Err(_) => {
                // Not a valid zip, can't proceed further
                let _ = tokio::fs::remove_dir_all(&scratch).await;
                return Err(crate::ErrorKind::InputError(
                    "Unrecognized modpack format: no known pack manifest was found in the archive".to_string()
                ).into());
            }
        };

        let mut sub_archive_paths = Vec::new();

        // Collect all potential sub-archive files
        for i in 0..archive.len() {
            let lower_name = {
                let entry = archive
                    .by_index_raw(i)
                    .map_err(|e| ErrorKind::OtherError(e.to_string()))?;
                let name = crate::api::pack::detect::decode_zip_entry_name(
                    entry.name_raw(),
                );
                name.to_lowercase()
            }; // entry dropped here, releasing the mutable borrow on archive

            // Check if it looks like a compressed file
            if lower_name.ends_with(".zip") || lower_name.ends_with(".mrpack") {
                // Extract this sub-archive
                reporter
                    .update(
                        InstallPhaseId::ExtractingOverrides,
                        Some(InstallProgress {
                            current: sub_archive_paths.len() as u64 + 1,
                            total: archive.len() as u64,
                            secondary: None,
                        }),
                        InstallPhaseDetails::Modpack {
                            project_id: None,
                            version_id: None,
                            title: Some(format!("Extracting {filename}")),
                        },
                    )
                    .await?;

                let mut entry = archive
                    .by_index(i)
                    .map_err(|e| ErrorKind::OtherError(e.to_string()))?;
                let sub_path = scratch.join(entry.mangled_name());

                if let Some(parent) = sub_path.parent() {
                    std::fs::create_dir_all(parent)?;
                }

                let mut out = std::fs::File::create(&sub_path)?;
                std::io::copy(&mut entry, &mut out)?;

                sub_archive_paths.push(sub_path);
            }
        }

        // Try to install each sub-archive recursively
        for sub_path in sub_archive_paths {
            let result = install_local_pack_file_recursive(
                sub_path,
                instance_id.clone(),
                reporter.clone(),
                current_depth + 1,
                max_depth,
            )
            .await;

            if result.is_ok() {
                // Success! Clean up and return
                let _ = tokio::fs::remove_dir_all(&scratch).await;
                return result;
            }
        }

        // Clean up scratch directory
        let _ = tokio::fs::remove_dir_all(&scratch).await;
    }

    // If all else fails, return error
    Err(ErrorKind::InputError(
        "Unrecognized modpack format: no known pack manifest was found in the archive"
            .to_string(),
    ).into())
}

/// Dispatches a local non-mrpack modpack file to its format-specific
/// installer, based on the detected pack format.
#[async_recursion::async_recursion]
async fn install_local_pack_file(
    detected: crate::api::pack::detect::DetectedLocalPack,
    path: PathBuf,
    instance_id: String,
    reporter: InstallProgressReporter,
) -> crate::Result<InstallExecutionOutcome<()>> {
    use crate::api::pack::detect::LocalPackFormat;

    let source_filename = path
        .file_name()
        .map(|name| name.to_string_lossy().to_string());
    match detected.format {
        LocalPackFormat::Mrpack => {
            let create_pack =
                generate_pack_from_file(path, instance_id.clone()).await?;
            return Ok(
                match install_zipped_mrpack_files_with_reporter(
                    create_pack,
                    false,
                    DownloadReason::Modpack,
                    reporter,
                )
                .await?
                {
                    MrpackInstallOutcome::Completed(_) => {
                        InstallExecutionOutcome::Completed(())
                    }
                    MrpackInstallOutcome::WaitingForUser(reason) => {
                        InstallExecutionOutcome::WaitingForUser(reason)
                    }
                },
            );
        }
        LocalPackFormat::CurseForge => {
            let skipped_missing_content_paths = reporter
                .current_state()
                .await?
                .skipped_missing_content_paths;
            let result = crate::api::curseforge::install_modpack_from_local_archive_with_reporter(
                instance_id,
                path,
                detected.base_folder,
                source_filename,
                false,
                reporter,
                crate::launcher::InstanceCompletionPolicy::DeferToInstallJob,
            )
            .await?;
            if let Some(reason) = curseforge_manual_download_pause(
                &result,
                &skipped_missing_content_paths,
            ) {
                return Ok(InstallExecutionOutcome::WaitingForUser(reason));
            }
        }
        LocalPackFormat::Mcbbs => {
            crate::api::pack::install_mcbbs::install_mcbbs_pack_with_reporter(
                instance_id,
                path,
                detected.base_folder,
                source_filename,
                reporter,
            )
            .await?;
        }
        LocalPackFormat::Hmcl => {
            crate::api::pack::install_hmcl::install_hmcl_pack_with_reporter(
                instance_id,
                path,
                detected.base_folder,
                source_filename,
                reporter,
            )
            .await?;
        }
        LocalPackFormat::MmcExport => {
            crate::api::pack::install_mmc_zip::install_mmc_zip_with_reporter(
                instance_id,
                path,
                detected.base_folder,
                source_filename,
                reporter,
            )
            .await?;
        }
        LocalPackFormat::LauncherBundled => {
            let inner_entry = detected.inner_pack_entry.ok_or_else(|| {
                ErrorKind::InputError(
                    "Launcher bundle is missing its inner modpack file"
                        .to_string(),
                )
            })?;
            let state = State::get().await?;
            let scratch =
                crate::api::pack::archive_util::create_import_scratch_dir(
                    &state,
                )
                .await?;
            let inner_name = inner_entry
                .rsplit('/')
                .next()
                .unwrap_or("modpack.zip")
                .to_string();
            let inner_path = scratch.join(&inner_name);
            crate::api::pack::archive_util::extract_archive_entry_to_file(
                path,
                inner_entry,
                inner_path.clone(),
            )
            .await?;

            // Use our recursive function to install the inner pack
            let result = install_local_pack_file_recursive(
                inner_path,
                instance_id,
                reporter,
                1, // already one level deep
                3,
            )
            .await;

            // Clean up temporary directory
            if let Err(error) = tokio::fs::remove_dir_all(&scratch).await {
                tracing::warn!(
                    "Failed to clean up modpack import scratch directory {}: {error}",
                    scratch.display()
                );
            }

            return result;
        }
        LocalPackFormat::PlainArchive => {
            let version_id = detected.plain_version_id.ok_or_else(|| {
                ErrorKind::InputError(
                    "Could not locate the instance version in the archive"
                        .to_string(),
                )
            })?;
            crate::api::pack::install_plain_archive::install_plain_archive_with_reporter(
                instance_id,
                path,
                detected.base_folder,
                version_id,
                source_filename,
                reporter,
            )
            .await?;
        }
        LocalPackFormat::InstanceFolder => {
            // Extract the base folder contents to a temporary directory
            let state = State::get().await?;
            let scratch =
                crate::api::pack::archive_util::create_import_scratch_dir(
                    &state,
                )
                .await?;

            // Extract the instance folder contents
            crate::api::pack::archive_util::extract_archive_subdir(
                path,
                detected.base_folder,
                scratch.clone(),
            )
            .await?;

            // Now import it as a generic instance
            let details = InstallPhaseDetails::Modpack {
                project_id: None,
                version_id: None,
                title: source_filename.clone(),
            };

            crate::api::pack::import::generic::import_generic(
                scratch,
                &instance_id,
                reporter,
                details,
                false,
                &crate::api::pack::import::ImportOverrides::default(),
                None, // Not compatible mode
            )
            .await?;
        }
    }
    Ok(InstallExecutionOutcome::Completed(()))
}

fn curseforge_manual_download_pause(
    result: &crate::api::curseforge::CurseForgeModpackInstallResult,
    skipped_missing_content_paths: &[String],
) -> Option<InstallPauseReason> {
    let missing_downloads = result
        .content
        .manual_downloads
        .iter()
        .filter(|download| {
            !skipped_missing_content_paths.contains(&download.file_name)
        })
        .collect::<Vec<_>>();
    if missing_downloads.is_empty() {
        return None;
    }
    Some(InstallPauseReason::MissingRequiredContent {
        failed_files: missing_downloads.len() as u64,
        paths: missing_downloads
            .iter()
            .map(|download| download.file_name.clone())
            .collect(),
    })
}

fn curseforge_world_was_imported_manually(
    job_state: &InstallJobState,
    request: &crate::api::curseforge::CurseForgeWorldInstallRequest,
) -> bool {
    let project_id = request.project_id.to_string();
    let file_id = request.file_id.to_string();
    job_state.download_items().iter().any(|item| {
        item.status == super::model::DownloadItemStatus::Completed
            && item.project_id.as_deref() == Some(project_id.as_str())
            && item.version_id.as_deref() == Some(file_id.as_str())
    })
}

async fn prepare_existing_rollback(
    job_state: &mut InstallJobState,
    state: &State,
    instance_id: &str,
) -> crate::Result<()> {
    if job_state.rollback.is_some() {
        return Ok(());
    }

    let instance = crate::state::get_instance(instance_id, &state.pool)
        .await?
        .ok_or_else(|| {
            crate::ErrorKind::InputError(format!(
                "Unknown instance {instance_id}"
            ))
        })?;
    let install_stage = instance.instance.install_stage;
    set_display(
        job_state,
        instance.instance.name.clone(),
        instance.instance.icon_path.clone(),
    );
    job_state.rollback = Some(InstallRollbackState {
        instance,
        install_stage,
        content: None,
    });
    job_state.cleanup = InstallCleanup::RestoreExistingInstance {
        instance_id: instance_id.to_string(),
    };

    crate::state::instances::commands::set_instance_install_stage(
        instance_id,
        InstanceInstallStage::MinecraftInstalling,
        &state.pool,
    )
    .await?;
    emit_instance(instance_id, InstancePayloadType::Edited).await?;

    Ok(())
}

async fn update_progress(
    job_id: Uuid,
    job_state: &mut InstallJobState,
    state: &State,
    phase: InstallPhaseId,
    details: InstallPhaseDetails,
) -> crate::Result<()> {
    job_state.set_progress(phase, None, details);
    let record = store::update_state(job_id, job_state, state).await?;
    emit_install_job(&record.snapshot()).await?;
    Ok(())
}

fn set_instance_id(job_state: &mut InstallJobState, instance_id: String) {
    job_state.target = match &job_state.target {
        InstallTarget::ExistingInstance { .. } => {
            InstallTarget::ExistingInstance {
                instance_id: instance_id.clone(),
            }
        }
        InstallTarget::NewInstance { .. } => InstallTarget::NewInstance {
            instance_id: Some(instance_id.clone()),
        },
    };
    job_state.cleanup = match &job_state.cleanup {
        InstallCleanup::RestoreExistingInstance { .. } => {
            InstallCleanup::RestoreExistingInstance { instance_id }
        }
        InstallCleanup::DeleteNewInstance { .. } => {
            InstallCleanup::DeleteNewInstance {
                instance_id: Some(instance_id),
            }
        }
        InstallCleanup::None => InstallCleanup::None,
    };
}

fn clear_deleted_new_instance_id(job_state: &mut InstallJobState) {
    if matches!(job_state.cleanup, InstallCleanup::DeleteNewInstance { .. }) {
        job_state.target = InstallTarget::NewInstance { instance_id: None };
        job_state.cleanup =
            InstallCleanup::DeleteNewInstance { instance_id: None };
    }
}

fn set_display(
    job_state: &mut InstallJobState,
    title: String,
    icon: Option<String>,
) {
    job_state.display = Some(InstallJobDisplay { title, icon });
}

fn install_error_view(
    phase: InstallPhaseId,
    error: &crate::Error,
    context: Option<InstallErrorContext>,
) -> InstallErrorView {
    let context = match error.raw.as_ref() {
        ErrorKind::CacheReadError {
            cache_type,
            sqlite_code,
            ..
        } => {
            let mut context = context.unwrap_or_else(|| {
                InstallErrorContext::new("read project metadata cache").build()
            });
            context.cache_types = vec![cache_type.clone()];
            context.sqlite_code = sqlite_code.clone();
            Some(context)
        }
        _ => context,
    };
    InstallErrorView::from_error(
        install_error_code(phase, error),
        phase,
        error,
        context,
    )
}

fn install_error_code(
    phase: InstallPhaseId,
    error: &crate::Error,
) -> &'static str {
    use InstallPhaseId::*;

    match error.raw.as_ref() {
        ErrorKind::CacheReadError { .. } => "cache_repair_required",
        ErrorKind::InputError(msg)
            if msg.starts_with("Unrecognized modpack format")
                && matches!(phase, ResolvingPack) =>
        {
            "unrecognized_format"
        }
        ErrorKind::InputError(_) => match phase {
            PreparingInstance | Finalizing => "instance_error",
            ResolvingPack | DownloadingPackFile | ReadingPackManifest => {
                "pack_error"
            }
            DownloadingContent => "content_error",
            ExtractingOverrides => "path_error",
            PreparingJava => "java_error",
            DownloadingMinecraft => "instance_error",
            RollingBack => "rollback_error",
            ResolvingMinecraft | ResolvingLoader | RunningLoaderProcessors => {
                "launcher_error"
            }
        },
        ErrorKind::LauncherError(_) => match phase {
            RunningLoaderProcessors => "processor_error",
            PreparingJava => "java_error",
            ResolvingLoader => "loader_error",
            _ => "launcher_error",
        },
        ErrorKind::JREError(_) => "java_error",
        ErrorKind::NoValueFor(_) | ErrorKind::MetadataError(_) => match phase {
            ResolvingLoader => "loader_error",
            PreparingJava => "java_error",
            _ => "metadata_error",
        },
        ErrorKind::FetchError(_)
        | ErrorKind::NetworkError(_)
        | ErrorKind::HttpError { .. }
        | ErrorKind::ApiIsDownError(_) => "network_error",
        ErrorKind::Any(_)
            if matches!(
                phase,
                DownloadingPackFile
                    | DownloadingContent
                    | ResolvingMinecraft
                    | ResolvingLoader
                    | PreparingJava
                    | DownloadingMinecraft
            ) =>
        {
            "network_error"
        }
        ErrorKind::LabrinthError(_) => "api_error",
        ErrorKind::HashError(_, _) => "hash_error",
        ErrorKind::ZipError(_) => "archive_error",
        ErrorKind::DeserializationError(_) | ErrorKind::StripPrefixError(_) => {
            "path_error"
        }
        ErrorKind::FSError(_)
        | ErrorKind::IOError(_)
        | ErrorKind::StdIOError(_)
        | ErrorKind::UTFError(_) => "filesystem_error",
        ErrorKind::INIError(_) | ErrorKind::JSONError(_) => "parse_error",
        ErrorKind::Sqlx(_) | ErrorKind::SqlxMigrate(_) => "database_error",
        ErrorKind::JoinError(_)
        | ErrorKind::RecvError(_)
        | ErrorKind::AcquireError(_)
        | ErrorKind::EventError(_) => "internal_error",
        ErrorKind::OtherError(_) | ErrorKind::Any(_) => "internal_error",
        _ => "unknown_error",
    }
}

fn current_instance_id(job_state: &InstallJobState) -> Option<String> {
    match &job_state.target {
        InstallTarget::NewInstance { instance_id } => instance_id.clone(),
        InstallTarget::ExistingInstance { instance_id } => {
            Some(instance_id.clone())
        }
    }
}

pub(crate) const OPTIFABRIC_CURSEFORGE_PROJECT_ID: u32 = 322_385;

async fn resolve_required_adjuncts(
    game_version: &str,
    loader: ModLoader,
    adjuncts: &mut Vec<LoaderComponent>,
    _state: &State,
) -> crate::Result<()> {
    for adjunct in adjuncts.iter() {
        match adjunct.kind {
            LoaderComponentKind::OptiFine
                if !matches!(
                    loader,
                    ModLoader::Forge
                        | ModLoader::NeoForge
                        | ModLoader::Fabric
                        | ModLoader::LegacyFabric
                ) =>
            {
                return Err(ErrorKind::InputError(format!(
                    "OptiFine is not supported with {}",
                    loader.as_str()
                ))
                .into());
            }
            LoaderComponentKind::LiteLoader if loader != ModLoader::Forge => {
                return Err(ErrorKind::InputError(format!(
                    "LiteLoader is not supported with {}",
                    loader.as_str()
                ))
                .into());
            }
            _ => {}
        }
    }
    for adjunct in adjuncts.iter_mut() {
        adjunct.role = LoaderComponentRole::Adjunct;
        adjunct.instance_id.clear();
        match adjunct.kind {
            LoaderComponentKind::OptiFine => {
                let resolved =
					crate::launcher::optifine::resolve_loader_version(
						game_version,
						adjunct.version.as_deref(),
					)
					.await?
					.ok_or_else(|| {
						ErrorKind::InputError(format!(
							"No OptiFine version supports Minecraft {game_version}"
						))
					})?;
                adjunct.version = Some(resolved.id);
            }
            LoaderComponentKind::LiteLoader => {
                let resolved =
					crate::launcher::get_loader_version_from_profile(
						game_version,
						ModLoader::LiteLoader,
						adjunct.version.as_deref(),
					)
					.await?
					.ok_or_else(|| {
						ErrorKind::InputError(format!(
							"No LiteLoader version supports Minecraft {game_version}"
						))
					})?;
                adjunct.version = Some(resolved.id);
            }
            _ => {}
        }
    }
    if adjuncts
        .iter()
        .any(|component| component.kind == LoaderComponentKind::OptiFine)
        && matches!(loader, ModLoader::Fabric | ModLoader::LegacyFabric)
        && !adjuncts
            .iter()
            .any(|component| component.kind == LoaderComponentKind::OptiFabric)
    {
        let version_id = resolve_optifabric_version(game_version).await?;
        adjuncts.push(LoaderComponent {
            instance_id: String::new(),
            kind: LoaderComponentKind::OptiFabric,
            version: Some(version_id),
            role: LoaderComponentRole::Adjunct,
            provider_metadata: Some(serde_json::json!({
                "projectId": OPTIFABRIC_CURSEFORGE_PROJECT_ID,
                "provider": "curseforge"
            })),
        });
    }
    let mut components =
        vec![LoaderComponent::new_primary(String::new(), loader, None)];
    components.extend(adjuncts.iter().cloned());
    validate_loader_components(&components)
}

pub(crate) fn validate_loader_components(
    components: &[LoaderComponent],
) -> crate::Result<()> {
    let primary = components
        .iter()
        .find(|component| component.role == LoaderComponentRole::Primary)
        .ok_or_else(|| {
            ErrorKind::InputError(
                "Loader selection has no primary loader".to_string(),
            )
        })?;
    let has = |kind| {
        components.iter().any(|component| {
            component.role == LoaderComponentRole::Adjunct
                && component.kind == kind
        })
    };
    if has(LoaderComponentKind::OptiFine) {
        match primary.kind {
            LoaderComponentKind::Vanilla => {}
            LoaderComponentKind::Forge | LoaderComponentKind::NeoForge => {}
            LoaderComponentKind::Fabric | LoaderComponentKind::LegacyFabric
                if has(LoaderComponentKind::OptiFabric) => {}
            _ => {
                return Err(ErrorKind::InputError(format!(
                    "OptiFine is not supported with {}",
                    primary.kind.as_str()
                ))
                .into());
            }
        }
    }
    if has(LoaderComponentKind::OptiFabric)
        && !has(LoaderComponentKind::OptiFine)
    {
        return Err(ErrorKind::InputError(
            "OptiFabric can only be installed with OptiFine".to_string(),
        )
        .into());
    }
    if has(LoaderComponentKind::OptiFabric)
        && !matches!(
            primary.kind,
            LoaderComponentKind::Fabric | LoaderComponentKind::LegacyFabric
        )
    {
        return Err(ErrorKind::InputError(format!(
            "OptiFabric is not supported with {}",
            primary.kind.as_str()
        ))
        .into());
    }
    if has(LoaderComponentKind::LiteLoader)
        && !matches!(
            primary.kind,
            LoaderComponentKind::Vanilla | LoaderComponentKind::Forge
        )
    {
        return Err(ErrorKind::InputError(format!(
            "LiteLoader is not supported with {}",
            primary.kind.as_str()
        ))
        .into());
    }
    if components.iter().any(|component| {
        component.role == LoaderComponentRole::Adjunct
            && !matches!(
                component.kind,
                LoaderComponentKind::OptiFine
                    | LoaderComponentKind::LiteLoader
                    | LoaderComponentKind::OptiFabric
            )
    }) {
        return Err(ErrorKind::InputError(
            "Only OptiFine, LiteLoader, and OptiFabric can be adjunct loaders"
                .to_string(),
        )
        .into());
    }
    Ok(())
}

pub(crate) async fn resolve_optifabric_version(
    game_version: &str,
) -> crate::Result<String> {
    let files = crate::api::curseforge::get_files(
        OPTIFABRIC_CURSEFORGE_PROJECT_ID,
        crate::api::curseforge::CurseForgeFilesRequest {
            game_version: None,
            mod_loader_type: None,
            game_version_type_id: None,
            index: 0,
            page_size: 50,
        },
    )
    .await?
    .files;
    select_optifabric_file_id(&files, game_version)
        .map(|file_id| file_id.to_string())
        .ok_or_else(|| {
            ErrorKind::InputError(format!(
                "OptiFine requires OptiFabric, but no OptiFabric version supports Minecraft {game_version}"
            ))
            .into()
        })
}

fn select_optifabric_file_id(
    files: &[crate::api::curseforge::CurseForgeFile],
    game_version: &str,
) -> Option<u32> {
    files
        .iter()
        .find(|file| {
            file.is_available
                && file
                    .game_versions
                    .iter()
                    .any(|version| version == game_version)
        })
        .map(|file| file.id)
}

pub(crate) async fn install_optifabric_file(
    instance_id: &str,
    game_version: &str,
    version: &str,
) -> crate::Result<String> {
    let file_id = version.parse::<u32>().map_err(|_| {
        ErrorKind::InputError(
            "OptiFabric CurseForge file ID is invalid".to_string(),
        )
    })?;
    let file = crate::api::curseforge::get_file(
        OPTIFABRIC_CURSEFORGE_PROJECT_ID,
        file_id,
    )
    .await?;
    if file.mod_id != OPTIFABRIC_CURSEFORGE_PROJECT_ID
        || !file.is_available
        || !file
            .game_versions
            .iter()
            .any(|version| version == game_version)
    {
        return Err(ErrorKind::InputError(format!(
            "OptiFabric file {file_id} does not support Minecraft {game_version}"
        ))
        .into());
    }

    let result = crate::api::curseforge::install_file(
        crate::api::curseforge::CurseForgeInstallRequest {
            instance_id: instance_id.to_string(),
            project_id: OPTIFABRIC_CURSEFORGE_PROJECT_ID,
            file_id,
            project_type: "mod".to_string(),
            ownership_kind: crate::state::instances::ContentOwnershipKind::UserAdded,
            manual_operation_kind:
                crate::state::instances::ManualDownloadOperationKind::ContentInstall,
            game_version: Some(game_version.to_string()),
            mod_loader_type: Some(4),
            world_name: None,
            install_dependencies: false,
            excluded_dependency_project_ids: Vec::new(),
            force_dependency_project_ids: Vec::new(),
            dependency_plan_id: None,
        },
    )
    .await?;
    if !result.manual_downloads.is_empty() {
        return Err(ErrorKind::InputError(
            "OptiFabric requires a manual CurseForge download".to_string(),
        )
        .into());
    }
    if let Some(failure) = result.failed_downloads.first() {
        return Err(ErrorKind::InputError(format!(
            "Failed to install OptiFabric: {}",
            failure.reason
        ))
        .into());
    }
    if !result.installed.iter().any(|installed| {
        !installed.dependency
            && installed.project_id == OPTIFABRIC_CURSEFORGE_PROJECT_ID
            && installed.file_id == file_id
    }) {
        return Err(ErrorKind::InputError(
            "OptiFabric was not installed".to_string(),
        )
        .into());
    }
    Ok(file_id.to_string())
}

async fn install_adjunct_components(
    state: &State,
    instance_id: &str,
    adjuncts: &[LoaderComponent],
    game_version: &str,
    loader: ModLoader,
    cancellation: tokio_util::sync::CancellationToken,
) -> crate::Result<()> {
    if adjuncts.is_empty() {
        return Ok(());
    }
    let metadata = crate::api::instance::get(instance_id)
        .await?
        .ok_or_else(|| ErrorKind::InputError("Unknown instance".to_string()))?;
    let instance_path = state
        .directories
        .instances_dir()
        .join(&metadata.instance.path);
    let mut components = metadata.loader_components.clone();

    for adjunct in adjuncts {
        match adjunct.kind {
            LoaderComponentKind::OptiFine => {
                let version = crate::launcher::optifine::resolve_loader_version(
					game_version,
					adjunct.version.as_deref(),
				)
				.await?
				.ok_or_else(|| {
					ErrorKind::InputError(format!(
						"No OptiFine version supports Minecraft {game_version}"
					))
				})?;
                crate::api::pack::install_mcbbs::install_optifine_mod(
                    state,
                    instance_id,
                    cancellation.clone(),
                    game_version,
                    &version.id,
                    &instance_path,
                )
                .await?;
                set_component_version(
                    &mut components,
                    LoaderComponentKind::OptiFine,
                    version.id,
                );
            }
            LoaderComponentKind::OptiFabric => {
                let version_id = match &adjunct.version {
                    Some(version) => version.clone(),
                    None => resolve_optifabric_version(game_version).await?,
                };
                let version_id = install_optifabric_file(
                    instance_id,
                    game_version,
                    &version_id,
                )
                .await?;
                set_component_version(
                    &mut components,
                    LoaderComponentKind::OptiFabric,
                    version_id,
                );
            }
            LoaderComponentKind::LiteLoader => {
                let version = install_liteloader_adjunct(
                    state,
                    &metadata,
                    game_version,
                    loader,
                    adjunct.version.as_deref(),
                )
                .await?;
                set_component_version(
                    &mut components,
                    LoaderComponentKind::LiteLoader,
                    version,
                );
            }
            _ => {}
        }
    }
    crate::state::instances::commands::replace_instance_loader_components(
        instance_id,
        &components,
        &state.pool,
    )
    .await
}

fn set_component_version(
    components: &mut [LoaderComponent],
    kind: LoaderComponentKind,
    version: String,
) {
    if let Some(component) = components
        .iter_mut()
        .find(|component| component.kind == kind)
    {
        component.version = Some(version);
    }
}

pub(crate) async fn install_liteloader_adjunct(
    state: &State,
    metadata: &crate::state::InstanceMetadata,
    game_version: &str,
    primary_loader: ModLoader,
    requested_version: Option<&str>,
) -> crate::Result<String> {
    let version = crate::launcher::get_loader_version_from_profile(
        game_version,
        ModLoader::LiteLoader,
        requested_version,
    )
    .await?
    .ok_or_else(|| {
        ErrorKind::InputError(format!(
            "No LiteLoader version supports Minecraft {game_version}"
        ))
    })?;
    install_liteloader_adjunct_resolved(
        state,
        metadata,
        game_version,
        primary_loader,
        &version,
    )
    .await
}

pub(crate) async fn install_liteloader_adjunct_resolved(
    state: &State,
    metadata: &crate::state::InstanceMetadata,
    game_version: &str,
    primary_loader: ModLoader,
    version: &daedalus::modded::LoaderVersion,
) -> crate::Result<String> {
    let partial = crate::api::loader_metadata::resolve_loader_profile(
        state,
        game_version,
        version,
    )
    .await?;
    let primary_version = metadata
        .applied_content_set
        .loader_version
        .as_deref()
        .ok_or_else(|| {
            ErrorKind::InputError(format!(
                "{} adjunct installation requires a pinned primary version",
                primary_loader.as_str()
            ))
        })?;
    let version_id = format!("{game_version}-{primary_version}");
    let path = state
        .directories
        .version_dir(&version_id)
        .join(format!("{version_id}.json"));
    let bytes = crate::util::io::read(&path).await?;
    let primary: daedalus::minecraft::VersionInfo =
        serde_json::from_slice(&bytes)?;
    let mut merged = daedalus::modded::merge_partial_version(partial, primary);
    merged.id.clone_from(&version_id);
    crate::launcher::download::download_libraries(
        state,
        None,
        &merged.libraries,
        &version_id,
        None,
        0.0,
        std::env::consts::ARCH,
        false,
        false,
        None,
    )
    .await?;
    crate::util::io::write(&path, serde_json::to_vec(&merged)?).await?;
    Ok(version.id.clone())
}

fn modpack_details(location: &CreatePackLocation) -> InstallPhaseDetails {
    match location {
        CreatePackLocation::FromVersionId {
            project_id,
            version_id,
            title,
            ..
        } => InstallPhaseDetails::Modpack {
            project_id: Some(project_id.clone()),
            version_id: Some(version_id.clone()),
            title: Some(title.clone()),
        },
        CreatePackLocation::FromFile { .. } => InstallPhaseDetails::Modpack {
            project_id: None,
            version_id: None,
            title: None,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn components(
        primary: ModLoader,
        adjuncts: &[LoaderComponentKind],
    ) -> Vec<LoaderComponent> {
        std::iter::once(LoaderComponent::new_primary("", primary, None))
            .chain(adjuncts.iter().map(|kind| LoaderComponent {
                instance_id: String::new(),
                kind: *kind,
                version: None,
                role: LoaderComponentRole::Adjunct,
                provider_metadata: None,
            }))
            .collect()
    }

    fn curseforge_file(
        id: u32,
        is_available: bool,
        game_versions: &[&str],
    ) -> crate::api::curseforge::CurseForgeFile {
        crate::api::curseforge::CurseForgeFile {
            id,
            game_id: 432,
            mod_id: OPTIFABRIC_CURSEFORGE_PROJECT_ID,
            is_available,
            display_name: String::new(),
            file_name: String::new(),
            release_type: 1,
            file_status: 4,
            hashes: Vec::new(),
            file_date: String::new(),
            file_length: 0,
            download_count: 0,
            file_size_on_disk: None,
            download_url: None,
            game_versions: game_versions
                .iter()
                .map(ToString::to_string)
                .collect(),
            sortable_game_versions: Vec::new(),
            dependencies: Vec::new(),
            expose_as_alternative: None,
            parent_project_file_id: None,
            alternate_file_id: None,
            is_server_pack: None,
            server_pack_file_id: None,
            is_early_access_content: None,
            early_access_end_date: None,
            file_fingerprint: 0,
            modules: Vec::new(),
        }
    }

    #[test]
    fn loader_component_preflight_accepts_verified_combinations() {
        for components in [
            components(ModLoader::Vanilla, &[LoaderComponentKind::OptiFine]),
            components(ModLoader::Vanilla, &[LoaderComponentKind::LiteLoader]),
            components(ModLoader::Forge, &[LoaderComponentKind::OptiFine]),
            components(ModLoader::NeoForge, &[LoaderComponentKind::OptiFine]),
            components(ModLoader::Forge, &[LoaderComponentKind::LiteLoader]),
            components(
                ModLoader::Fabric,
                &[
                    LoaderComponentKind::OptiFine,
                    LoaderComponentKind::OptiFabric,
                ],
            ),
        ] {
            validate_loader_components(&components).unwrap();
        }
    }

    #[test]
    fn loader_component_preflight_rejects_unverified_combinations() {
        for components in [
            components(ModLoader::Quilt, &[LoaderComponentKind::OptiFine]),
            components(ModLoader::Cleanroom, &[LoaderComponentKind::OptiFine]),
            components(
                ModLoader::LegacyFabric,
                &[LoaderComponentKind::LiteLoader],
            ),
            components(ModLoader::Fabric, &[LoaderComponentKind::LiteLoader]),
            components(ModLoader::Fabric, &[LoaderComponentKind::OptiFine]),
            components(
                ModLoader::Forge,
                &[
                    LoaderComponentKind::OptiFine,
                    LoaderComponentKind::OptiFabric,
                ],
            ),
        ] {
            assert!(validate_loader_components(&components).is_err());
        }
    }

    #[test]
    fn optifabric_selection_requires_an_available_exact_game_version() {
        let files = vec![
            curseforge_file(1, true, &["1.19.2"]),
            curseforge_file(2, false, &["1.20.1"]),
            curseforge_file(3, true, &["1.20.1"]),
        ];

        assert_eq!(select_optifabric_file_id(&files, "1.20.1"), Some(3));
        assert_eq!(select_optifabric_file_id(&files, "1.20.2"), None);
    }

    #[test]
    fn stalled_downloads_are_reported_as_network_errors() {
        let error: crate::Error = crate::ErrorKind::NetworkError(
            "no data received for 60 seconds".to_string(),
        )
        .into();

        assert_eq!(
            install_error_code(InstallPhaseId::DownloadingMinecraft, &error),
            "network_error"
        );
    }

    #[test]
    fn cache_read_errors_have_repair_context_but_generic_sqlx_does_not() {
        let cache_error: crate::Error = crate::ErrorKind::CacheReadError {
            cache_type: "curseforge_project".to_string(),
            message: "malformed cache row".to_string(),
            sqlite_code: Some("11".to_string()),
        }
        .into();
        let view = install_error_view(
            InstallPhaseId::ResolvingPack,
            &cache_error,
            None,
        );
        assert_eq!(view.code, "cache_repair_required");
        let context = view.context.unwrap();
        assert_eq!(context.cache_types, vec!["curseforge_project"]);
        assert_eq!(context.sqlite_code.as_deref(), Some("11"));

        let database_error: crate::Error =
            crate::ErrorKind::Sqlx(sqlx::Error::RowNotFound).into();
        assert_eq!(
            install_error_code(InstallPhaseId::ResolvingPack, &database_error),
            "database_error"
        );
    }

    #[test]
    fn cache_repair_validation_rejects_old_or_unknown_context() {
        let old_context = InstallErrorContext::new("read cache").build();
        let old_error = InstallErrorView::from_message(
            "cache_repair_required",
            InstallPhaseId::ResolvingPack,
            "cache failed",
        );
        assert!(
            validated_cache_repair_types_for(
                InstallJobStatus::Failed,
                Some(&InstallErrorView {
                    context: Some(old_context),
                    ..old_error.clone()
                }),
            )
            .is_err()
        );

        let mut unknown_context =
            InstallErrorContext::new("read cache").build();
        unknown_context.cache_types = vec!["install_jobs".to_string()];
        assert!(
            validated_cache_repair_types_for(
                InstallJobStatus::Failed,
                Some(&InstallErrorView {
                    context: Some(unknown_context),
                    ..old_error
                }),
            )
            .is_err()
        );
    }

    #[test]
    fn cache_repair_validation_accepts_only_whitelisted_terminal_jobs() {
        let mut context = InstallErrorContext::new("read cache").build();
        context.cache_types = vec![
            "curseforge_project".to_string(),
            "curseforge_project".to_string(),
        ];
        let error = InstallErrorView {
            code: "cache_repair_required".to_string(),
            phase: Some(InstallPhaseId::ResolvingPack),
            message: "cache failed".to_string(),
            api: None,
            context: Some(context),
        };
        assert_eq!(
            validated_cache_repair_types_for(
                InstallJobStatus::Interrupted,
                Some(&error),
            )
            .unwrap(),
            vec![crate::state::CacheValueType::CurseForgeProject]
        );
        assert!(
            validated_cache_repair_types_for(
                InstallJobStatus::Running,
                Some(&error),
            )
            .is_err()
        );
    }

    #[test]
    fn missing_required_content_pauses_without_starting_rollback() {
        let mut job_state =
            InstallJobState::new(InstallRequest::DownloadJava {
                vendor: "test".to_string(),
                version: 21,
            });
        job_state.progress.phase = InstallPhaseId::DownloadingContent;
        job_state.cleanup = InstallCleanup::DeleteNewInstance {
            instance_id: Some("same-instance".to_string()),
        };
        let cleanup = job_state.cleanup.clone();
        let reason = InstallPauseReason::MissingRequiredContent {
            failed_files: 2,
            paths: vec!["mods/a.jar".to_string(), "mods/b.jar".to_string()],
        };

        begin_waiting_for_user(&mut job_state, reason.clone());

        assert_eq!(
            job_state.progress.phase,
            InstallPhaseId::DownloadingContent
        );
        assert_eq!(job_state.pause_reason, Some(reason));
        assert_eq!(job_state.cleanup, cleanup);
        assert!(job_state.error.is_none());
        assert!(job_state.rollback.is_none());
        assert!(job_state.events.iter().any(|event| matches!(
            &event.kind,
            InstallJobEventKind::WaitingForUser { .. }
        )));
        assert!(!job_state.events.iter().any(|event| matches!(
            &event.kind,
            InstallJobEventKind::RollbackStarted { .. }
        )));
    }

    #[test]
    fn curseforge_manual_downloads_create_a_recoverable_pause() {
        let manual_download =
            crate::api::curseforge::CurseForgeManualDownload {
                project_id: 123,
                file_id: 456,
                file_name: "mods/manual.jar".to_string(),
                ownership_kind:
                    crate::state::instances::ContentOwnershipKind::PackManaged,
                operation_kind: crate::state::instances::ManualDownloadOperationKind::PackInstall,
                website_url: Some(
                    "https://www.curseforge.com/minecraft/mc-mods/example/download/456"
                        .to_string(),
                ),
                project_type: "mod".to_string(),
                project_slug: "example".to_string(),
                target_folder: "mods".to_string(),
                hashes: Vec::new(),
                file_length: 12,
                file_fingerprint: 34,
            };
        let result = crate::api::curseforge::CurseForgeModpackInstallResult {
            content: crate::api::curseforge::CurseForgeInstallResult {
                manual_downloads: vec![manual_download],
                ..Default::default()
            },
            ..Default::default()
        };

        assert_eq!(
            curseforge_manual_download_pause(&result, &[]),
            Some(InstallPauseReason::MissingRequiredContent {
                failed_files: 1,
                paths: vec!["mods/manual.jar".to_string()],
            })
        );
        assert_eq!(
            curseforge_manual_download_pause(
                &result,
                &["mods/manual.jar".to_string()],
            ),
            None
        );
    }

    #[test]
    fn recovered_manual_world_download_does_not_run_again() {
        let request = crate::api::curseforge::CurseForgeWorldInstallRequest {
            instance_id: "instance".to_string(),
            project_id: 123,
            file_id: 456,
        };
        let mut job_state =
            InstallJobState::new(InstallRequest::InstallCurseForgeWorld {
                request: request.clone(),
                display_title: "World".to_string(),
                display_icon: None,
            });
        job_state.record_event(InstallJobEventKind::ContentFileSkipped {
            path: "saves/world.zip".to_string(),
            reason: "manual download required".to_string(),
            project_id: Some(request.project_id.to_string()),
            version_id: Some(request.file_id.to_string()),
            manual_url: Some("https://www.curseforge.com/download".to_string()),
        });
        job_state.record_event(InstallJobEventKind::ContentFileRecovered {
            path: "saves/world.zip".to_string(),
            bytes: 42,
        });

        assert!(curseforge_world_was_imported_manually(&job_state, &request));
    }

    #[test]
    fn resume_preserves_instance_cleanup_and_existing_pack_checkpoint() {
        let mut job_state =
            InstallJobState::new(InstallRequest::DownloadJava {
                vendor: "test".to_string(),
                version: 21,
            });
        job_state.target = InstallTarget::NewInstance {
            instance_id: Some("same-instance".to_string()),
        };
        job_state.cleanup = InstallCleanup::DeleteNewInstance {
            instance_id: Some("same-instance".to_string()),
        };
        job_state.continuation =
            Some(InstallContinuationState::InstallingPackToExistingInstance {
                disabled_project_ids: vec!["project-a".to_string()],
            });
        job_state.pause_reason =
            Some(InstallPauseReason::MissingRequiredContent {
                failed_files: 1,
                paths: vec!["mods/a.jar".to_string()],
            });
        let target = job_state.target.clone();
        let cleanup = job_state.cleanup.clone();
        let continuation = job_state.continuation.clone();

        prepare_resumed_job(&mut job_state);

        assert_eq!(job_state.target, target);
        assert_eq!(job_state.cleanup, cleanup);
        assert_eq!(job_state.continuation, continuation);
        assert!(job_state.pause_reason.is_none());
        assert!(job_state.events.iter().any(|event| matches!(
            &event.kind,
            InstallJobEventKind::JobQueued { .. }
        )));
    }

    #[test]
    fn resumed_job_can_pause_again_without_rollback() {
        let mut job_state =
            InstallJobState::new(InstallRequest::DownloadJava {
                vendor: "test".to_string(),
                version: 21,
            });
        job_state.pause_reason =
            Some(InstallPauseReason::MissingRequiredContent {
                failed_files: 1,
                paths: vec!["mods/first.jar".to_string()],
            });
        prepare_resumed_job(&mut job_state);
        begin_waiting_for_user(
            &mut job_state,
            InstallPauseReason::MissingRequiredContent {
                failed_files: 1,
                paths: vec!["mods/still-missing.jar".to_string()],
            },
        );

        assert!(matches!(
            job_state.pause_reason,
            Some(InstallPauseReason::MissingRequiredContent {
                failed_files: 1,
                ..
            })
        ));
        assert!(!job_state.events.iter().any(|event| matches!(
            &event.kind,
            InstallJobEventKind::RollbackStarted { .. }
        )));
    }

    #[test]
    fn canceling_waiting_jobs_keeps_the_original_cleanup_plan() {
        for cleanup in [
            InstallCleanup::DeleteNewInstance {
                instance_id: Some("new-instance".to_string()),
            },
            InstallCleanup::RestoreExistingInstance {
                instance_id: "existing-instance".to_string(),
            },
        ] {
            let mut job_state =
                InstallJobState::new(InstallRequest::DownloadJava {
                    vendor: "test".to_string(),
                    version: 21,
                });
            job_state.cleanup = cleanup.clone();
            job_state.pause_reason =
                Some(InstallPauseReason::MissingRequiredContent {
                    failed_files: 1,
                    paths: vec!["mods/missing.jar".to_string()],
                });

            begin_canceling_job(&mut job_state);

            assert_eq!(job_state.cleanup, cleanup);
            assert!(job_state.pause_reason.is_none());
            assert_eq!(job_state.progress.phase, InstallPhaseId::RollingBack);
            assert!(job_state.events.iter().any(|event| matches!(
                &event.kind,
                InstallJobEventKind::RollbackStarted {
                    cleanup: event_cleanup,
                } if event_cleanup == &cleanup
            )));
        }
    }

    #[test]
    fn manifest_and_override_errors_remain_fatal() {
        for (phase, message) in [
            (
                InstallPhaseId::ReadingPackManifest,
                "No pack manifest found in mrpack",
            ),
            (InstallPhaseId::ExtractingOverrides, "Invalid override path"),
        ] {
            let mut job_state =
                InstallJobState::new(InstallRequest::DownloadJava {
                    vendor: "test".to_string(),
                    version: 21,
                });
            job_state.progress.phase = phase;
            let error: crate::Error =
                crate::ErrorKind::InputError(message.to_string()).into();

            begin_failed_job_rollback(&mut job_state, &error);

            assert!(job_state.pause_reason.is_none());
            assert_eq!(job_state.progress.phase, InstallPhaseId::RollingBack);
            assert!(job_state.events.iter().any(|event| matches!(
                &event.kind,
                InstallJobEventKind::Failed {
                    phase: failed_phase,
                    ..
                } if *failed_phase == phase
            )));
            assert!(job_state.events.iter().any(|event| matches!(
                &event.kind,
                InstallJobEventKind::RollbackStarted { .. }
            )));
        }
    }

    /// The launcher state is a process-wide singleton; initialize it once and
    /// reuse it so `State::get()` resolves inside these APIs. The state root
    /// is intentionally leaked (`.keep()`) because the shared state outlives
    /// this function.
    async fn global_state() -> std::sync::Arc<State> {
        if !State::initialized() {
            let root = tempfile::tempdir().unwrap().keep();
            let _ =
                State::init_for_test(root.to_string_lossy().to_string()).await;
        }
        State::get().await.unwrap()
    }

    #[tokio::test]
    async fn direct_link_instances_cannot_be_duplicated() {
        let state = global_state().await;
        let minecraft = tempfile::tempdir().unwrap();
        let version_dir = minecraft.path().join("versions/duplicate-demo");
        std::fs::create_dir_all(&version_dir).unwrap();
        std::fs::write(
            version_dir.join("duplicate-demo.json"),
            serde_json::to_vec_pretty(&serde_json::json!({
                "id": "duplicate-demo",
                "inheritsFrom": "1.20.1",
                "mainClass": "net.minecraft.client.main.Main"
            }))
            .unwrap(),
        )
        .unwrap();

        let instance = crate::state::create_direct_link_instance(
            crate::state::CreateDirectLinkInstance {
                name: None,
                launcher_type:
                    crate::api::pack::import::ImportLauncherType::Generic,
                base_path: minecraft.path().to_path_buf(),
                instance_folder: "versions/duplicate-demo".to_string(),
                instance_path: None,
            },
            &state,
        )
        .await
        .unwrap();

        let error = duplicate_instance(instance.id)
            .await
            .expect_err("directly associated instances must be rejected");

        assert!(
            error.to_string().contains("directly associated"),
            "expected a friendly rejection, got: {error}"
        );
    }
}
