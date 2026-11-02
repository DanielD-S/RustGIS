//! Help › Check for Updates: asks GitHub for the latest release and compares it with this build,
//! showing a newer one's version and release notes. On Windows, a copy installed with
//! `setup.exe` can download that release's installer, check it and install it when RustGIS
//! closes (#1290, see [`install`]). Nothing is sent until the user asks.

use egui::Align2;
use serde_json::Value;

use crate::{RustGisApp, Slot};

#[cfg(not(target_arch = "wasm32"))]
mod install;

/// The latest release, as JSON naming its tag and page.
const LATEST: &str = "https://api.github.com/repos/opengeos/RustGIS/releases/latest";
/// Every release, for when the check can't reach GitHub.
const RELEASES: &str = "https://github.com/opengeos/RustGIS/releases";
/// The release JSON is a few kilobytes (some tens with long notes); anything far larger isn't it.
const MAX_BYTES: usize = 1024 * 1024;
/// The release notes shown, at most: the rest is on the release page.
const MAX_NOTES: usize = 20_000;

/// The Check for Updates window.
#[derive(Default)]
pub struct UpdateState {
    pub open: bool,
    /// Start a check on the next frame (commands have no egui context to repaint with).
    start: bool,
    pending: Option<Slot<Result<Vec<u8>, String>>>,
    result: Option<Result<Release, String>>,
    /// Download and Install: what it is doing.
    #[cfg(not(target_arch = "wasm32"))]
    install: Install,
}

/// Download and Install's progress, from the click to the installer waiting for RustGIS to close.
#[cfg(not(target_arch = "wasm32"))]
#[derive(Default)]
enum Install {
    #[default]
    Idle,
    /// Downloading and checking `version`'s installer.
    Downloading {
        version: String,
        progress: install::Progress,
        slot: Slot<Result<std::path::PathBuf, String>>,
    },
    /// Checked and saved: it runs when RustGIS closes, once the user has said so (`at_exit`).
    Ready {
        version: String,
        path: std::path::PathBuf,
        at_exit: bool,
    },
    Failed(String),
}

impl UpdateState {
    /// Opens the window and checks again.
    pub fn check(&mut self) {
        self.open = true;
        self.start = true;
        self.result = None;
    }

    /// The installer to run as RustGIS closes, and the folder it installs into: set once the user
    /// chose Install and Restart. The desktop app runs it from its exit (`apps/rustgis`).
    #[cfg(not(target_arch = "wasm32"))]
    pub fn installer_at_exit(&self) -> Option<(std::path::PathBuf, Vec<&'static str>, String)> {
        let Install::Ready { path, at_exit: true, .. } = &self.install else { return None };
        let (args, dir) = install::installer_args(&install::setup_install()?);
        Some((path.clone(), args, dir))
    }
}

/// A file published with a release.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Asset {
    pub name: String,
    pub url: String,
}

/// A published release: its version (the tag without its `v`), its page, its notes and files.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Release {
    pub version: String,
    pub url: String,
    /// The release notes, as written (Markdown), cut to [`MAX_NOTES`].
    pub notes: String,
    /// Its files with an address RustGIS downloads from: HTTPS, or HTTP to this computer.
    pub assets: Vec<Asset>,
}

