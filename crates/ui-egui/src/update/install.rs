//! Download and Install, for RustGIS installed with `setup.exe` on Windows (#1290): the release's
//! installer for this computer, checked against the release's `SHA256SUMS`, run silently when
//! RustGIS closes; it installs over this copy and opens it again. Native only: none of it is in
//! the web app, which always runs the latest release.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};

use sha2::{Digest, Sha256};

use super::{Asset, Release};
use crate::Slot;

/// An installer is a few tens of megabytes; anything far larger isn't one.
const MAX_INSTALLER: usize = 512 * 1024 * 1024;
/// `SHA256SUMS` lists a few dozen files.
const MAX_SUMS: usize = 1024 * 1024;
/// Time for the whole download, on a slow connection.
const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30 * 60);
/// How often the progress bar is redrawn while downloading.
const REPAINT: std::time::Duration = std::time::Duration::from_millis(100);
/// The note Install and Restart leaves in the `updates` folder for the next start.
const PENDING: &str = "installing.txt";

/// The name of this computer's installer in a release: `RustGIS-<version>-windows-<arch>-setup.exe`.
pub(crate) fn installer_name(version: &str, arch: &str) -> String {
    format!("RustGIS-{version}-windows-{arch}-setup.exe")
}

/// The release's installer for this computer and its checksums, when it has both.
pub(crate) fn assets_for<'a>(release: &'a Release, arch: &str) -> Option<(&'a Asset, &'a Asset)> {
    let name = installer_name(&release.version, arch);
    let installer = release.assets.iter().find(|a| a.name == name)?;
    let sums = release.assets.iter().find(|a| a.name == "SHA256SUMS")?;
    Some((installer, sums))
}

/// The SHA-256 `SHA256SUMS` gives for `name` (lines of `<hex>  <name>`, or `<hex> *<name>`).
pub(crate) fn sum_for(sums: &str, name: &str) -> Option<String> {
    sums.lines().find_map(|line| {
        let mut parts = line.split_whitespace();
        let hash = parts.next()?;
        let file = parts.next()?.trim_start_matches('*');
        (file == name && hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit())).then(|| hash.to_ascii_lowercase())
    })
}

/// The SHA-256 of `bytes`, in lower-case hex.
fn sha256(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
}

/// Every check an installer passes before it may run, in one place. Today: its SHA-256 is the
/// one the release's `SHA256SUMS` gives for it, which catches a corrupt or cut download (not a
/// replaced release: the list comes from the same place). A signature check joins here once
/// Windows builds are signed: the installer's Authenticode signature, from the expected
/// publisher by name, not any valid signature.
pub(crate) fn verify(installer: &[u8], name: &str, sums: &str) -> Result<(), String> {
    let expected = sum_for(sums, name).ok_or_else(|| crate::i18n::fmt("The release's checksums don't list {name}.", &[("name", name)]))?;
    if sha256(installer) != expected {
        return Err(crate::i18n::fmt("{name} doesn't match the release's checksum: the download is damaged.", &[("name", name)]));
    }
    Ok(())
}

/// The folder RustGIS was installed in by `setup.exe`, which Download and Install can update:
/// the one holding this program beside the installer's `uninstall.exe`. winget installs with the
/// same `setup.exe`, so its installs count too (winget then reads the new version from Apps &
/// features). `None` for the MSI, the Store and the portable zip, which update their own way, and
/// on other systems. Worked out once: the window asks for it every frame.
pub(crate) fn setup_install() -> Option<PathBuf> {
    static SETUP: OnceLock<Option<PathBuf>> = OnceLock::new();
    SETUP
        .get_or_init(|| {
            if !cfg!(windows) {
                return None;
            }
            let dir = std::env::current_exe().ok()?.parent()?.to_path_buf();
            setup_install_in(&dir).then_some(dir)
        })
        .clone()
}

/// Whether `dir` holds an install made by `setup.exe`.
pub(crate) fn setup_install_in(dir: &Path) -> bool {
    dir.join("uninstall.exe").is_file() && dir.join("rustgis.exe").is_file()
}

/// The arguments the installer runs with when RustGIS closes: silently (`/S`), opening RustGIS
/// again when done (`/RELAUNCH`, see `packaging/windows/rustgis.nsi`), into this install's
/// folder. NSIS takes `/D=` last and unquoted, spaces and all: the caller passes it raw, and as
/// the system gives the folder, not as text, so no path changes on the way.
pub(crate) fn installer_args(dir: &Path) -> (Vec<&'static str>, OsString) {
    let mut d = OsString::from("/D=");
    d.push(dir.as_os_str());
    (vec!["/S", "/RELAUNCH"], d)
}

