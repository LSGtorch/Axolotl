use crate::State;
use crate::state::{CacheBehaviour, CachedEntry};
pub use daedalus::minecraft::VersionManifest;
pub use daedalus::modded::Manifest;

#[tracing::instrument]
pub async fn get_minecraft_versions() -> crate::Result<VersionManifest> {
    get_minecraft_versions_with_cache(None).await
}

pub async fn get_minecraft_versions_with_cache(
    cache_behaviour: Option<CacheBehaviour>,
) -> crate::Result<VersionManifest> {
    let state = State::get().await?;
    let minecraft_versions = CachedEntry::get_minecraft_manifest(
        cache_behaviour,
        &state.pool,
        &state.api_semaphore,
    )
    .await?
    .ok_or_else(|| {
        crate::ErrorKind::NoValueFor("minecraft versions".to_string())
    })?;

    Ok(minecraft_versions)
}

// #[tracing::instrument]
pub async fn get_loader_versions(loader: &str) -> crate::Result<Manifest> {
    get_loader_versions_with_cache(loader, None).await
}

pub async fn get_loader_versions_with_cache(
    loader: &str,
    cache_behaviour: Option<CacheBehaviour>,
) -> crate::Result<Manifest> {
    if loader == "cleanroom" {
        return Ok(Manifest {
            game_versions: vec![daedalus::modded::Version {
                id: "1.12.2".to_string(),
                stable: true,
                version_group: None,
                loaders: vec![daedalus::modded::LoaderVersion {
                    id: "latest".to_string(),
                    url: String::new(),
                    stable: true,
                }],
            }],
            version_groups: Vec::new(),
        });
    }

    let state = State::get().await?;
    let cache_key =
        daedalus::modded::loader_manifest_metadata(loader).cache_key;
    let loaders = CachedEntry::get_loader_manifest(
        &cache_key,
        cache_behaviour,
        &state.pool,
        &state.api_semaphore,
    )
    .await?
    .ok_or_else(|| {
        crate::ErrorKind::NoValueFor(format!("{loader} loader versions"))
    })?;

    Ok(loaders.manifest)
}