/// Reads GitHub's latest-release JSON.
pub fn parse_release(body: &[u8]) -> Result<Release, String> {
    let v: Value = serde_json::from_slice(body).map_err(|_| "GitHub's reply isn't a release".to_string())?;
    let tag =
        v.get("tag_name").and_then(Value::as_str).map(str::trim).filter(|t| !t.is_empty()).ok_or("GitHub's reply names no release")?;
    // A tag that isn't a version can't be compared: saying "up to date" would be a guess.
    if version(tag).is_none() {
        return Err("GitHub's latest release has no version number".into());
    }
    let url = v.get("html_url").and_then(Value::as_str).filter(|u| u.starts_with("https://")).unwrap_or(RELEASES);
    let body = v.get("body").and_then(Value::as_str).unwrap_or_default().trim();
    let notes = match body.char_indices().nth(MAX_NOTES) {
        Some((cut, _)) => format!("{}…", body.get(..cut).unwrap_or_default()),
        None => body.to_string(),
    };
    let assets = v
        .get("assets")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|a| {
            let name = a.get("name").and_then(Value::as_str)?;
            // HTTPS, or HTTP to this computer (a release served there to try an update).
            let url =
                a.get("browser_download_url").and_then(Value::as_str).filter(|u| rustgis_engine::plugins::registry::allowed_url(u))?;
            Some(Asset { name: name.to_string(), url: url.to_string() })
        })
        .collect();
    Ok(Release { version: tag.trim_start_matches(['v', 'V']).to_string(), url: url.to_string(), notes, assets })
}

/// Where the check asks for the latest release: GitHub's API, or, to try an update against a
/// release served on this computer, the release JSON `RUSTGIS_UPDATE_RELEASE` names (HTTPS, or
/// HTTP to this computer, as every download here).
fn latest_url() -> String {
    #[cfg(not(target_arch = "wasm32"))]
    if let Ok(url) = std::env::var("RUSTGIS_UPDATE_RELEASE")
        && !url.trim().is_empty()
    {
        return url.trim().to_string();
    }
    LATEST.to_string()
}

/// A pre-release identifier: numbers sort numerically and before words (`beta.2` < `beta.10`,
/// `1` < `alpha`).
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Ident {
    Num(u64),
    Word(String),
}

/// A version's dotted numbers (one to three; missing ones are 0) and its pre-release
/// identifiers, without build metadata; `None` when it isn't a version.
fn version(v: &str) -> Option<([u64; 3], Vec<Ident>)> {
    let v = v.trim().trim_start_matches(['v', 'V']);
    let v = v.split('+').next().unwrap_or(v);
    let (core, pre) = v.split_once('-').map_or((v, None), |(c, p)| (c, Some(p)));
    let parts: Vec<&str> = core.split('.').collect();
    if parts.len() > 3 {
        return None;
    }
    let mut n = [0u64; 3];
    for (slot, part) in n.iter_mut().zip(&parts) {
        if part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        *slot = part.parse().ok()?;
    }
    let pre = match pre {
        None => Vec::new(),
        Some("") => return None,
        Some(p) => p.split('.').map(|i| i.parse().map_or_else(|_| Ident::Word(i.to_string()), Ident::Num)).collect(),
    };
    Some((n, pre))
}

/// Whether version `latest` is newer than `current`, in semantic-version order: the numbers
/// first, then a pre-release (`0.2.0-beta.1`) before its release, and pre-releases by their
/// identifiers. False when either isn't a version.
pub fn is_newer(latest: &str, current: &str) -> bool {
    let (Some((a, a_pre)), Some((b, b_pre))) = (version(latest), version(current)) else { return false };
    if a != b {
        return a > b;
    }
    match (a_pre.is_empty(), b_pre.is_empty()) {
        (true, true) | (false, true) => false,
        (true, false) => true,
        (false, false) => a_pre > b_pre,
    }
}

/// What a click in the window asks for, done once it has drawn.
#[cfg(not(target_arch = "wasm32"))]
enum Action {
    /// Download and check the installer of `version` (Download and Install, or Try Again).
    Download {
        version: String,
        installer: Asset,
        sums: Asset,
    },
    Cancel,
    /// Install and Restart: close RustGIS, which runs the installer.
    InstallNow,
}

/// Megabytes, for a download's progress.
#[cfg(not(target_arch = "wasm32"))]
fn mb(bytes: u64) -> String {
    format!("{:.1}", bytes as f64 / (1024.0 * 1024.0))
}