/// Whether the saved installer is still the one that was checked: read at Install and Restart, so
/// a file removed or changed says so while RustGIS can still show it, and again as RustGIS closes
/// to run it.
pub(crate) fn still_checked(path: &Path, sha: &str) -> Result<(), String> {
    let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    if sha256(&bytes) != sha {
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        return Err(crate::i18n::fmt("{name} changed after it was checked: download it again.", &[("name", &name)]));
    }
    Ok(())
}

/// The `updates` folder in RustGIS's cache, where installers and the note are kept.
fn updates_dir() -> Result<PathBuf, String> {
    Ok(rustgis_engine::data::dirs::cache_dir()
        .ok_or_else(|| tl_raw!("There is no cache folder to keep the installer in.").to_string())?
        .join("updates"))
}

/// Notes, for the next start, that `version` is being installed: once RustGIS has closed nothing
/// can be shown, so the run after says whether that version arrived (the installer failing to
/// start, or failing silently, isn't lost).
pub(crate) fn note_pending(version: &str) -> Result<(), String> {
    let path = updates_dir()?.join(PENDING);
    std::fs::write(&path, version).map_err(|e| format!("{}: {e}", path.display()))
}

/// What the last Install and Restart did, read once at start and forgotten: `Ok(version)` when
/// this RustGIS is the version it installed, `Err(version)` when it isn't. Its installer goes too.
pub(crate) fn last_install(current: &str) -> Option<Result<String, String>> {
    let dir = updates_dir().ok()?;
    last_install_in(&dir, current)
}

/// The installer opens RustGIS once it is done (`/RELAUNCH`), so the note is read after it. A
/// RustGIS started by hand while the installer still waits holds `rustgis.exe`, and the installer
/// stops without installing (code 2): "wasn't installed" is right. Only when that RustGIS is
/// closed again within the installer's 30 s does the update arrive after the notice said it
/// didn't; that rare case is left as it is.
fn last_install_in(dir: &Path, current: &str) -> Option<Result<String, String>> {
    let version = std::fs::read_to_string(dir.join(PENDING)).ok()?.trim().to_string();
    clear(dir, &[]);
    (!version.is_empty()).then(|| if super::version_eq(&version, current) { Ok(version) } else { Err(version) })
}

/// Removes what earlier updates left in the `updates` folder, but `keep`.
fn clear(dir: &Path, keep: &[&Path]) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        let p = e.path();
        if !keep.contains(&p.as_path()) && p.is_file() {
            let _ = std::fs::remove_file(&p);
        }
    }
}

/// Before a new installer is saved: the earlier ones go, but not an update set to run when
/// another copy of RustGIS closes, the note and the installer it names. The next start says
/// whether that one ran.
fn clear_for_download(dir: &Path) {
    let note = dir.join(PENDING);
    let pending = std::fs::read_to_string(&note).ok().map(|v| dir.join(installer_name(v.trim(), std::env::consts::ARCH)));
    let mut keep = vec![note.as_path()];
    keep.extend(pending.as_deref());
    clear(dir, &keep);
}

/// A download under way: bytes so far and in all, and a way to stop it.
#[derive(Clone, Default)]
pub(crate) struct Progress {
    pub done: Arc<AtomicU64>,
    /// 0 until the server says.
    pub total: Arc<AtomicU64>,
    pub cancel: Arc<AtomicBool>,
}

/// A checked installer, saved: where it is and its SHA-256 (to check it again before it runs).
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Saved {
    pub path: PathBuf,
    pub sha256: String,
}

/// Downloads the installer and the checksums on a thread, checks the installer, and saves it in
/// RustGIS's cache folder; the slot gets it, or why not.
pub(crate) fn start(installer: Asset, sums: Asset, progress: Progress, ctx: egui::Context) -> Slot<Result<Saved, String>> {
    let slot: Slot<Result<Saved, String>> = Slot::default();
    let out = slot.clone();
    std::thread::spawn(move || {
        let r = download(&installer, &sums, &progress, &ctx);
        if let Ok(mut s) = out.lock() {
            *s = Some(r);
        }
        ctx.request_repaint();
    });
    slot
}

