//! `guild uninstall <offer>`: deletes the files of the offer's installed build.
//!
//! Only the files listed in the build's zip central directory are removed,
//! plus `__Installer` and the folders that end up empty, so anything else the
//! user keeps in the install folder (which can be a shared library folder)
//! survives. The registry values the game's touchup wrote stay (they are under
//! HKLM and need elevation); `is_installed()` reads false once the files are gone.

use std::{
    collections::BTreeSet,
    env, fs, io,
    path::{Component, Path, PathBuf},
};

use maxima::{content::zip::ZipFile, core::Maxima, util::native::maxima_dir};
use serde::Serialize;

use super::library::without_urls;
use super::out::{emit, fail, scrub, ErrorCode};

const INSTALLER_DIR: &str = "__Installer";

#[derive(Serialize)]
struct DoneEvent {
    done: bool,
}

pub async fn run(offer_id: &str) -> i32 {
    let maxima_arc = match super::logged_in_maxima().await {
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

    let Some(dir) = super::library::install_dir(&offer).await else {
        return fail(ErrorCode::Path);
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

    let installed_version = offer.installed_version().await.ok();
    let mut maxima = maxima_arc.lock().await;
    let entries = build_entries(&mut maxima, offer.offer_id(), installed_version.as_deref()).await;
    drop(maxima);
    let Some(entries) = entries else {
        return fail(ErrorCode::Download);
    };

    let (files, dirs) = plan_removal(&dir, &entries);
    log::info!(
        "Deleting {} files of {} from {}",
        files.len(),
        offer.offer_id(),
        dir.display()
    );
    let failed = apply_removal(&dir, &files, &dirs);
    if failed > 0 {
        log::error!("{} files in {} could not be deleted", failed, dir.display());
        return fail(ErrorCode::Path);
    }

    emit(&DoneEvent { done: true });
    0
}

/// The entry names of the installed build's zip (the live build when the
/// installed one isn't listed). Logs why and returns `None` when they can't be
/// fetched.
async fn build_entries(
    maxima: &mut Maxima,
    offer_id: &str,
    installed_version: Option<&str>,
) -> Option<Vec<String>> {
    let service = maxima.content_manager().service();
    let builds = match service.available_builds(offer_id).await {
        Ok(builds) => builds,
        Err(err) => {
            log::error!("Failed to list builds: {}", without_urls(&err.to_string()));
            return None;
        }
    };
    // `build()` matches on the game version, which is what the manifest records.
    let build = installed_version
        .and_then(|version| builds.build(version))
        .or_else(|| builds.live_build());
    let Some(build) = build else {
        log::error!("{} has no build to read the file list from", offer_id);
        return None;
    };
    // Never log or print this URL: it is signed.
    let url = match service.download_url(offer_id, Some(build.build_id())).await {
        Ok(meta) => meta.url().clone(),
        Err(err) => {
            log::error!(
                "Failed to get the build's download address: {}",
                without_urls(&err.to_string())
            );
            return None;
        }
    };
    match ZipFile::fetch(&url).await {
        Ok(zip) => Some(zip.entries().iter().map(|e| e.name().clone()).collect()),
        Err(err) => {
            log::error!(
                "Failed to read the build's file list: {}",
                without_urls(&err.to_string())
            );
            None
        }
    }
}

/// The relative path of a zip entry, or `None` for an entry that must not be
/// touched: absolute, with a drive or other prefix, or containing `..`.
/// Accepts `/` and `\` as separators and drops empty and `.` segments.
fn entry_path(name: &str) -> Option<PathBuf> {
    if name.starts_with(['/', '\\']) || name.contains('\0') {
        return None;
    }
    let mut rel = PathBuf::new();
    for segment in name.split(['/', '\\']) {
        match segment {
            "" | "." => continue,
            ".." => return None,
            // A drive (`C:`) or an alternate data stream.
            s if s.contains(':') => return None,
            s => rel.push(s),
        }
    }
    // What we built must be plain names only.
    if rel.as_os_str().is_empty() || !rel.components().all(|c| matches!(c, Component::Normal(_))) {
        return None;
    }
    Some(rel)
}

/// Which paths deleting a build from `root` touches: the files its zip lists
/// and the folders those files sit in (deepest first, `root` itself not
/// included). Directory entries (ending in `/`) are not files, but their
/// folders count. Unsafe entries are dropped, so every path is below `root`.
pub fn plan_removal(root: &Path, entries: &[String]) -> (Vec<PathBuf>, Vec<PathBuf>) {
    let mut files = BTreeSet::new();
    let mut dirs = BTreeSet::new();
    for name in entries {
        let Some(rel) = entry_path(name) else {
            log::warn!("Skipping unsafe entry {:?}", name);
            continue;
        };
        let mut parent = if name.ends_with(['/', '\\']) {
            Some(rel.as_path())
        } else {
            files.insert(rel.clone());
            rel.parent()
        };
        while let Some(dir) = parent.filter(|d| !d.as_os_str().is_empty()) {
            dirs.insert(dir.to_path_buf());
            parent = dir.parent();
        }
    }

    let files = files.into_iter().map(|f| root.join(f)).collect();
    let mut dirs: Vec<PathBuf> = dirs.into_iter().collect();
    // Deepest first; ties in path order so the result is stable.
    dirs.sort_by(|a, b| {
        b.components()
            .count()
            .cmp(&a.components().count())
            .then_with(|| a.cmp(b))
    });
    let dirs = dirs.into_iter().map(|d| root.join(d)).collect();
    (files, dirs)
}

/// Deletes `files`, `root/__Installer`, then each of `dirs` and `root` if
/// empty. Missing paths are fine; a file reached through a link that leads
/// out of `root` is skipped. Returns how many files could not be deleted.
fn apply_removal(root: &Path, files: &[PathBuf], dirs: &[PathBuf]) -> usize {
    let real_root = match fs::canonicalize(root) {
        Ok(path) => path,
        Err(err) => {
            log::error!("Failed to resolve {}: {}", root.display(), err);
            return files.len().max(1);
        }
    };

    let mut failed = 0;
    for file in files {
        // A junction or symlink inside the folder must not lead us out of it.
        match file.parent().map(fs::canonicalize) {
            Some(Ok(parent)) if parent.starts_with(&real_root) => {}
            Some(Ok(_)) => {
                log::warn!(
                    "Skipping {}: it is outside the install folder",
                    file.display()
                );
                continue;
            }
            // Its folder is already gone, so is the file.
            _ => continue,
        }
        match fs::remove_file(file) {
            Ok(()) => {}
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => {
                log::warn!("Failed to delete {}: {}", file.display(), err);
                failed += 1;
            }
        }
    }

    let installer = root.join(INSTALLER_DIR);
    match fs::remove_dir_all(&installer) {
        Ok(()) => {}
        Err(err) if err.kind() == io::ErrorKind::NotFound => {}
        Err(err) => {
            log::warn!("Failed to delete {}: {}", installer.display(), err);
            failed += 1;
        }
    }

    // `remove_dir` only removes empty folders: one that still holds something
    // (in use, or not the game's) stays.
    for dir in dirs.iter().map(PathBuf::as_path).chain([root]) {
        let _ = fs::remove_dir(dir);
    }
    failed
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

    fn entries(names: &[&str]) -> Vec<String> {
        names.iter().map(|n| n.to_string()).collect()
    }

    fn game_root() -> PathBuf {
        PathBuf::from(if cfg!(windows) {
            "C:\\Games\\EA"
        } else {
            "/games/ea"
        })
    }

    #[test]
    fn plan_drops_unsafe_entries() {
        let root = game_root();
        let (files, dirs) = plan_removal(
            &root,
            &entries(&[
                "../evil.txt",
                "Zau/../../evil.txt",
                "/etc/passwd",
                "\\Windows\\evil.dll",
                "C:/Windows/evil.dll",
                "C:evil.dll",
                "Zau/file.txt:stream",
                "..",
                "Zau/ok.txt",
            ]),
        );
        assert_eq!(files, vec![root.join("Zau").join("ok.txt")]);
        assert_eq!(dirs, vec![root.join("Zau")]);
    }

    #[test]
    fn plan_lists_parent_dirs_deepest_first() {
        let root = game_root();
        let (files, dirs) = plan_removal(&root, &entries(&["Zau/Binaries/Win64/Zau.exe"]));
        let win64 = root.join("Zau").join("Binaries").join("Win64");
        assert_eq!(files, vec![win64.join("Zau.exe")]);
        assert_eq!(
            dirs,
            vec![
                win64.clone(),
                root.join("Zau").join("Binaries"),
                root.join("Zau")
            ]
        );
    }

    #[test]
    fn plan_stays_below_root_and_skips_dir_entries() {
        let root = game_root();
        let (files, dirs) = plan_removal(
            &root,
            &entries(&[
                "Zau/",
                "Zau/Content/",
                "Zau/Content/a.pak",
                "Zau\\Content\\b.pak",
                "./top.txt",
                "Zau/Logs/",
            ]),
        );
        let content = root.join("Zau").join("Content");
        assert_eq!(
            files,
            vec![
                content.join("a.pak"),
                content.join("b.pak"),
                root.join("top.txt")
            ]
        );
        assert_eq!(
            dirs,
            vec![
                content.clone(),
                root.join("Zau").join("Logs"),
                root.join("Zau")
            ]
        );
        for path in files.iter().chain(&dirs) {
            assert!(
                path.starts_with(&root) && path != &root,
                "{}",
                path.display()
            );
            assert!(!path.starts_with(root.join("Other")), "{}", path.display());
        }
    }

    #[test]
    fn removal_keeps_unrelated_files() {
        let base = env::temp_dir().join(format!("guild-uninstall-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let root = base.join("EA");
        let names = entries(&[
            "Zau/Binaries/Win64/Zau.exe",
            "Zau/Content/Paks/a.pak",
            "Zau/Content/",
            "readme.txt",
        ]);
        for name in &names {
            let path = root.join(name);
            if name.ends_with('/') {
                fs::create_dir_all(&path).unwrap();
            } else {
                fs::create_dir_all(path.parent().unwrap()).unwrap();
                fs::write(&path, b"game").unwrap();
            }
        }
        fs::create_dir_all(root.join(INSTALLER_DIR)).unwrap();
        fs::write(root.join(INSTALLER_DIR).join("installerdata.xml"), b"x").unwrap();
        fs::create_dir_all(root.join("Other")).unwrap();
        fs::write(root.join("Other").join("save.txt"), b"mine").unwrap();
        // The user's file inside a game folder keeps that folder.
        fs::write(root.join("Zau").join("Content").join("mod.txt"), b"mine").unwrap();

        // Listed but already missing is fine.
        let mut listed = names.clone();
        listed.push("Zau/missing.dll".into());
        let (files, dirs) = plan_removal(&root, &listed);
        assert_eq!(apply_removal(&root, &files, &dirs), 0);

        assert!(root.join("Other").join("save.txt").is_file());
        assert!(root.join("Zau").join("Content").join("mod.txt").is_file());
        assert!(root.is_dir());
        for gone in [
            "Zau/Binaries",
            "Zau/Content/Paks",
            "readme.txt",
            INSTALLER_DIR,
        ] {
            assert!(!root.join(gone).exists(), "{gone} should be gone");
        }

        // With nothing else left, the install folder itself goes too.
        fs::remove_dir_all(root.join("Other")).unwrap();
        fs::remove_file(root.join("Zau").join("Content").join("mod.txt")).unwrap();
        assert_eq!(apply_removal(&root, &files, &dirs), 0);
        assert!(!root.exists());
        assert!(base.is_dir());
        fs::remove_dir_all(&base).unwrap();
    }
}
