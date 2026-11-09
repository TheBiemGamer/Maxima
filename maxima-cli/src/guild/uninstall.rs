//! `guild uninstall <offer>`: deletes the offer's install folder.
//!
//! The registry values the game's touchup wrote stay (they are under HKLM and
//! need elevation); `is_installed()` reads false once the folder is gone.

use std::{
    env,
    path::{Component, Path, PathBuf},
};

use maxima::{
    core::{LockedMaxima, Maxima, MaximaOptionsBuilder},
    util::{native::maxima_dir, registry::parse_partial_registry_path},
};
use serde::Serialize;

use super::out::{emit, fail, scrub, ErrorCode};

#[derive(Serialize)]
struct DoneEvent {
    done: bool,
}

pub async fn run(offer_id: &str) -> i32 {
    let maxima_arc = match logged_in_maxima().await {
        Ok(maxima) => maxima,
        Err(code) => return code,
    };
    let mut maxima = maxima_arc.lock().await;

    let offer = match maxima.mut_library().game_by_base_offer(offer_id).await {
        Ok(Some(offer)) => offer.clone(),
        Ok(None) => return fail(ErrorCode::NotOwned),
        Err(err) => {
            log::error!("Failed to read the library: {}", scrub(&err.to_string()));
            return fail(ErrorCode::Internal);
        }
    };
    drop(maxima);

    if !offer.is_installed().await {
        return fail(ErrorCode::Path);
    }

    let Some(check) = offer.offer().install_check_override().clone() else {
        return fail(ErrorCode::Path);
    };
    let dir = match parse_partial_registry_path(&check).await {
        Ok(dir) => dir,
        Err(err) => {
            log::error!(
                "Failed to resolve the install folder: {}",
                scrub(&err.to_string())
            );
            return fail(ErrorCode::Path);
        }
    };

    if !safe_to_remove(&dir) {
        log::error!("Refusing to delete {}", dir.display());
        return fail(ErrorCode::UnsafePath);
    }
    // Without the `[HKLM-key\Value]` part the resolved path is the check file
    // itself, not a folder; there is nothing safe to delete then.
    if !dir.is_dir() {
        log::error!("Install folder {} is not a directory", dir.display());
        return fail(ErrorCode::Path);
    }

    log::info!("Deleting {}", dir.display());
    if let Err(err) = tokio::fs::remove_dir_all(&dir).await {
        log::error!("Failed to delete {}: {}", dir.display(), err);
        return fail(ErrorCode::Path);
    }

    emit(&DoneEvent { done: true });
    0
}

// TODO(merge): replace with guild::logged_in_maxima from Task 2
async fn logged_in_maxima() -> Result<LockedMaxima, i32> {
    let options = MaximaOptionsBuilder::default()
        .load_auth_storage(true)
        .dummy_local_user(false)
        .build()
        .map_err(|_| fail(ErrorCode::Internal))?;
    let maxima_arc = Maxima::new_with_options(options).await.map_err(|err| {
        log::error!("Failed to start Maxima: {}", scrub(&err.to_string()));
        fail(ErrorCode::Internal)
    })?;

    let logged_in = {
        let maxima = maxima_arc.lock().await;
        let mut auth = maxima.auth_storage().lock().await;
        auth.logged_in().await
    };
    match logged_in {
        Ok(true) => Ok(maxima_arc),
        Ok(false) => Err(fail(ErrorCode::LoginRequired)),
        Err(err) => {
            log::error!("Failed to refresh the login: {}", scrub(&err.to_string()));
            Err(fail(ErrorCode::LoginRequired))
        }
    }
}

/// Whether deleting `path` as a game's install folder is safe: it must be
/// absolute, at least two folders below a filesystem root, free of `.` and
/// `..`, and neither equal to nor a parent of the user's home or Maxima's
/// data dir.
pub fn safe_to_remove(path: &Path) -> bool {
    let mut protected = Vec::new();
    let home_var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    if let Some(home) = env::var_os(home_var).filter(|home| !home.is_empty()) {
        protected.push(PathBuf::from(home));
    }
    match maxima_dir() {
        Ok(dir) => protected.push(dir),
        // Without the data dir we can't tell, so refuse.
        Err(_) => return false,
    }
    safe_to_remove_with(path, &protected)
}

