//! `guild library` and `guild info <offer>`.

use std::path::PathBuf;

use lazy_static::lazy_static;
use log::error;
use maxima::{
    content::zip::ZipFile, core::library::OwnedOffer, util::registry::parse_partial_registry_path,
};
use regex::Regex;
use serde::Serialize;

use super::out::{emit, fail, scrub, ErrorCode};

lazy_static! {
    static ref URL: Regex = Regex::new(r"https?://\S+").unwrap();
}

/// Error text with every URL removed: the build's download URL is signed, and
/// reqwest puts it in its errors.
fn without_urls(text: &str) -> String {
    scrub(&URL.replace_all(text, "<url>"))
}

#[derive(Serialize, Debug)]
pub struct Game {
    pub offer_id: String,
    pub content_id: String,
    pub slug: String,
    pub name: String,
    pub installed: bool,
    pub install_path: Option<String>,
    pub installed_version: Option<String>,
    pub latest_version: Option<String>,
    pub has_cloud_save: bool,
}

#[derive(Serialize, Debug)]
#[serde(rename_all = "snake_case")]
pub enum LibraryEvent {
    Game(Game),
    Done(bool),
}

#[derive(Serialize, Debug)]
pub struct BuildInfo {
    pub build_id: String,
    pub version: Option<String>,
    pub download_bytes: u64,
    pub install_bytes: u64,
}

#[derive(Serialize, Debug)]
#[serde(rename_all = "snake_case")]
pub enum InfoEvent {
    Info(BuildInfo),
}

/// The install folder of an offer, from its install check (`[HKLM-key\Value]rest`
/// resolves to the registry value). Logs why when it can't be resolved.
pub(super) async fn install_dir(offer: &OwnedOffer) -> Option<PathBuf> {
    let Some(check) = offer.offer().install_check_override() else {
        error!("{} has no install check", offer.offer_id());
        return None;
    };
    match parse_partial_registry_path(check).await {
        // The registry value comes back with `/`; rebuild it with native separators.
        Ok(dir) => Some(dir.components().collect()),
        Err(err) => {
            error!(
                "Failed to resolve the install folder of {}: {}",
                offer.offer_id(),
                scrub(&err.to_string())
            );
            None
        }
    }
}

async fn game_event(name: String, offer: &OwnedOffer) -> Game {
    let installed = offer.is_installed().await;
    let (install_path, installed_version) = if installed {
        (
            install_dir(offer)
                .await
                .map(|dir| dir.to_string_lossy().into_owned()),
            offer.installed_version().await.ok(),
        )
    } else {
        (None, None)
    };
    let legacy = offer.offer();
    Game {
        offer_id: offer.offer_id().clone(),
        content_id: legacy.content_id().clone(),
        slug: offer.slug().clone(),
        name,
        installed,
        install_path,
        installed_version,
        latest_version: legacy
            .downloads()
            .iter()
            .find(|d| d.download_type() == "LIVE")
            .map(|d| d.version().clone()),
        has_cloud_save: legacy.has_cloud_save(),
    }
}

/// `guild library`: one `{"game":…}` line per owned title's base offer, then `{"done":true}`.
pub async fn library() -> i32 {
    let maxima_arc = match super::logged_in_maxima().await {
        Ok(maxima) => maxima,
        Err(code) => return code,
    };
    let mut maxima = maxima_arc.lock().await;
    let titles: Vec<(String, OwnedOffer)> = match maxima.mut_library().games().await {
        Ok(titles) => titles
            .iter()
            .map(|t| (t.name(), t.base_offer().clone()))
            .collect(),
        Err(err) => {
            error!("Failed to read the library: {}", scrub(&err.to_string()));
            return fail(ErrorCode::Internal);
        }
    };
    drop(maxima);

    for (name, offer) in &titles {
        emit(&LibraryEvent::Game(game_event(name.clone(), offer).await));
    }
    emit(&LibraryEvent::Done(true));
    0
}

/// `guild info <offer>`: the live build's id, version and sizes.
pub async fn info(offer_id: &str) -> i32 {
    let maxima_arc = match super::logged_in_maxima().await {
        Ok(maxima) => maxima,
        Err(code) => return code,
    };
    let mut maxima = maxima_arc.lock().await;

    match maxima.mut_library().game_by_base_offer(offer_id).await {
        Ok(Some(_)) => {}
        Ok(None) => return fail(ErrorCode::NotOwned),
        Err(err) => {
            error!("Failed to read the library: {}", scrub(&err.to_string()));
            return fail(ErrorCode::Internal);
        }
    }

    let service = maxima.content_manager().service();
    let builds = match service.available_builds(offer_id).await {
        Ok(builds) => builds,
        Err(err) => {
            error!("Failed to list builds: {}", without_urls(&err.to_string()));
            return fail(ErrorCode::Internal);
        }
    };
    let Some(build) = builds.live_build().cloned() else {
        return fail(ErrorCode::NoBuild);
    };
    // Never log or print this URL: it is signed.
    let url = match service.download_url(offer_id, Some(build.build_id())).await {
        Ok(meta) => meta.url().clone(),
        Err(err) => {
            error!(
                "Failed to get the build's download address: {}",
                without_urls(&err.to_string())
            );
            return fail(ErrorCode::Internal);
        }
    };
    drop(maxima);

    let zip = match ZipFile::fetch(&url).await {
        Ok(zip) => zip,
        Err(err) => {
            error!(
                "Failed to read the build's file list: {}",
                without_urls(&err.to_string())
            );
            return fail(ErrorCode::Internal);
        }
    };
    let (download_bytes, install_bytes) = zip.entries().iter().fold((0u64, 0u64), |(d, i), e| {
        (
            d + (*e.compressed_size()).max(0) as u64,
            i + (*e.uncompressed_size()).max(0) as u64,
        )
    });

    emit(&InfoEvent::Info(BuildInfo {
        build_id: build.build_id().clone(),
        version: build.game_version().clone(),
        download_bytes,
        install_bytes,
    }));
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn game_event_shape() {
        let event = LibraryEvent::Game(Game {
            offer_id: "Origin.OFR.50.0005198".into(),
            content_id: "1".into(),
            slug: "zau".into(),
            name: "ZAU".into(),
            installed: false,
            install_path: None,
            installed_version: None,
            latest_version: Some("1.0".into()),
            has_cloud_save: true,
        });
        assert_eq!(
            serde_json::to_string(&event).unwrap(),
            r#"{"game":{"offer_id":"Origin.OFR.50.0005198","content_id":"1","slug":"zau","name":"ZAU","installed":false,"install_path":null,"installed_version":null,"latest_version":"1.0","has_cloud_save":true}}"#
        );
        assert_eq!(
            serde_json::to_string(&LibraryEvent::Done(true)).unwrap(),
            r#"{"done":true}"#
        );
        assert_eq!(
            serde_json::to_string(&InfoEvent::Info(BuildInfo {
                build_id: "b".into(),
                version: Some("1.0".into()),
                download_bytes: 10,
                install_bytes: 20,
            }))
            .unwrap(),
            r#"{"info":{"build_id":"b","version":"1.0","download_bytes":10,"install_bytes":20}}"#
        );
    }
}