/// Draws the window, starting and collecting the check.
pub fn show(app: &mut RustGisApp, ctx: &egui::Context) {
    if !app.update.open {
        return;
    }
    // A check asked for while one is running waits for that one rather than sending another.
    if std::mem::take(&mut app.update.start) && app.update.pending.is_none() && !cfg!(target_arch = "wasm32") {
        app.update.pending = Some(crate::pluginsui::fetch(app, &latest_url(), MAX_BYTES, Some(ctx.clone())));
    }
    if let Some(r) = app.update.pending.as_ref().and_then(crate::pluginsui::take) {
        app.update.pending = None;
        app.update.result = Some(r.and_then(|body| parse_release(&body)));
    }
    #[cfg(not(target_arch = "wasm32"))]
    if let Install::Downloading { version, slot, .. } = &app.update.install
        && let Some(r) = slot.lock().ok().and_then(|mut s| s.take())
    {
        app.update.install = match r {
            Ok(path) => Install::Ready { version: version.clone(), path, at_exit: false },
            Err(e) => Install::Failed(e),
        };
    }
    let current = env!("CARGO_PKG_VERSION");
    let mut open = true;
    let mut again = false;
    let mut visit: Option<String> = None;
    #[cfg(not(target_arch = "wasm32"))]
    let mut action: Option<Action> = None;
    egui::Window::new(tl!("Check for Updates"))
        .open(&mut open)
        .collapsible(false)
        .resizable(false)
        .pivot(Align2::CENTER_CENTER)
        .default_pos(ctx.content_rect().center())
        .show(ctx, |ui| {
            ui.set_width(420.0);
            if cfg!(target_arch = "wasm32") {
                ui.label(tl!("The web app always runs the latest release: reload the page to get it."));
                return;
            }
            match &app.update.result {
                None => {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label(tl!("Checking for updates…"));
                    });
                }
                Some(Ok(r)) if is_newer(&r.version, current) => {
                    ui.label(egui::RichText::new(crate::i18n::fmt("RustGIS {latest} is available.", &[("latest", &r.version)])).strong());
                    ui.label(crate::i18n::fmt("You have version {version}.", &[("version", current)]));
                    // What the release brings, before anything is installed.
                    if !r.notes.is_empty() {
                        ui.add_space(6.0);
                        ui.label(egui::RichText::new(tl!("What's new")).strong());
                        egui::Frame::group(ui.style()).show(ui, |ui| {
                            egui::ScrollArea::vertical().max_height(200.0).auto_shrink([false, true]).show(ui, |ui| {
                                ui.label(egui::RichText::new(&r.notes).small());
                            });
                        });
                    }
                    ui.add_space(6.0);
                    #[cfg(not(target_arch = "wasm32"))]
                    {
                        let assets = install::setup_install().and(install::assets_for(r, std::env::consts::ARCH));
                        let stage = match &app.update.install {
                            // A download of another release (an older check) doesn't count.
                            Install::Downloading { version, .. } | Install::Ready { version, .. } if *version != r.version => {
                                &Install::Idle
                            }
                            s => s,
                        };
                        match (stage, assets) {
                            (Install::Downloading { progress, .. }, _) => {
                                use std::sync::atomic::Ordering;
                                let (done, total) = (progress.done.load(Ordering::Relaxed), progress.total.load(Ordering::Relaxed));
                                let text = if total > 0 {
                                    crate::i18n::fmt("Downloading: {done} of {total} MB", &[("done", &mb(done)), ("total", &mb(total))])
                                } else {
                                    crate::i18n::fmt("Downloading: {done} MB", &[("done", &mb(done))])
                                };
                                let bar = if total > 0 {
                                    egui::ProgressBar::new(done as f32 / total as f32)
                                } else {
                                    egui::ProgressBar::new(0.0).animate(true)
                                };
                                ui.add(bar.text(text));
                                ui.add_space(6.0);
                                if ui.button(tl!("Cancel")).clicked() {
                                    action = Some(Action::Cancel);
                                }
                            }
                            (Install::Ready { at_exit, .. }, _) => {
                                ui.label(tl!(
                                    "The installer is downloaded and checked. RustGIS closes, installs the update and opens again."
                                ));
                                if *at_exit {
                                    ui.label(egui::RichText::new(tl!("It installs when RustGIS closes.")).small().weak());
                                }
                                ui.add_space(6.0);
                                ui.horizontal(|ui| {
                                    if ui.button(tl!("Install and Restart")).clicked() {
                                        action = Some(Action::InstallNow);
                                    }
                                    if ui.button(tl!("Open the Release Page")).clicked() {
                                        visit = Some(r.url.clone());
                                    }
                                });
                            }
                            (stage, Some((installer, sums))) => {
                                if let Install::Failed(e) = stage {
                                    ui.label(
                                        egui::RichText::new(crate::i18n::fmt(
                                            "Couldn't download the update: {error}",
                                            &[("error", tl_raw!(e.as_str()))],
                                        ))
                                        .color(ui.visuals().error_fg_color),
                                    );
                                    ui.add_space(4.0);
                                }
                                ui.horizontal(|ui| {
                                    let label =
                                        if matches!(stage, Install::Failed(_)) { tl!("Try Again") } else { tl!("Download and Install") };
                                    if ui.button(label).clicked() {
                                        action = Some(Action::Download {
                                            version: r.version.clone(),
                                            installer: installer.clone(),
                                            sums: sums.clone(),
                                        });
                                    }
                                    if ui.button(tl!("Open the Release Page")).clicked() {
                                        visit = Some(r.url.clone());
                                    }
                                });
                            }
                            (_, None) => {
                                ui.label(
                                    egui::RichText::new(tl!("Installed with Homebrew, winget, the AUR or COPR? Update it there."))
                                        .small()
                                        .weak(),
                                );
                                ui.add_space(6.0);
                                ui.horizontal(|ui| {
                                    if ui.button(tl!("Open the Release Page")).clicked() {
                                        visit = Some(r.url.clone());
                                    }
                                    if ui.button(tl!("Check Again")).clicked() {
                                        again = true;
                                    }
                                });
                            }
                        }
                    }
                }
                Some(Ok(_)) => {
                    ui.label(crate::i18n::fmt("RustGIS {version} is the latest version.", &[("version", current)]));
                    ui.add_space(6.0);
                    if ui.button(tl!("Check Again")).clicked() {
                        again = true;
                    }
                }
                Some(Err(e)) => {
                    ui.label(crate::i18n::fmt("Couldn't check for updates: {error}", &[("error", tl_raw!(e.as_str()))]));
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        if ui.button(tl!("Open the Release Page")).clicked() {
                            visit = Some(RELEASES.to_string());
                        }
                        if ui.button(tl!("Check Again")).clicked() {
                            again = true;
                        }
                    });
                }
            }
        });
    #[cfg(not(target_arch = "wasm32"))]
    match action {
        Some(Action::Download { version, installer, sums }) => {
            let progress = install::Progress::default();
            let slot = install::start(installer, sums, progress.clone(), ctx.clone());
            app.update.install = Install::Downloading { version, progress, slot };
        }
        Some(Action::Cancel) => {
            if let Install::Downloading { progress, .. } = &app.update.install {
                progress.cancel.store(true, std::sync::atomic::Ordering::Relaxed);
            }
            app.update.install = Install::Idle;
        }
        // The usual way out: a project with unsaved changes asks first (Cancel keeps RustGIS
        // open, and the update waits for it to close).
        Some(Action::InstallNow) => {
            if let Install::Ready { at_exit, .. } = &mut app.update.install {
                *at_exit = true;
            }
            app.quit_requested = true;
        }
        None => {}
    }
    if again {
        app.update.check();
    }
    if visit.is_some() {
        app.ui.open_url = visit;
    }
    if !open {
        app.update.open = false;
        app.update.pending = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_compare_by_number_with_pre_releases_first() {
        assert!(is_newer("0.2.0", "0.1.0"));
        assert!(is_newer("v0.1.1", "0.1.0"));
        assert!(is_newer("1.0", "0.9.9"));
        assert!(is_newer("0.10.0", "0.9.0"));
        assert!(is_newer("0.2.0", "0.2.0-beta.1"));
        assert!(!is_newer("0.2.0-beta.1", "0.2.0"));
        assert!(!is_newer("0.1.0", "0.1.0"));
        assert!(!is_newer("0.1.0", "0.2.0"));
        assert!(is_newer("0.2.0-beta.2", "0.2.0-beta.1"));
        assert!(is_newer("0.2.0-beta.10", "0.2.0-beta.2"));
        assert!(is_newer("0.2.0-rc.1", "0.2.0-beta.3"));
        assert!(is_newer("0.2.0-beta", "0.2.0-1"));
        assert!(!is_newer("0.2.0-beta.1", "0.2.0-beta.1"));
        assert!(!is_newer("0.1.0+build.5", "0.1.0"));
        assert!(!is_newer("junk", "0.1.0"));
        assert!(!is_newer("release-2.0.0", "0.1.0"));
        assert!(!is_newer("1.2.3.4", "0.1.0"));
        assert!(!is_newer("", ""));
    }

    #[test]
    fn release_json_is_read_and_checked() {
        let r = parse_release(br#"{"tag_name":"v0.2.0","html_url":"https://github.com/opengeos/RustGIS/releases/tag/v0.2.0"}"#).unwrap();
        assert_eq!(r.version, "0.2.0");
        assert!(r.url.ends_with("/v0.2.0"));
        // A page that isn't HTTPS falls back to the releases list.
        assert_eq!(parse_release(br#"{"tag_name":"0.2.0","html_url":"javascript:alert(1)"}"#).unwrap().url, RELEASES);
        assert!(parse_release(b"<html>rate limited</html>").is_err());
        assert!(parse_release(br#"{"message":"Not Found"}"#).is_err());
        assert!(parse_release(br#"{"tag_name":"  "}"#).is_err());
        // A tag that isn't a version is an error, not "up to date".
        assert!(parse_release(br#"{"tag_name":"release-2.0.0"}"#).is_err());
        assert_eq!(parse_release(br#"{"tag_name":"v1.0.0-rc.1"}"#).unwrap().version, "1.0.0-rc.1");
    }

    #[test]
    fn release_notes_and_files_are_read() {
        let r = parse_release(
            br#"{"tag_name":"v0.6.0","body":"  ## Highlights\n- Faster maps  ","assets":[
                {"name":"SHA256SUMS","browser_download_url":"https://github.com/opengeos/RustGIS/releases/download/v0.6.0/SHA256SUMS"},
                {"name":"plain.exe","browser_download_url":"http://example.com/plain.exe"},
                {"name":"local.exe","browser_download_url":"http://127.0.0.1:8000/local.exe"},
                {"name":"nameless"},
                {"browser_download_url":"https://example.com/x"}
            ]}"#,
        )
        .unwrap();
        assert_eq!(r.notes, "## Highlights\n- Faster maps");
        // Only files with a name and an address RustGIS downloads from: HTTPS, or HTTP to this
        // computer (a release served there to try an update), not plain HTTP elsewhere.
        assert_eq!(r.assets.iter().map(|a| a.name.as_str()).collect::<Vec<_>>(), ["SHA256SUMS", "local.exe"]);
        // No notes or files: empty, not an error.
        let r = parse_release(br#"{"tag_name":"v0.6.0","body":null,"assets":"none"}"#).unwrap();
        assert!(r.notes.is_empty() && r.assets.is_empty());
        // Long notes are cut, on a character.
        let long = format!(r#"{{"tag_name":"v0.6.0","body":"{}"}}"#, "é".repeat(MAX_NOTES + 10));
        let notes = parse_release(long.as_bytes()).unwrap().notes;
        assert_eq!(notes.chars().count(), MAX_NOTES + 1);
        assert!(notes.ends_with('…'));
    }
}