fn safe_to_remove_with(path: &Path, protected: &[PathBuf]) -> bool {
    if path.as_os_str().is_empty() || !path.is_absolute() {
        return false;
    }
    if path
        .components()
        .any(|c| matches!(c, Component::CurDir | Component::ParentDir))
    {
        return false;
    }
    let below_root = path
        .components()
        .filter(|c| matches!(c, Component::Normal(_)))
        .count();
    if below_root < 2 {
        return false;
    }

    let target = path_key(path);
    !protected
        .iter()
        .map(|p| path_key(p))
        .any(|p| p.starts_with(&target))
}

/// Comparable form of a path: one string per component, case-folded on
/// Windows, with `C:` and `\\?\C:` treated as the same drive.
fn path_key(path: &Path) -> Vec<String> {
    path.components()
        .filter(|c| !matches!(c, Component::CurDir))
        .map(|c| match c {
            #[cfg(windows)]
            Component::Prefix(prefix) => match prefix.kind() {
                std::path::Prefix::Disk(d) | std::path::Prefix::VerbatimDisk(d) => {
                    format!("{}:", (d as char).to_ascii_lowercase())
                }
                _ => prefix.as_os_str().to_string_lossy().to_lowercase(),
            },
            Component::RootDir => "/".to_owned(),
            other => {
                let s = other.as_os_str().to_string_lossy();
                if cfg!(windows) {
                    s.to_lowercase()
                } else {
                    s.into_owned()
                }
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn safe(path: &str) -> bool {
        safe_to_remove(Path::new(path))
    }

    #[test]
    fn refuses_empty_and_relative() {
        assert!(!safe(""));
        assert!(!safe("relative\\dir"));
        assert!(!safe("relative/dir/deeper"));
    }

    #[cfg(windows)]
    #[test]
    fn refuses_roots_and_shallow_paths() {
        assert!(!safe("C:\\"));
        assert!(!safe("C:/"));
        assert!(!safe("\\\\?\\C:\\"));
        assert!(!safe("C:\\Games"));
        assert!(!safe("C:\\Games\\"));
        assert!(!safe("D:\\Games"));
        assert!(!safe("\\\\server\\share\\Games"));
        // Drive-relative, not absolute.
        assert!(!safe("C:Games\\EA\\Zau"));
    }

    #[cfg(windows)]
    #[test]
    fn refuses_parent_and_current_dir_components() {
        assert!(!safe("C:\\Games\\EA\\..\\.."));
        assert!(!safe("C:\\Games\\EA\\Zau\\..\\..\\.."));
    }

    #[cfg(windows)]
    #[test]
    fn refuses_home_and_its_parents() {
        let home = env::var("USERPROFILE").expect("USERPROFILE is set on Windows");
        assert!(!safe(&home));
        assert!(!safe(&home.to_uppercase()));
        assert!(!safe(&format!("{home}\\")));
        assert!(!safe(&home.replace('\\', "/")));
        let parent = Path::new(&home).parent().unwrap();
        assert!(!safe_to_remove(parent));
    }

    #[test]
    fn refuses_maxima_dir_and_its_parents() {
        let dir = maxima_dir().unwrap();
        assert!(!safe_to_remove(&dir));
        assert!(!safe_to_remove(dir.parent().unwrap()));
        // Below the data dir is fine (e.g. a library folder inside it).
        assert!(safe_to_remove(&dir.join("games").join("Zau")));
    }

    #[cfg(windows)]
    #[test]
    fn allows_game_folders() {
        assert!(safe("C:\\Games\\EA\\Zau"));
        assert!(safe("C:/Games/EA/Zau"));
        assert!(safe("D:\\EA\\Zau"));
        assert!(safe("\\\\?\\C:\\Games\\EA\\Zau"));
    }

    #[cfg(unix)]
    #[test]
    fn unix_paths() {
        assert!(!safe("/"));
        assert!(!safe("/games"));
        assert!(!safe(&env::var("HOME").unwrap()));
        assert!(safe("/games/ea/zau"));
    }

    #[test]
    fn protected_compare_is_by_component() {
        let protected = [PathBuf::from(if cfg!(windows) {
            "C:\\Users\\Noah"
        } else {
            "/home/noah"
        })];
        let (sibling, child) = if cfg!(windows) {
            ("C:\\Users\\Noah2", "C:\\Users\\Noah\\Games")
        } else {
            ("/home/noah2", "/home/noah/games")
        };
        assert!(safe_to_remove_with(Path::new(sibling), &protected));
        assert!(safe_to_remove_with(Path::new(child), &protected));
    }
}
