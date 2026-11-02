//! Download and Install, for RustGIS installed with `setup.exe` on Windows (#1290): the release's
//! installer for this computer, checked against the release's `SHA256SUMS`, run silently when
//! RustGIS closes; it installs over this copy and opens it again. Native only: none of it is in
//! the web app, which always runs the latest release.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use sha2::{Digest, Sha256};

use super::{Asset, Release};
use crate::Slot;

/// An installer is a few tens of megabytes; anything far larger isn't one.
const MAX_INSTALLER: usize = 512 * 1024 * 1024;
/// `SHA256SUMS` lists a few dozen files.
const MAX_SUMS: usize = 1024 * 1024;
/// Time for the whole download, on a slow connection.
const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30 * 60);

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

/// Every check an installer passes before it may run, in one place. Today: its SHA-256 is the
/// one the release's `SHA256SUMS` gives for it, which catches a corrupt or cut download. A
/// signature check joins here once Windows builds are signed: the installer's Authenticode
/// signature, from the expected publisher by name, not any valid signature.
pub(crate) fn verify(installer: &[u8], name: &str, sums: &str) -> Result<(), String> {
    let expected = sum_for(sums, name).ok_or_else(|| crate::i18n::fmt("The release's checksums don't list {name}.", &[("name", name)]))?;
    let got: String = Sha256::digest(installer).iter().map(|b| format!("{b:02x}")).collect();
    if got != expected {
        return Err(crate::i18n::fmt("{name} doesn't match the release's checksum: the download is damaged.", &[("name", name)]));
    }
    Ok(())
}

/// The folder RustGIS was installed in by `setup.exe`, which Download and Install can update:
/// the one holding this program beside the installer's `uninstall.exe`. `None` for the other
/// ways of installing it (the MSI, the Store, winget, the portable zip), which update their own
/// way, and on other systems.
pub(crate) fn setup_install() -> Option<PathBuf> {
    if !cfg!(windows) {
        return None;
    }
    let dir = std::env::current_exe().ok()?.parent()?.to_path_buf();
    setup_install_in(&dir).then_some(dir)
}

/// Whether `dir` holds an install made by `setup.exe`.
pub(crate) fn setup_install_in(dir: &Path) -> bool {
    dir.join("uninstall.exe").is_file() && dir.join("rustgis.exe").is_file()
}

/// The arguments the installer runs with when RustGIS closes: silently (`/S`), opening RustGIS
/// again when done (`/RELAUNCH`, see `packaging/windows/rustgis.nsi`), into this install's
/// folder. NSIS takes `/D=` last and unquoted, spaces and all: the caller passes it raw.
pub(crate) fn installer_args(dir: &Path) -> (Vec<&'static str>, String) {
    (vec!["/S", "/RELAUNCH"], format!("/D={}", dir.display()))
}

/// A download under way: bytes so far and in all, and a way to stop it.
#[derive(Clone, Default)]
pub(crate) struct Progress {
    pub done: Arc<AtomicU64>,
    /// 0 until the server says.
    pub total: Arc<AtomicU64>,
    pub cancel: Arc<AtomicBool>,
}

/// Downloads the installer and the checksums on a thread, checks the installer, and saves it in
/// RustGIS's cache folder; the slot gets its path, or why not.
pub(crate) fn start(installer: Asset, sums: Asset, progress: Progress, ctx: egui::Context) -> Slot<Result<PathBuf, String>> {
    let slot: Slot<Result<PathBuf, String>> = Slot::default();
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

fn download(installer: &Asset, sums: &Asset, progress: &Progress, ctx: &egui::Context) -> Result<PathBuf, String> {
    let sums = rustgis_engine::plugins::registry::fetch(&sums.url, MAX_SUMS)?;
    let sums = String::from_utf8(sums).map_err(|_| tl_raw!("The release's checksums aren't text.").to_string())?;
    let bytes = rustgis_engine::plugins::registry::fetch_with_progress(&installer.url, MAX_INSTALLER, TIMEOUT, &mut |done, total| {
        progress.done.store(done, Ordering::Relaxed);
        progress.total.store(total.unwrap_or(0), Ordering::Relaxed);
        ctx.request_repaint();
        !progress.cancel.load(Ordering::Relaxed)
    })?;
    verify(&bytes, &installer.name, &sums)?;
    let dir = rustgis_engine::data::dirs::cache_dir()
        .ok_or_else(|| tl_raw!("There is no cache folder to keep the installer in.").to_string())?
        .join("updates");
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let path = dir.join(&installer.name);
    std::fs::write(&path, &bytes).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn release(names: &[&str]) -> Release {
        Release {
            version: "0.6.0".into(),
            url: String::new(),
            notes: String::new(),
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
        let hash: String = Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect();
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
        assert_eq!(dir, r"/D=C:\Users\Ana María\AppData\Local\Programs\RustGIS");
    }
}
