//! `guild install <offer> <path>` and `guild import <offer> <path>`.
//!
//! Install downloads the live build into `path`, then runs the game's touchup
//! through one UAC prompt. Running it again on the same path is the update:
//! files whose size already matches are skipped. Import only runs touchup on a
//! folder that already holds the game.

use std::{
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use maxima::{
    content::manager::QueuedGameBuilder,
    core::{
        manifest::{self, ManifestError, MANIFEST_RELATIVE_PATH},
        LockedMaxima, MaximaEvent,
    },
    util::native::NativeError,
};
use serde::Serialize;

use super::out::{emit, fail, scrub, ErrorCode};

/// How often progress is checked and, at most, printed.
const POLL_INTERVAL: Duration = Duration::from_millis(500);
/// No new bytes for this long before touchup starts counts as a dead download.
const STALL_TIMEOUT: Duration = Duration::from_secs(120);
/// How long to wait for the download task to stop after a cancel.
const CANCEL_GRACE: Duration = Duration::from_secs(5);

#[derive(Serialize, Debug, Clone, Copy, PartialEq, Eq)]
struct Progress {
    bytes: u64,
    total: u64,
}

#[derive(Serialize)]
struct ProgressEvent {
    progress: Progress,
}

#[derive(Serialize)]
struct DoneEvent {
    done: bool,
}

/// The install folder must be an absolute path; anything else would land
/// relative to wherever the caller happened to start us.
pub fn validate_install_path(path: &str) -> Result<PathBuf, ErrorCode> {
    let path = PathBuf::from(path);
    if path.as_os_str().is_empty() || !path.is_absolute() {
        return Err(ErrorCode::Path);
    }
    Ok(path)
}

/// Maps a `GameDownloader::error()` kind to the error printed on stdout.
fn failure_code(kind: Option<&str>) -> ErrorCode {
    match kind {
        Some("touchup") => ErrorCode::Touchup,
        Some("elevation_declined") => ErrorCode::ElevationDeclined,
        // "file", "manifest", "cancelled", or a failure without a stored kind
        _ => ErrorCode::Download,
    }
}

fn touchup_failure_code(err: &ManifestError) -> ErrorCode {
    match err {
        ManifestError::Native(NativeError::Elevation(_)) => ErrorCode::ElevationDeclined,
        _ => ErrorCode::Touchup,
    }
}

/// Bytes of the files only. The downloader's total counts one extra byte that
/// is added when touchup finishes; leaving it out makes the download read 100%
/// while touchup runs instead of hanging just below it.
fn file_progress(bytes: usize, total: usize) -> Progress {
    let total = total.saturating_sub(1);
    Progress {
        bytes: bytes.min(total) as u64,
        total: total as u64,
    }
}

/// Fails with `not_owned` unless the account owns `offer_id`.
async fn check_owned(maxima_arc: &LockedMaxima, offer_id: &str) -> Result<(), i32> {
    let mut maxima = maxima_arc.lock().await;
    match maxima.mut_library().game_by_base_offer(offer_id).await {
        Ok(Some(_)) => Ok(()),
        Ok(None) => Err(fail(ErrorCode::NotOwned)),
        Err(err) => {
            log::error!("Failed to read the library: {}", scrub(&err.to_string()));
            Err(fail(ErrorCode::Internal))
        }
    }
}

pub async fn install(offer_id: &str, path: &str) -> i32 {
    // Before anything touches the network or the download queue.
    let path = match validate_install_path(path) {
        Ok(path) => path,
        Err(code) => return fail(code),
    };

    let maxima_arc = match super::logged_in_maxima().await {
        Ok(maxima) => maxima,
        Err(code) => return code,
    };
    if let Err(code) = check_owned(&maxima_arc, offer_id).await {
        return code;
    }

    let mut maxima = maxima_arc.lock().await;

    let builds = match maxima
        .content_manager()
        .service()
        .available_builds(offer_id)
        .await
    {
        Ok(builds) => builds,
        Err(err) => {
            log::error!("Failed to get builds: {}", scrub(&err.to_string()));
            return fail(ErrorCode::NoBuild);
        }
    };
    let Some(build) = builds.live_build() else {
        return fail(ErrorCode::NoBuild);
    };
    log::info!(
        "Installing build {} into {}",
        build.build_id(),
        path.display()
    );

    let game = match QueuedGameBuilder::default()
        .offer_id(offer_id.to_owned())
        .build_id(build.build_id().to_owned())
        .path(path)
        .build()
    {
        Ok(game) => game,
        Err(err) => {
            log::error!("Failed to queue the install: {}", err);
            return fail(ErrorCode::Internal);
        }
    };

    if let Err(err) = maxima.content_manager().install_now(game).await {
        log::error!("Failed to start the download: {}", scrub(&err.to_string()));
        return fail(ErrorCode::Download);
    }
    drop(maxima);

    watch_download(&maxima_arc, offer_id).await
}

enum Poll {
    Running(Progress),
    Finished,
    Failed(ErrorCode),
}

async fn poll(maxima_arc: &LockedMaxima, offer_id: &str) -> Poll {
    let mut maxima = maxima_arc.lock().await;
    maxima.update().await;

    for event in maxima.consume_pending_events() {
        match event {
            MaximaEvent::InstallFinished(id) if id == offer_id => return Poll::Finished,
            MaximaEvent::InstallFailed(id) if id == offer_id => {
                let kind = maxima.content_manager().last_error().clone();
                log::error!("Install failed: {}", kind.as_deref().unwrap_or("unknown"));
                return Poll::Failed(failure_code(kind.as_deref()));
            }
            _ => {}
        }
    }

    match maxima.content_manager().current() {
        Some(downloader) if downloader.offer_id() == offer_id => Poll::Running(file_progress(
            downloader.bytes_downloaded(),
            downloader.bytes_total(),
        )),
        _ => {
            // Our download is gone without an event saying how it ended.
            log::error!("Download of {} stopped without a result", offer_id);
            Poll::Failed(ErrorCode::Download)
        }
    }
}

async fn watch_download(maxima_arc: &LockedMaxima, offer_id: &str) -> i32 {
    let mut last_emitted: Option<Progress> = None;
    let mut last_emit_at: Option<Instant> = None;
    let mut last_bytes = 0;
    let mut last_bytes_at = Instant::now();

    loop {
        let progress = match poll(maxima_arc, offer_id).await {
            Poll::Running(progress) => progress,
            Poll::Finished => {
                emit(&DoneEvent { done: true });
                return 0;
            }
            Poll::Failed(code) => return fail(code),
        };

        let now = Instant::now();
        if progress.bytes != last_bytes {
            last_bytes = progress.bytes;
            last_bytes_at = now;
        }

        let due = last_emit_at.map_or(true, |at| now - at >= POLL_INTERVAL);
        if due && last_emitted != Some(progress) {
            emit(&ProgressEvent { progress });
            last_emitted = Some(progress);
            last_emit_at = Some(now);
        }

        // Once every file is in, touchup runs (and may sit at a UAC prompt), so
        // a still byte count is expected from then on.
        let touchup_started = progress.bytes >= progress.total;
        if !touchup_started && now - last_bytes_at >= STALL_TIMEOUT {
            log::error!(
                "No download progress for {} seconds, cancelling",
                STALL_TIMEOUT.as_secs()
            );
            cancel(maxima_arc, offer_id).await;
            return fail(ErrorCode::Download);
        }

        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

/// Cancels the download and waits briefly for it to stop, so the content
/// manager clears it from the saved queue and a later run doesn't pick it up.
async fn cancel(maxima_arc: &LockedMaxima, offer_id: &str) {
    {
        let mut maxima = maxima_arc.lock().await;
        if let Some(downloader) = maxima.content_manager().current() {
            downloader.cancel();
        }
    }

    let deadline = Instant::now() + CANCEL_GRACE;
    while Instant::now() < deadline {
        match poll(maxima_arc, offer_id).await {
            Poll::Running(_) => tokio::time::sleep(Duration::from_millis(100)).await,
            _ => return,
        }
    }
    log::warn!(
        "Download did not stop within {} seconds",
        CANCEL_GRACE.as_secs()
    );
}

pub async fn import(offer_id: &str, path: &str) -> i32 {
    let path = match validate_install_path(path) {
        Ok(path) => path,
        Err(code) => return fail(code),
    };

    let maxima_arc = match super::logged_in_maxima().await {
        Ok(maxima) => maxima,
        Err(code) => return code,
    };
    if let Err(code) = check_owned(&maxima_arc, offer_id).await {
        return code;
    }

    run_import_touchup(&path).await
}

async fn run_import_touchup(path: &Path) -> i32 {
    let manifest_path = path.join(MANIFEST_RELATIVE_PATH);
    if !manifest_path.is_file() {
        log::error!("No install manifest at {}", manifest_path.display());
        return fail(ErrorCode::Path);
    }
    let manifest = match manifest::read(manifest_path).await {
        Ok(manifest) => manifest,
        Err(err) => {
            log::error!("Failed to read the install manifest: {}", err);
            return fail(ErrorCode::Path);
        }
    };

    log::info!("Running touchup for {}", path.display());
    if let Err(err) = manifest.run_touchup(&path.to_path_buf()).await {
        log::error!("Touchup failed: {}", err);
        return fail(touchup_failure_code(&err));
    }

    emit(&DoneEvent { done: true });
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_event_shape() {
        let event = ProgressEvent {
            progress: Progress {
                bytes: 5,
                total: 10,
            },
        };
        assert_eq!(
            serde_json::to_string(&event).unwrap(),
            r#"{"progress":{"bytes":5,"total":10}}"#
        );
    }

    #[test]
    fn relative_path_rejected() {
        assert_eq!(validate_install_path("Games\\EA"), Err(ErrorCode::Path));
        assert_eq!(validate_install_path(""), Err(ErrorCode::Path));
        #[cfg(windows)]
        {
            assert_eq!(validate_install_path("\\Games\\EA"), Err(ErrorCode::Path));
            assert!(validate_install_path("C:\\Games\\EA").is_ok());
        }
        #[cfg(unix)]
        assert!(validate_install_path("/games/ea").is_ok());
    }

    #[test]
    fn failure_kinds_map_to_codes() {
        assert_eq!(failure_code(Some("file")), ErrorCode::Download);
        assert_eq!(failure_code(Some("manifest")), ErrorCode::Download);
        assert_eq!(failure_code(Some("cancelled")), ErrorCode::Download);
        assert_eq!(failure_code(None), ErrorCode::Download);
        assert_eq!(failure_code(Some("touchup")), ErrorCode::Touchup);
        assert_eq!(
            failure_code(Some("elevation_declined")),
            ErrorCode::ElevationDeclined
        );
    }

    #[test]
    fn declined_prompt_on_import_is_its_own_error() {
        let declined = ManifestError::Native(NativeError::Elevation("touchup.exe".to_owned()));
        assert_eq!(
            touchup_failure_code(&declined),
            ErrorCode::ElevationDeclined
        );
        let failed = ManifestError::Native(NativeError::Command(3));
        assert_eq!(touchup_failure_code(&failed), ErrorCode::Touchup);
    }

    #[test]
    fn progress_leaves_out_the_touchup_byte() {
        assert_eq!(
            file_progress(0, 101),
            Progress {
                bytes: 0,
                total: 100
            }
        );
        assert_eq!(
            file_progress(101, 101),
            Progress {
                bytes: 100,
                total: 100
            }
        );
        assert_eq!(file_progress(0, 0), Progress { bytes: 0, total: 0 });
    }
}
