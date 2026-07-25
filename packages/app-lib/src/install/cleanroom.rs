// ghfast.top mirror for GitHub download acceleration.
use crate::state::{InstanceLaunchContext, State};
use crate::util::{
	fetch::{download_to_path, DownloadRequest, ResourceClass},
	io,
};
use daedalus::minecraft::Library;
use daedalus::modded::PartialVersionInfo;
use serde::Deserialize;
use std::io::{Cursor, Read};

const FUGUE_URL: &str = "https://github.com/CleanroomMC/Fugue/releases/download/0.23.8/%2BFugue-0.23.8-dev.jar";
const SCALAR_URL: &str = "https://github.com/CleanroomMC/Scalar/releases/download/2.11.1/scalar-1.12.2-2.11.1.jar";
const CLEANROOM_GITHUB_API: &str = "https://api.github.com/repos/CleanroomMC/Cleanroom/releases/latest";
const CLEANROOM_GITHUB_MIRROR: &str = "https://ghfast.top/";

#[derive(Deserialize)]
struct Release {
    assets: Vec<ReleaseAsset>,
}

#[derive(Deserialize)]
struct ReleaseAsset {
    name: String,
    browser_download_url: String,
}

#[derive(Deserialize)]
struct InstallProfile {
    libraries: Vec<Library>,
}

pub async fn install(context: &InstanceLaunchContext) -> crate::Result<()> {
	let state = State::get().await?;
	let cache_dir = state.directories.caches_dir().join("cleanroom");
	let release_path = cache_dir.join("latest-release.json");
	download_to_path(
		cleanroom_download_request(CLEANROOM_GITHUB_API),
		&release_path,
		&state.download_semaphore,
		&state.pool,
		None,
	)
	.await?;
	let release: Release = serde_json::from_slice(&io::read(&release_path).await?)?;
	let installer = release
        .assets
        .into_iter()
        .find(|asset| asset.name.ends_with("-installer.jar"))
        .ok_or_else(|| {
            crate::ErrorKind::LauncherError(
                "Cleanroom's latest release has no installer JAR".to_string(),
            )
        })?;
	let installer_path = cache_dir.join("installer.jar");
	download_to_path(
		cleanroom_download_request(&installer.browser_download_url),
		&installer_path,
		&state.download_semaphore,
		&state.pool,
		None,
	)
	.await?;
	let installer = read_installer(&io::read(&installer_path).await?)?;

    let version_dir = state.directories.version_dir(&installer.version.id);
    io::create_dir_all(&version_dir).await?;
    io::write(
        version_dir.join(format!("{}.json", installer.version.id)),
        serde_json::to_vec(&installer.version)?,
    )
    .await?;

    for (path, contents) in installer.libraries {
        let path = state.directories.libraries_dir().join(path);
        if let Some(parent) = path.parent() {
            io::create_dir_all(parent).await?;
        }
        io::write(path, contents).await?;
    }

    let mods_dir = state
        .directories
        .instances_dir()
        .join(&context.instance.path)
        .join("mods");
    io::create_dir_all(&mods_dir).await?;
	for (url, file_name) in [
		(FUGUE_URL, "+Fugue-0.23.8-dev.jar"),
		(SCALAR_URL, "scalar-1.12.2-2.11.1.jar"),
	] {
		download_to_path(
			cleanroom_download_request(url),
			mods_dir.join(file_name),
			&state.download_semaphore,
			&state.pool,
			None,
		)
		.await?;
	}

	Ok(())
}

fn cleanroom_download_request(url: &str) -> DownloadRequest {
	DownloadRequest::new(
		format!("{CLEANROOM_GITHUB_MIRROR}{url}"),
		ResourceClass::Other,
	)
	.with_candidate_urls([url])
}

struct InstallerContents {
    version: PartialVersionInfo,
    libraries: Vec<(String, Vec<u8>)>,
}

fn read_installer(installer: &[u8]) -> crate::Result<InstallerContents> {
    let mut archive = zip::ZipArchive::new(Cursor::new(installer)).map_err(|error| {
        crate::ErrorKind::LauncherError(format!(
            "Failed to open Cleanroom installer archive: {error}"
        ))
    })?;
    let profile: InstallProfile = read_json(&mut archive, "install_profile.json")?;
    let mut version: PartialVersionInfo = read_json(&mut archive, "version.json")?;
    version.libraries.extend(profile.libraries);

    let mut libraries = Vec::new();
    for index in 0..archive.len() {
        let mut entry = archive.by_index(index).map_err(installer_archive_error)?;
        let Some(path) = entry.enclosed_name().map(|path| path.to_owned()) else {
            continue;
        };
        let path = match path.strip_prefix("maven/") {
            Ok(p) => p,
            Err(_) => continue,
        };
        if entry.is_dir() {
            continue;
        }
        let mut contents = Vec::new();
        entry.read_to_end(&mut contents)?;
        libraries.push((path.to_string_lossy().to_string(), contents));
    }

    Ok(InstallerContents { version, libraries })
}

fn read_json<T: serde::de::DeserializeOwned>(
    archive: &mut zip::ZipArchive<Cursor<&[u8]>>,
    name: &str,
) -> crate::Result<T> {
    let mut entry = archive.by_name(name).map_err(|error| match error {
        zip::result::ZipError::FileNotFound => crate::ErrorKind::LauncherError(format!(
            "Cleanroom installer is missing {name}"
        )),
        error => installer_archive_error(error),
    })?;
    let mut contents = Vec::new();
    entry.read_to_end(&mut contents)?;
    Ok(serde_json::from_slice(&contents)?)
}

fn installer_archive_error(error: zip::result::ZipError) -> crate::ErrorKind {
    crate::ErrorKind::LauncherError(format!(
        "Failed to read Cleanroom installer archive: {error}"
    ))
}