fn download(installer: &Asset, sums: &Asset, progress: &Progress, ctx: &egui::Context) -> Result<Saved, String> {
    let sums = rustgis_engine::plugins::registry::fetch(&sums.url, MAX_SUMS)?;
    let sums = String::from_utf8(sums).map_err(|_| tl_raw!("The release's checksums aren't text.").to_string())?;
    // The bar moves about ten times a second, not at every piece: each repaint draws the whole
    // window, map included. `start` repaints once more when it ends.
    let mut shown: Option<std::time::Instant> = None;
    let bytes = rustgis_engine::plugins::registry::fetch_with_progress(&installer.url, MAX_INSTALLER, TIMEOUT, &mut |done, total| {
        progress.done.store(done, Ordering::Relaxed);
        progress.total.store(total.unwrap_or(0), Ordering::Relaxed);
        if shown.is_none_or(|t| t.elapsed() >= REPAINT) {
            shown = Some(std::time::Instant::now());
            ctx.request_repaint();
        }
        !progress.cancel.load(Ordering::Relaxed)
    })?;
    verify(&bytes, &installer.name, &sums)?;
    // Replaced by another download while this one finished: the folder is that one's now.
    if progress.cancel.load(Ordering::Relaxed) {
        return Err(format!("{}: cancelled", installer.url));
    }
    let dir = updates_dir()?;
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    clear_for_download(&dir);
    let path = dir.join(&installer.name);
    let sha = sha256(&bytes);
    save(&path, &bytes, &sha)?;
    Ok(Saved { path, sha256: sha })
}

