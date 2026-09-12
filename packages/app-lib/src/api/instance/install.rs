use crate::state::{JavaVersion, State};

pub async fn get_optimal_jre_key(
    instance_id: &str,
) -> crate::Result<Option<JavaVersion>> {
    let state = State::get().await?;
    let context =
        crate::state::instances::commands::get_instance_launch_context(
            instance_id,
            &state.pool,
        )
        .await?
        .ok_or_else(|| {
            crate::ErrorKind::OtherError(format!(
                "Tried to resolve a nonexistent instance {instance_id}!"
            ))
        })?;

    // Directly associated instances already carry the full local version
    // JSON chain (HMCL/PCL patches included). Resolve the Java requirement
    // from that local metadata instead of downloading a version manifest —
    // the manifest fetch goes through launcher-specific loader channels
    // (e.g. Cleanroom) and can stall for a long time, which used to freeze
    // the instance Java settings page while it waited.
    if context.instance.is_direct_linked()
        && let Ok(Some(direct)) =
            crate::launcher::DirectLinkedLaunch::from_instance(
                &context.instance,
            )
        && let Ok(resolved) = direct.resolve()
        && let Ok(version_info) =
            crate::launcher::merged_to_version_info(resolved.merged)
    {
        let major_version = version_info
            .java_version
            .as_ref()
            .map_or(8, |java| java.major_version);
        return crate::api::jre::find_java_for_version(major_version).await;
    }

    let (minecraft, version_index) =
        crate::launcher::resolve_minecraft_manifest(
            &context.applied_content_set.game_version,
            &state,
        )
        .await?;
    let version = &minecraft.versions[version_index];
    let loader_version = crate::launcher::get_loader_version_from_profile(
        &context.applied_content_set.game_version,
        context.applied_content_set.loader,
        context.applied_content_set.loader_version.as_deref(),
    )
    .await?;
    let version_info = crate::launcher::download::download_version_info(
        &state,
        version,
        context.applied_content_set.loader,
        loader_version.as_ref(),
        None,
        None,
        None,
    )
    .await?;

    let major_version = version_info
        .java_version
        .as_ref()
        .map_or(8, |java| java.major_version);

    crate::api::jre::find_java_for_version(major_version).await
}