/// Saves the checked installer at `path`, unless that file already is it: another copy of
/// RustGIS may have set that one to run as it closes, and rewriting it in place could fail the
/// check it makes then.
fn save(path: &Path, bytes: &[u8], sha: &str) -> Result<(), String> {
    if std::fs::read(path).is_ok_and(|b| sha256(&b) == sha) {
        return Ok(());
    }
    std::fs::write(path, bytes).map_err(|e| format!("{}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn release(names: &[&str]) -> Release {
        Release {
            version: "0.6.0".into(),
            url: String::new(),
            notes: String::new(),
            notes_cut: false,
            assets: names.iter().map(|n| Asset { name: n.to_string(), url: format!("https://example.com/{n}") }).collect(),
        }
    }

    #[test]
    fn the_installer_for_this_computer_and_the_checksums_are_found() {
        let r = release(&[
            "RustGIS-0.6.0-windows-aarch64-setup.exe",
            "RustGIS-0.6.0-windows-x86_64-setup.exe",
            "RustGIS-0.6.0-windows-x86_64.msi",
            "SHA256SUMS",
        ]);
        let (installer, sums) = assets_for(&r, "x86_64").unwrap();
        assert_eq!((installer.name.as_str(), sums.name.as_str()), ("RustGIS-0.6.0-windows-x86_64-setup.exe", "SHA256SUMS"));
        assert_eq!(assets_for(&r, "aarch64").unwrap().0.name, "RustGIS-0.6.0-windows-aarch64-setup.exe");
        // No installer for this computer, or no checksums: nothing to install.
        assert!(assets_for(&r, "riscv64").is_none());
        assert!(assets_for(&release(&["RustGIS-0.6.0-windows-x86_64-setup.exe"]), "x86_64").is_none());
    }

    #[test]
    fn an_installer_runs_only_when_it_matches_the_release_checksum() {
        let bytes = b"installer";
        let hash = sha256(bytes);
        let name = "RustGIS-0.6.0-windows-x86_64-setup.exe";
        let sums = format!("{}  other.exe\n{hash}  {name}\n", "0".repeat(64));
        assert_eq!(verify(bytes, name, &sums), Ok(()));
        // The binary-mode marker and upper-case hex are read too.
        assert_eq!(verify(bytes, name, &format!("{}  *{name}", hash.to_uppercase())), Ok(()));
        // A damaged download, a file the list doesn't name, or a malformed list.
        assert!(verify(b"installer, cut", name, &sums).unwrap_err().contains("damaged"));
        assert!(verify(bytes, name, &format!("{hash}  other.exe")).unwrap_err().contains("don't list"));
        assert!(verify(bytes, name, &format!("{}  {name}", &hash[..10])).is_err());
        assert!(verify(bytes, name, "").is_err());
    }

    #[test]
    fn only_a_setup_install_is_updated_in_place() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!setup_install_in(dir.path()));
        std::fs::write(dir.path().join("rustgis.exe"), b"").unwrap();
        // The portable zip, the MSI or the Store: no uninstall.exe from setup.exe.
        assert!(!setup_install_in(dir.path()));
        std::fs::write(dir.path().join("uninstall.exe"), b"").unwrap();
        assert!(setup_install_in(dir.path()));
    }

    #[test]
    fn the_installer_runs_silently_relaunches_and_installs_here() {
        let (args, dir) = installer_args(Path::new(r"C:\Users\Ana María\AppData\Local\Programs\RustGIS"));
        assert_eq!(args, ["/S", "/RELAUNCH"]);
        // Last and unquoted, as NSIS wants it, even with spaces.
        assert_eq!(dir, OsString::from(r"/D=C:\Users\Ana María\AppData\Local\Programs\RustGIS"));
    }

    #[test]
    fn a_saved_installer_is_checked_again_before_it_runs() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("RustGIS-0.6.0-windows-x86_64-setup.exe");
        std::fs::write(&path, b"installer").unwrap();
        let sha = sha256(b"installer");
        assert_eq!(still_checked(&path, &sha), Ok(()));
        std::fs::write(&path, b"installer, changed").unwrap();
        assert!(still_checked(&path, &sha).unwrap_err().contains("changed after it was checked"));
        std::fs::remove_file(&path).unwrap();
        assert!(still_checked(&path, &sha).is_err());
    }

    #[test]
    fn the_next_start_says_whether_the_update_arrived() {
        let dir = tempfile::tempdir().unwrap();
        // Nothing was installed.
        assert_eq!(last_install_in(dir.path(), "0.6.0"), None);
        // The version installed is this one: it arrived, and the installer is cleared.
        std::fs::write(dir.path().join("RustGIS-0.6.0-windows-x86_64-setup.exe"), b"x").unwrap();
        std::fs::write(dir.path().join(PENDING), "0.6.0\n").unwrap();
        assert_eq!(last_install_in(dir.path(), "0.6.0"), Some(Ok("0.6.0".into())));
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
        // Asked once only.
        assert_eq!(last_install_in(dir.path(), "0.6.0"), None);
        // Still the old version: the update didn't happen, and the next start says so once.
        std::fs::write(dir.path().join(PENDING), "0.6.0").unwrap();
        assert_eq!(last_install_in(dir.path(), "0.5.0"), Some(Err("0.6.0".into())));
        assert_eq!(last_install_in(dir.path(), "0.5.0"), None);
    }

    #[test]
    fn earlier_installers_are_cleared() {
        let dir = tempfile::tempdir().unwrap();
        for n in ["RustGIS-0.5.0-windows-x86_64-setup.exe", PENDING, "keep.exe"] {
            std::fs::write(dir.path().join(n), b"x").unwrap();
        }
        clear(dir.path(), &[&dir.path().join("keep.exe")]);
        let left: Vec<_> = std::fs::read_dir(dir.path()).unwrap().flatten().map(|e| e.file_name()).collect();
        assert_eq!(left, ["keep.exe"]);
    }

    #[test]
    fn an_installer_already_saved_is_not_written_again() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("RustGIS-0.6.0-windows-x86_64-setup.exe");
        save(&path, b"installer", &sha256(b"installer")).unwrap();
        let written = std::fs::metadata(&path).unwrap().modified().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        save(&path, b"installer", &sha256(b"installer")).unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().modified().unwrap(), written);
        // Another file at that name is replaced.
        std::fs::write(&path, b"cut").unwrap();
        save(&path, b"installer", &sha256(b"installer")).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"installer");
    }

    #[test]
    fn a_download_keeps_the_update_another_copy_has_set() {
        let dir = tempfile::tempdir().unwrap();
        let set = installer_name("0.6.0", std::env::consts::ARCH);
        for n in ["RustGIS-0.5.0-windows-x86_64-setup.exe", set.as_str()] {
            std::fs::write(dir.path().join(n), b"x").unwrap();
        }
        std::fs::write(dir.path().join(PENDING), "0.6.0\n").unwrap();
        clear_for_download(dir.path());
        let mut left: Vec<_> = std::fs::read_dir(dir.path()).unwrap().flatten().map(|e| e.file_name()).collect();
        left.sort();
        let mut expected = [std::ffi::OsString::from(&set), PENDING.into()];
        expected.sort();
        assert_eq!(left, expected);
        // Nothing set: every earlier installer goes.
        std::fs::remove_file(dir.path().join(PENDING)).unwrap();
        clear_for_download(dir.path());
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }
}
