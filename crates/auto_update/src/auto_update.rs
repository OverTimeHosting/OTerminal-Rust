pub mod github_release;

use anyhow::{Context as _, Result};
use client::Client;
use db::kvp::KeyValueStore;
use futures_lite::StreamExt;
use github_release::{
    Platform, RELEASES_API_URL, RELEASES_PAGE_URL, UpdateCandidate, expected_sha256,
    parse_checksums_file, parse_releases, rate_limit_backoff, select_update, verify_sha256,
};
use gpui::{
    App, AppContext as _, AsyncApp, BackgroundExecutor, Context, Entity, Global, Task, TaskExt,
    Window, actions,
};
use http_client::{AsyncBody, HttpClient, HttpClientWithUrl, HttpRequestExt, RedirectPolicy};
use paths::remote_servers_dir;
use release_channel::{OTerminalVersion, ReleaseChannel};
use semver::Version;
use serde::{Deserialize, Serialize};
use settings::{RegisterSetting, Settings, SettingsStore};
use smol::fs::File;
use smol::{
    fs,
    io::{AsyncReadExt, AsyncWriteExt},
};
use std::mem;
use std::{
    env::{
        self,
        consts::{ARCH, OS},
    },
    ffi::OsStr,
    ffi::OsString,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime},
};
use util::command::new_command;
use workspace::Workspace;

const SHOULD_SHOW_UPDATE_NOTIFICATION_KEY: &str = "auto-updater-should-show-updated-notification";

#[derive(Debug)]
struct MissingDependencyError(String);

impl std::fmt::Display for MissingDependencyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for MissingDependencyError {}
/// OTerminal checks GitHub on startup and then every few hours. The
/// unauthenticated GitHub API allows 60 requests per hour per IP, and
/// conditional (ETag) requests that return 304 do not count against it.
const POLL_INTERVAL: Duration = Duration::from_secs(4 * 60 * 60);
const NIGHTLY_POLL_INTERVAL: Duration = Duration::from_secs(60 * 60);
/// How long a manual "Check for Updates" waits before giving up on telling
/// the user the result.
const MANUAL_CHECK_RESULT_TIMEOUT: Duration = Duration::from_secs(120);
const REMOTE_SERVER_CACHE_LIMIT: usize = 5;

#[cfg(target_os = "linux")]
fn linux_rsync_install_hint() -> &'static str {
    let os_release = match std::fs::read_to_string("/etc/os-release") {
        Ok(os_release) => os_release,
        Err(_) => return "Please install rsync using your package manager",
    };

    let mut distribution_ids = Vec::new();
    for line in os_release.lines() {
        let trimmed = line.trim();
        if let Some(value) = trimmed.strip_prefix("ID=") {
            distribution_ids.push(value.trim_matches('"').to_ascii_lowercase());
        } else if let Some(value) = trimmed.strip_prefix("ID_LIKE=") {
            for id in value.trim_matches('"').split_whitespace() {
                distribution_ids.push(id.to_ascii_lowercase());
            }
        }
    }

    let package_manager_hint = if distribution_ids
        .iter()
        .any(|distribution_id| distribution_id == "arch")
    {
        Some("Install it with: sudo pacman -S rsync")
    } else if distribution_ids
        .iter()
        .any(|distribution_id| distribution_id == "debian" || distribution_id == "ubuntu")
    {
        Some("Install it with: sudo apt install rsync")
    } else if distribution_ids.iter().any(|distribution_id| {
        distribution_id == "fedora"
            || distribution_id == "rhel"
            || distribution_id == "centos"
            || distribution_id == "rocky"
            || distribution_id == "almalinux"
    }) {
        Some("Install it with: sudo dnf install rsync")
    } else if distribution_ids
        .iter()
        .any(|distribution_id| distribution_id == "nixos")
    {
        Some("Install pkgs.rsync from nixpkgs")
    } else {
        None
    };

    package_manager_hint.unwrap_or("Please install rsync using your package manager")
}

actions!(
    auto_update,
    [
        /// Checks for available updates.
        Check,
        /// Dismisses the update error message.
        DismissMessage,
        /// Opens the release notes for the current version in a browser.
        ViewReleaseNotes,
    ]
);

#[derive(Serialize, Debug)]
pub struct AssetQuery<'a> {
    asset: &'a str,
    os: &'a str,
    arch: &'a str,
    metrics_id: Option<&'a str>,
    system_id: Option<&'a str>,
    is_staff: Option<bool>,
}

#[derive(Clone, Debug)]
pub enum AutoUpdateStatus {
    Idle,
    Checking,
    Downloading {
        version: Version,
        /// Download progress as a fraction in the range `0.0..=1.0`, or `None`
        /// when the total download size is not yet known.
        progress: Option<f32>,
    },
    Installing {
        version: Version,
    },
    Updated {
        version: Version,
    },
    /// OTerminal: a newer release exists, but this copy of OTerminal cannot
    /// install it by itself (a development build, a portable copy, or a
    /// platform without self-update yet). `url` is the GitHub release page.
    UpdateAvailable {
        version: Version,
        url: String,
    },
    Errored {
        error: Arc<anyhow::Error>,
    },
}

impl PartialEq for AutoUpdateStatus {
    // `progress` is deliberately not compared: two `Downloading` statuses for
    // the same version are equal regardless of how far the download is.
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (AutoUpdateStatus::Idle, AutoUpdateStatus::Idle) => true,
            (AutoUpdateStatus::Checking, AutoUpdateStatus::Checking) => true,
            (
                AutoUpdateStatus::Downloading { version: v1, .. },
                AutoUpdateStatus::Downloading { version: v2, .. },
            ) => v1 == v2,
            (
                AutoUpdateStatus::Installing { version: v1 },
                AutoUpdateStatus::Installing { version: v2 },
            ) => v1 == v2,
            (
                AutoUpdateStatus::Updated { version: v1 },
                AutoUpdateStatus::Updated { version: v2 },
            ) => v1 == v2,
            (
                AutoUpdateStatus::UpdateAvailable {
                    version: v1,
                    url: u1,
                },
                AutoUpdateStatus::UpdateAvailable {
                    version: v2,
                    url: u2,
                },
            ) => v1 == v2 && u1 == u2,
            (AutoUpdateStatus::Errored { error: e1 }, AutoUpdateStatus::Errored { error: e2 }) => {
                e1.to_string() == e2.to_string()
            }
            _ => false,
        }
    }
}

impl AutoUpdateStatus {
    pub fn is_updated(&self) -> bool {
        matches!(self, Self::Updated { .. })
    }
}

pub struct AutoUpdater {
    status: AutoUpdateStatus,
    /// OTerminal's own version ([`OTerminalVersion`]), not Zed's.
    current_version: Version,
    client: Arc<Client>,
    pending_poll: Option<Task<Option<()>>>,
    quit_subscription: Option<gpui::Subscription>,
    update_check_type: UpdateCheckType,
    _wake_subscription: gpui::Subscription,
    dismissed_status: Option<AutoUpdateStatus>,
    /// Last GitHub releases response, replayed when GitHub answers 304.
    github_cache: Option<GithubReleasesCache>,
    /// Set after GitHub rate limited us; automatic checks are skipped until then.
    rate_limited_until: Option<SystemTime>,
    /// Release page of the update that was downloaded/installed.
    pending_release_url: Option<String>,
}

#[derive(Clone)]
struct GithubReleasesCache {
    etag: String,
    body: Arc<Vec<u8>>,
}

enum ReleasesFetch {
    Fresh { etag: Option<String>, body: Vec<u8> },
    NotModified,
    RateLimited(Duration),
}

#[derive(Deserialize, Serialize, Clone, Debug)]
pub struct ReleaseAsset {
    pub version: String,
    pub url: String,
}

struct MacOsUnmounter<'a> {
    mount_path: PathBuf,
    background_executor: &'a BackgroundExecutor,
}

impl MacOsUnmounter<'_> {
    /// Unmounts the disk image and waits for completion. This must happen
    /// before the `InstallerDir` is dropped: deleting the temp dir while the
    /// image is still mounted inside it fails silently and leaks the
    /// directory (and the downloaded DMG) in the system temp dir.
    async fn unmount(mut self) {
        let mount_path = mem::take(&mut self.mount_path);
        unmount_disk_image(&mount_path).await;
    }
}

impl Drop for MacOsUnmounter<'_> {
    fn drop(&mut self) {
        let mount_path = mem::take(&mut self.mount_path);
        // Safety net for early exits and cancellation; the happy path calls
        // `unmount`, which leaves the path empty.
        if mount_path.as_os_str().is_empty() {
            return;
        }
        self.background_executor
            .spawn(async move { unmount_disk_image(&mount_path).await })
            .detach();
    }
}

async fn unmount_disk_image(mount_path: &Path) {
    let unmount_output = new_command("hdiutil")
        .args(["detach", "-force"])
        .arg(mount_path)
        .output()
        .await;
    match unmount_output {
        Ok(output) if output.status.success() => {
            log::info!("Successfully unmounted the disk image");
        }
        Ok(output) => {
            log::error!(
                "Failed to unmount disk image: {:?}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        Err(error) => {
            log::error!("Error while trying to unmount disk image: {:?}", error);
        }
    }
}

#[derive(Clone, Copy, Debug, RegisterSetting)]
struct AutoUpdateSetting(bool);

/// Whether or not to automatically check for updates.
///
/// Default: true
impl Settings for AutoUpdateSetting {
    fn from_settings(content: &settings::SettingsContent) -> Self {
        Self(content.auto_update.unwrap())
    }
}

#[derive(Clone, Copy, Debug, RegisterSetting)]
struct AutoUpdateIncludePrereleasesSetting(bool);

/// OTerminal: whether GitHub pre-releases are eligible updates.
///
/// Default: true
impl Settings for AutoUpdateIncludePrereleasesSetting {
    fn from_settings(content: &settings::SettingsContent) -> Self {
        Self(content.auto_update_include_prereleases.unwrap_or(true))
    }
}

/// Whether this copy of OTerminal can replace itself. On Windows that means
/// it was installed by the OTerminal installer (the updater runs the next
/// installer and `tools\auto_update_helper.exe` swaps the files on restart);
/// development builds and portable copies only get an "update available" link.
fn can_install_updates() -> bool {
    if cfg!(test) {
        return true;
    }
    // TODO(oterminal): enable self-update on macOS/Linux once CI publishes
    // bundles for them (the install paths below still expect Zed's layout).
    if !cfg!(target_os = "windows") {
        return false;
    }
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf))
        .is_some_and(|dir| {
            dir.join("tools").join("auto_update_helper.exe").is_file()
                && dir.join("unins000.exe").is_file()
        })
}

/// Debug builds (e.g. `target\debug\oterminal.exe`) do not check for updates
/// on their own unless `OTERMINAL_UPDATE_CHECK=1` is set; "Check for Updates"
/// always works.
fn automatic_checks_enabled_for_build() -> bool {
    !cfg!(debug_assertions) || cfg!(test) || env::var_os("OTERMINAL_UPDATE_CHECK").is_some()
}

#[derive(Default)]
struct GlobalAutoUpdate(Option<Entity<AutoUpdater>>);

impl Global for GlobalAutoUpdate {}

pub fn init(client: Arc<Client>, cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _window, _cx| {
        workspace.register_action(|_, action, window, cx| check(action, window, cx));

        workspace.register_action(|_, action, _, cx| {
            view_release_notes(action, cx);
        });
    })
    .detach();

    let version = OTerminalVersion::global(cx);
    let auto_updater = cx.new(|cx| {
        let updater = AutoUpdater::new(version, client, cx);

        let poll_for_updates = ReleaseChannel::try_global(cx)
            .map(|channel| channel.poll_for_updates())
            .unwrap_or(false)
            && automatic_checks_enabled_for_build();

        if option_env!("ZED_UPDATE_EXPLANATION").is_none()
            && env::var("ZED_UPDATE_EXPLANATION").is_err()
            && poll_for_updates
        {
            let mut update_subscription = AutoUpdateSetting::get_global(cx)
                .0
                .then(|| updater.start_polling(cx));

            cx.observe_global::<SettingsStore>(move |updater: &mut AutoUpdater, cx| {
                if AutoUpdateSetting::get_global(cx).0 {
                    if update_subscription.is_none() {
                        update_subscription = Some(updater.start_polling(cx))
                    }
                } else {
                    update_subscription.take();
                }
            })
            .detach();
        }

        updater
    });
    cx.set_global(GlobalAutoUpdate(Some(auto_updater)));
}

pub fn check(_: &Check, window: &mut Window, cx: &mut App) {
    if let Some(message) = option_env!("ZED_UPDATE_EXPLANATION")
        .map(ToOwned::to_owned)
        .or_else(|| env::var("ZED_UPDATE_EXPLANATION").ok())
    {
        drop(window.prompt(
            gpui::PromptLevel::Info,
            "OTerminal was installed via a package manager.",
            Some(&message),
            &["OK"],
            cx,
        ));
        return;
    }

    if !ReleaseChannel::try_global(cx)
        .map(|channel| channel.poll_for_updates())
        .unwrap_or(false)
    {
        return;
    }

    if let Some(updater) = AutoUpdater::get(cx) {
        updater.update(cx, |updater, cx| updater.poll(UpdateCheckType::Manual, cx));

        // Tell the user when there is nothing to update; every other outcome
        // (downloading, restart to update, update available, error) shows up
        // in the title bar.
        let updater = updater.downgrade();
        window
            .spawn(cx, async move |cx| {
                let started = std::time::Instant::now();
                while started.elapsed() < MANUAL_CHECK_RESULT_TIMEOUT {
                    cx.background_executor()
                        .timer(Duration::from_millis(250))
                        .await;
                    let Ok((finished, status, version)) = updater.read_with(cx, |updater, _| {
                        (
                            updater.pending_poll.is_none(),
                            updater.status.clone(),
                            updater.current_version.clone(),
                        )
                    }) else {
                        return;
                    };
                    if !finished {
                        continue;
                    }
                    if status == AutoUpdateStatus::Idle {
                        cx.update(|window, cx| {
                            drop(window.prompt(
                                gpui::PromptLevel::Info,
                                "OTerminal is up to date",
                                Some(&format!("You are running the newest release ({version}).")),
                                &["OK"],
                                cx,
                            ));
                        })
                        .ok();
                    }
                    return;
                }
            })
            .detach();
    } else {
        drop(window.prompt(
            gpui::PromptLevel::Info,
            "Could not check for updates",
            Some("Auto-updates disabled for non-bundled app."),
            &["OK"],
            cx,
        ));
    }
}

/// The GitHub release page of the running OTerminal version.
pub fn release_notes_url(cx: &mut App) -> Option<String> {
    let mut version = OTerminalVersion::global(cx);
    version.build = semver::BuildMetadata::EMPTY;
    Some(format!("{RELEASES_PAGE_URL}/tag/v{version}"))
}

/// The GitHub release page of the update that is ready to install (or
/// available), if any.
pub fn pending_release_notes_url(cx: &App) -> Option<String> {
    let updater = cx.try_global::<GlobalAutoUpdate>()?.0.clone()?;
    let updater = updater.read(cx);
    match &updater.status {
        AutoUpdateStatus::UpdateAvailable { url, .. } => Some(url.clone()),
        _ => updater.pending_release_url.clone(),
    }
}

pub fn view_release_notes(_: &ViewReleaseNotes, cx: &mut App) -> Option<()> {
    let url = release_notes_url(cx)?;
    cx.open_url(&url);
    None
}

#[cfg(not(target_os = "windows"))]
const INSTALLER_DIR_PREFIX: &str = "oterminal-auto-update";

#[cfg(not(target_os = "windows"))]
struct InstallerDir(tempfile::TempDir);

#[cfg(not(target_os = "windows"))]
impl InstallerDir {
    async fn new() -> Result<Self> {
        Ok(Self(
            tempfile::Builder::new()
                .prefix(INSTALLER_DIR_PREFIX)
                .tempdir()?,
        ))
    }

    fn path(&self) -> &Path {
        self.0.path()
    }
}

#[cfg(target_os = "windows")]
struct InstallerDir(PathBuf);

#[cfg(target_os = "windows")]
impl InstallerDir {
    async fn new() -> Result<Self> {
        #[cfg(not(test))]
        let installer_dir = std::env::current_exe()?
            .parent()
            .context("No parent dir for oterminal.exe")?
            .join("updates");
        // Tests run in parallel; give each download its own directory.
        #[cfg(test)]
        let installer_dir = {
            static NEXT_ID: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
            let id = NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let root = std::env::temp_dir().join("oterminal-update-tests");
            smol::fs::create_dir_all(&root).await?;
            root.join(format!("{}-{id}", std::process::id()))
        };
        if smol::fs::metadata(&installer_dir).await.is_ok() {
            smol::fs::remove_dir_all(&installer_dir).await?;
        }
        smol::fs::create_dir(&installer_dir).await?;
        Ok(Self(installer_dir))
    }

    fn path(&self) -> &Path {
        self.0.as_path()
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum UpdateCheckType {
    Automatic,
    Manual,
}

impl UpdateCheckType {
    pub fn is_manual(self) -> bool {
        self == Self::Manual
    }
}

impl AutoUpdater {
    pub fn get(cx: &mut App) -> Option<Entity<Self>> {
        cx.default_global::<GlobalAutoUpdate>().0.clone()
    }

    fn new(current_version: Version, client: Arc<Client>, cx: &mut Context<Self>) -> Self {
        // On windows, executable files cannot be overwritten while they are
        // running, so we must wait to overwrite the application until quitting
        // or restarting. When quitting the app, we spawn the auto update helper
        // to finish the auto update process after Zed exits. When restarting
        // the app after an update, we use `set_restart_path` to run the auto
        // update helper instead of the app, so that it can overwrite the app
        // and then spawn the new binary.
        #[cfg(target_os = "windows")]
        let quit_subscription = Some(cx.on_app_quit(|_, _| finalize_auto_update_on_quit()));
        #[cfg(not(target_os = "windows"))]
        let quit_subscription = None;

        cx.on_app_restart(|this, _| {
            this.quit_subscription.take();
        })
        .detach();

        // A download or check that was in flight when the machine went to sleep
        // is almost certainly riding a TCP connection that silently died during
        // suspend, so it would otherwise appear to stall indefinitely.
        let wake_subscription = cx.on_system_wake({
            let this = cx.entity().downgrade();
            move |cx| {
                this.update(cx, |this, cx| this.restart_after_wake(cx)).ok();
            }
        });

        Self {
            status: AutoUpdateStatus::Idle,
            current_version,
            client,
            pending_poll: None,
            quit_subscription,
            update_check_type: UpdateCheckType::Automatic,
            _wake_subscription: wake_subscription,
            dismissed_status: None,
            github_cache: None,
            rate_limited_until: None,
            pending_release_url: None,
        }
    }

    fn restart_after_wake(&mut self, cx: &mut Context<Self>) {
        // Only network phases can be safely restarted. `Installing` is a local
        // operation (mounting a dmg, rsync, etc.) that must not be interrupted.
        if !matches!(
            self.status,
            AutoUpdateStatus::Checking | AutoUpdateStatus::Downloading { .. }
        ) {
            return;
        }

        let check_type = self.update_check_type;
        self.pending_poll.take();
        self.status = AutoUpdateStatus::Idle;
        self.poll(check_type, cx);
    }

    pub fn start_polling(&self, cx: &mut Context<Self>) -> Task<Result<()>> {
        let poll_interval =
            ReleaseChannel::try_global(cx).map_or(POLL_INTERVAL, |channel| match channel {
                ReleaseChannel::Nightly => NIGHTLY_POLL_INTERVAL,
                _ => POLL_INTERVAL,
            });

        cx.spawn(async move |this, cx| {
            if cfg!(target_os = "windows") {
                use util::ResultExt;

                cleanup_windows()
                    .await
                    .context("failed to cleanup old directories")
                    .log_err();
            }

            #[cfg(all(not(target_os = "windows"), not(test)))]
            cx.background_spawn(cleanup_stale_installer_dirs()).detach();

            loop {
                this.update(cx, |this, cx| this.poll(UpdateCheckType::Automatic, cx))?;
                cx.background_executor().timer(poll_interval).await;
            }
        })
    }

    pub fn update_check_type(&self) -> UpdateCheckType {
        self.update_check_type
    }

    pub fn poll(&mut self, check_type: UpdateCheckType, cx: &mut Context<Self>) {
        if check_type.is_manual() {
            self.dismissed_status = None;
        }
        if self.pending_poll.is_some() {
            if self.update_check_type == UpdateCheckType::Automatic {
                self.update_check_type = check_type;
                cx.notify();
            }
            return;
        }
        self.update_check_type = check_type;

        cx.notify();

        self.pending_poll = Some(cx.spawn(async move |this, cx| {
            let result = Self::update(this.upgrade()?, cx).await;
            this.update(cx, |this, cx| {
                this.pending_poll = None;
                if let Err(error) = result {
                    let is_missing_dependency =
                        error.downcast_ref::<MissingDependencyError>().is_some();
                    this.status = match check_type {
                        UpdateCheckType::Automatic if is_missing_dependency => {
                            log::warn!("auto-update: {}", error);
                            AutoUpdateStatus::Errored {
                                error: Arc::new(error),
                            }
                        }
                        // Be quiet if the check was automated (e.g. when offline)
                        UpdateCheckType::Automatic => {
                            log::info!("auto-update check failed: error:{:?}", error);
                            AutoUpdateStatus::Idle
                        }
                        UpdateCheckType::Manual => {
                            log::error!("auto-update failed: error:{:?}", error);
                            AutoUpdateStatus::Errored {
                                error: Arc::new(error),
                            }
                        }
                    };

                    cx.notify();
                }
            })
            .ok()
        }));
    }

    pub fn current_version(&self) -> Version {
        self.current_version.clone()
    }

    pub fn status(&self) -> AutoUpdateStatus {
        self.status.clone()
    }

    pub fn dismissed_status(&self) -> Option<AutoUpdateStatus> {
        self.dismissed_status.clone()
    }

    pub fn dismiss_status(&mut self, status: AutoUpdateStatus, cx: &mut Context<Self>) {
        self.dismissed_status = Some(status);
        cx.notify();
    }

    pub fn dismiss(&mut self, cx: &mut Context<Self>) -> bool {
        if let AutoUpdateStatus::Idle = self.status {
            return false;
        }
        self.status = AutoUpdateStatus::Idle;
        cx.notify();
        true
    }

    // If you are packaging Zed and need to override the place it downloads SSH remotes from,
    // you can override this function. You should also update get_remote_server_release_url to return
    // Ok(None).
    pub async fn download_remote_server_release(
        release_channel: ReleaseChannel,
        version: Option<Version>,
        os: &str,
        arch: &str,
        set_status: impl Fn(&str, &mut AsyncApp) + Send + 'static,
        cx: &mut AsyncApp,
    ) -> Result<PathBuf> {
        let this = cx.update(|cx| {
            cx.default_global::<GlobalAutoUpdate>()
                .0
                .clone()
                .context("auto-update not initialized")
        })?;

        set_status("Fetching remote server release", cx);
        let release = Self::get_release_asset(
            &this,
            release_channel,
            version,
            "zed-remote-server",
            os,
            arch,
            cx,
        )
        .await?;

        let servers_dir = paths::remote_servers_dir();
        let channel_dir = servers_dir.join(release_channel.dev_name());
        let platform_dir = channel_dir.join(format!("{}-{}", os, arch));
        let version_path = platform_dir.join(format!("{}.gz", release.version));
        smol::fs::create_dir_all(&platform_dir).await.ok();

        let client = this.read_with(cx, |this, _| this.client.http_client());

        if smol::fs::metadata(&version_path).await.is_err() {
            log::info!(
                "downloading zed-remote-server {os} {arch} version {}",
                release.version
            );
            set_status("Downloading remote server", cx);
            download_remote_server_binary(&version_path, release, client).await?;
        }

        if let Err(error) =
            cleanup_remote_server_cache(&platform_dir, &version_path, REMOTE_SERVER_CACHE_LIMIT)
                .await
        {
            log::warn!(
                "Failed to clean up remote server cache in {:?}: {error:#}",
                platform_dir
            );
        }

        Ok(version_path)
    }

    pub async fn get_remote_server_release_url(
        channel: ReleaseChannel,
        version: Option<Version>,
        os: &str,
        arch: &str,
        cx: &mut AsyncApp,
    ) -> Result<Option<String>> {
        let this = cx.update(|cx| {
            cx.default_global::<GlobalAutoUpdate>()
                .0
                .clone()
                .context("auto-update not initialized")
        })?;

        let release =
            Self::get_release_asset(&this, channel, version, "zed-remote-server", os, arch, cx)
                .await?;

        Ok(Some(release.url))
    }

    async fn get_release_asset(
        this: &Entity<Self>,
        release_channel: ReleaseChannel,
        version: Option<Version>,
        asset: &str,
        os: &str,
        arch: &str,
        cx: &mut AsyncApp,
    ) -> Result<ReleaseAsset> {
        let client = this.read_with(cx, |this, _| this.client.clone());

        let (system_id, metrics_id, is_staff) = if client.telemetry().metrics_enabled() {
            (
                client.telemetry().system_id(),
                client.telemetry().metrics_id(),
                client.telemetry().is_staff(),
            )
        } else {
            (None, None, None)
        };

        let version = if let Some(mut version) = version {
            version.pre = semver::Prerelease::EMPTY;
            version.build = semver::BuildMetadata::EMPTY;
            version.to_string()
        } else {
            "latest".to_string()
        };
        let http_client = client.http_client();

        let path = format!("/releases/{}/{}/asset", release_channel.dev_name(), version,);
        let url = http_client.build_zed_cloud_url_with_query(
            &path,
            AssetQuery {
                os,
                arch,
                asset,
                metrics_id: metrics_id.as_deref(),
                system_id: system_id.as_deref(),
                is_staff,
            },
        )?;

        let mut response = http_client
            .get(url.as_str(), Default::default(), true)
            .await?;
        let mut body = Vec::new();
        response.body_mut().read_to_end(&mut body).await?;

        anyhow::ensure!(
            response.status().is_success(),
            "failed to fetch release: {:?}",
            String::from_utf8_lossy(&body),
        );

        serde_json::from_slice(body.as_slice()).with_context(|| {
            format!(
                "error deserializing release {:?}",
                String::from_utf8_lossy(&body),
            )
        })
    }

    async fn update(this: Entity<Self>, cx: &mut AsyncApp) -> Result<()> {
        let (
            client,
            installed_version,
            previous_status,
            check_type,
            cached_releases,
            rate_limited_until,
            include_prereleases,
        ) = this.read_with(cx, |this, cx| {
            (
                this.client.http_client(),
                this.current_version.clone(),
                this.status.clone(),
                this.update_check_type,
                this.github_cache.clone(),
                this.rate_limited_until,
                AutoUpdateIncludePrereleasesSetting::get_global(cx).0,
            )
        });

        Self::check_dependencies()?;

        if let Some(until) = rate_limited_until
            && let Ok(remaining) = until.duration_since(SystemTime::now())
        {
            let minutes = remaining.as_secs().div_ceil(60);
            anyhow::ensure!(
                !check_type.is_manual(),
                "GitHub's API rate limit was reached; try again in {minutes} minute(s)."
            );
            log::info!("Auto Update: skipping check, GitHub rate limit resets in {minutes} min");
            return Ok(());
        }

        this.update(cx, |this, cx| {
            this.status = AutoUpdateStatus::Checking;
            log::info!("Auto Update: checking GitHub releases for updates");
            cx.notify();
        });

        let user_agent = format!("OTerminal/{installed_version} ({OS}; {ARCH})");
        let fetched = fetch_github_releases(
            &client,
            cached_releases.as_ref().map(|cache| cache.etag.as_str()),
            &user_agent,
        )
        .await?;
        let body = match fetched {
            ReleasesFetch::Fresh { etag, body } => {
                let body = Arc::new(body);
                this.update(cx, |this, _| {
                    this.rate_limited_until = None;
                    this.github_cache = etag.map(|etag| GithubReleasesCache {
                        etag,
                        body: body.clone(),
                    });
                });
                body
            }
            ReleasesFetch::NotModified => {
                this.update(cx, |this, _| this.rate_limited_until = None);
                cached_releases
                    .context("GitHub answered 304 Not Modified without a cached response")?
                    .body
            }
            ReleasesFetch::RateLimited(wait) => {
                let minutes = wait.as_secs().div_ceil(60);
                this.update(cx, |this, cx| {
                    this.rate_limited_until = Some(SystemTime::now() + wait);
                    this.status = match previous_status {
                        AutoUpdateStatus::Updated { .. }
                        | AutoUpdateStatus::UpdateAvailable { .. } => previous_status,
                        _ => AutoUpdateStatus::Idle,
                    };
                    cx.notify();
                });
                anyhow::ensure!(
                    !check_type.is_manual(),
                    "GitHub's API rate limit was reached; try again in {minutes} minute(s)."
                );
                log::warn!("Auto Update: GitHub rate limited us, backing off for {minutes} min");
                return Ok(());
            }
        };

        let releases = parse_releases(&body)?;
        // Once an update has been installed (waiting for a restart), only a
        // release newer than that one is interesting.
        let current_version = match &previous_status {
            AutoUpdateStatus::Updated { version } => version.clone(),
            _ => installed_version,
        };
        let candidate = select_update(
            &releases,
            &current_version,
            include_prereleases,
            Platform::current(),
        );

        let Some(candidate) = candidate else {
            log::info!("Auto Update: no newer release than {current_version}");
            this.update(cx, |this, cx| {
                let status = match previous_status {
                    AutoUpdateStatus::Updated { .. } => previous_status,
                    _ => AutoUpdateStatus::Idle,
                };
                this.status = status;
                cx.notify();
            });
            return Ok(());
        };
        let newer_version = candidate.version.clone();
        log::info!(
            "Auto Update: found {} ({})",
            candidate.tag,
            candidate.asset_name
        );

        if !can_install_updates() {
            log::info!(
                "Auto Update: this copy of OTerminal cannot update itself; linking to {}",
                candidate.html_url
            );
            this.update(cx, |this, cx| {
                this.status = AutoUpdateStatus::UpdateAvailable {
                    version: newer_version,
                    url: candidate.html_url.clone(),
                };
                cx.notify();
            });
            return Ok(());
        }

        this.update(cx, |this, cx| {
            this.status = AutoUpdateStatus::Downloading {
                version: newer_version.clone(),
                progress: None,
            };
            cx.notify();
        });

        let expected_sha256 = expected_checksum(&client, &candidate).await?;

        let installer_dir = InstallerDir::new()
            .await
            .context("Failed to create installer dir")?;
        let target_path = Self::target_path(&installer_dir).await?;
        let progress_entity = this.clone();
        let mut progress_cx = cx.clone();
        let actual_sha256 = download_release(
            &target_path,
            ReleaseAsset {
                version: newer_version.to_string(),
                url: candidate.download_url.clone(),
            },
            client,
            move |progress| {
                progress_entity.update(&mut progress_cx, |this, cx| {
                    if let AutoUpdateStatus::Downloading {
                        progress: current_progress,
                        ..
                    } = &mut this.status
                    {
                        *current_progress = progress;
                        cx.notify();
                    }
                });
            },
        )
        .await
        .with_context(|| format!("Failed to download update to {}", target_path.display()))?;

        if let Err(error) = verify_sha256(&actual_sha256, &expected_sha256) {
            smol::fs::remove_file(&target_path).await.ok();
            return Err(error);
        }
        log::info!(
            "Auto Update: verified sha256 {actual_sha256} of {}",
            candidate.asset_name
        );

        this.update(cx, |this, cx| {
            this.status = AutoUpdateStatus::Installing {
                version: newer_version.clone(),
            };
            cx.notify();
        });

        #[cfg(test)]
        let install_result = match cx
            .try_read_global::<tests::InstallOverride, _>(|g, _| g.0.clone())
            .map(|test_install| test_install(&target_path, cx))
        {
            Some(result) => result,
            None => return Ok(()),
        };

        #[cfg(not(test))]
        let install_result = {
            let running_app_path = cx.update(|cx| cx.app_path())?;
            let background_executor = cx.background_executor().clone();
            let channel = cx.update(|cx| ReleaseChannel::global(cx).dev_name());
            cx.background_spawn(Self::install_release(
                installer_dir,
                target_path.clone(),
                running_app_path,
                channel,
                background_executor,
            ))
            .await
        };
        let new_binary_path = install_result
            .with_context(|| format!("Failed to install update at: {}", target_path.display()))?;
        if let Some(new_binary_path) = new_binary_path {
            cx.update(|cx| cx.set_restart_path(new_binary_path));
        }

        this.update(cx, |this, cx| {
            this.set_should_show_update_notification(true, cx)
                .detach_and_log_err(cx);
            this.pending_release_url = Some(candidate.html_url.clone());
            this.status = AutoUpdateStatus::Updated {
                version: newer_version,
            };
            cx.notify();
        });
        Ok(())
    }

    fn check_dependencies() -> Result<()> {
        #[cfg(target_os = "linux")]
        if which::which("rsync").is_err() {
            let install_hint = linux_rsync_install_hint();
            return Err(MissingDependencyError(format!(
                "rsync is required for auto-updates but is not installed. {install_hint}"
            ))
            .into());
        }

        #[cfg(target_os = "macos")]
        anyhow::ensure!(
            which::which("rsync").is_ok(),
            "Could not auto-update because the required rsync utility was not found."
        );

        Ok(())
    }

    async fn target_path(installer_dir: &InstallerDir) -> Result<PathBuf> {
        let filename = match OS {
            "macos" => anyhow::Ok("OTerminal.dmg"),
            "linux" => Ok("oterminal.tar.gz"),
            "windows" => Ok("OTerminal-Setup.exe"),
            unsupported_os => anyhow::bail!("not supported: {unsupported_os}"),
        }?;

        Ok(installer_dir.path().join(filename))
    }

    #[cfg_attr(test, allow(dead_code))]
    async fn install_release(
        installer_dir: InstallerDir,
        target_path: PathBuf,
        running_app_path: PathBuf,
        channel: &str,
        background_executor: BackgroundExecutor,
    ) -> Result<Option<PathBuf>> {
        match OS {
            "macos" => {
                install_release_macos(
                    &installer_dir,
                    &target_path,
                    running_app_path,
                    &background_executor,
                )
                .await
            }
            "linux" => {
                install_release_linux(&installer_dir, &target_path, channel, running_app_path).await
            }
            "windows" => install_release_windows(&target_path).await,
            unsupported_os => anyhow::bail!("not supported: {unsupported_os}"),
        }
    }

    pub fn set_should_show_update_notification(
        &self,
        should_show: bool,
        cx: &App,
    ) -> Task<Result<()>> {
        let kvp = KeyValueStore::global(cx);
        cx.background_spawn(async move {
            if should_show {
                kvp.write_kvp(
                    SHOULD_SHOW_UPDATE_NOTIFICATION_KEY.to_string(),
                    "".to_string(),
                )
                .await?;
            } else {
                kvp.delete_kvp(SHOULD_SHOW_UPDATE_NOTIFICATION_KEY.to_string())
                    .await?;
            }
            Ok(())
        })
    }

    pub fn should_show_update_notification(&self, cx: &App) -> Task<Result<bool>> {
        let kvp = KeyValueStore::global(cx);
        cx.background_spawn(async move {
            Ok(kvp.read_kvp(SHOULD_SHOW_UPDATE_NOTIFICATION_KEY)?.is_some())
        })
    }
}

async fn download_remote_server_binary(
    target_path: &PathBuf,
    release: ReleaseAsset,
    client: Arc<HttpClientWithUrl>,
) -> Result<()> {
    let temp = tempfile::Builder::new().tempfile_in(remote_servers_dir())?;
    let mut temp_file = File::create(&temp).await?;

    let mut response = client.get(&release.url, Default::default(), true).await?;
    anyhow::ensure!(
        response.status().is_success(),
        "failed to download remote server release: {:?}",
        response.status()
    );
    smol::io::copy(response.body_mut(), &mut temp_file).await?;
    smol::fs::rename(&temp, &target_path).await?;

    Ok(())
}

async fn cleanup_remote_server_cache(
    platform_dir: &Path,
    keep_path: &Path,
    limit: usize,
) -> Result<()> {
    if limit == 0 {
        return Ok(());
    }

    let mut entries = smol::fs::read_dir(platform_dir).await?;
    let now = SystemTime::now();
    let mut candidates = Vec::new();

    while let Some(entry) = entries.next().await {
        let entry = entry?;
        let path = entry.path();
        if path.extension() != Some(OsStr::new("gz")) {
            continue;
        }

        let mtime = if path == keep_path {
            now
        } else {
            smol::fs::metadata(&path)
                .await
                .and_then(|metadata| metadata.modified())
                .unwrap_or(SystemTime::UNIX_EPOCH)
        };

        candidates.push((path, mtime));
    }

    if candidates.len() <= limit {
        return Ok(());
    }

    candidates.sort_by(|(path_a, time_a), (path_b, time_b)| {
        time_b.cmp(time_a).then_with(|| path_a.cmp(path_b))
    });

    for (index, (path, _)) in candidates.into_iter().enumerate() {
        if index < limit || path == keep_path {
            continue;
        }

        if let Err(error) = smol::fs::remove_file(&path).await {
            log::warn!(
                "Failed to remove old remote server archive {:?}: {}",
                path,
                error
            );
        }
    }

    Ok(())
}

/// Lists the repository's releases, conditionally on `etag`.
async fn fetch_github_releases(
    client: &HttpClientWithUrl,
    etag: Option<&str>,
    user_agent: &str,
) -> Result<ReleasesFetch> {
    let request = http_client::Request::builder()
        .uri(RELEASES_API_URL)
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .header("User-Agent", user_agent)
        .when_some(etag, |builder, etag| builder.header("If-None-Match", etag))
        .follow_redirects(RedirectPolicy::FollowAll)
        .timeout(Duration::from_secs(30))
        .body(AsyncBody::empty())?;
    let mut response = client
        .send(request)
        .await
        .context("failed to reach the GitHub releases API")?;

    let status = response.status();
    if status == http_client::StatusCode::NOT_MODIFIED && etag.is_some() {
        return Ok(ReleasesFetch::NotModified);
    }
    let now_unix_secs = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or_default();
    if let Some(wait) = rate_limit_backoff(status, response.headers(), now_unix_secs) {
        return Ok(ReleasesFetch::RateLimited(wait));
    }

    let response_etag = response
        .headers()
        .get(http_client::http::header::ETAG)
        .and_then(|value| value.to_str().ok())
        .map(ToOwned::to_owned);
    let mut body = Vec::new();
    response.body_mut().read_to_end(&mut body).await?;
    anyhow::ensure!(
        status.is_success(),
        "GitHub releases API returned {status}: {}",
        String::from_utf8_lossy(&body[..body.len().min(500)])
    );
    Ok(ReleasesFetch::Fresh {
        etag: response_etag,
        body,
    })
}

/// The SHA-256 the downloaded asset must have, from GitHub's asset digest
/// and/or the release's `SHA256SUMS.txt`.
async fn expected_checksum(
    client: &HttpClientWithUrl,
    candidate: &UpdateCandidate,
) -> Result<String> {
    let mut from_checksums_file = None;
    if let Some(url) = &candidate.checksums_url {
        match fetch_text(client, url).await {
            Ok(contents) => {
                from_checksums_file = parse_checksums_file(&contents, &candidate.asset_name);
                if from_checksums_file.is_none() {
                    log::warn!(
                        "Auto Update: {} is not listed in {url}",
                        candidate.asset_name
                    );
                }
            }
            // GitHub's digest alone is enough when it exists.
            Err(error) if candidate.sha256_from_digest.is_some() => {
                log::warn!("Auto Update: could not fetch {url}: {error:#}");
            }
            Err(error) => return Err(error),
        }
    }
    expected_sha256(
        candidate.sha256_from_digest.as_deref(),
        from_checksums_file.as_deref(),
    )
}

async fn fetch_text(client: &HttpClientWithUrl, url: &str) -> Result<String> {
    let mut response = client.get(url, AsyncBody::empty(), true).await?;
    let mut body = Vec::new();
    response.body_mut().read_to_end(&mut body).await?;
    anyhow::ensure!(
        response.status().is_success(),
        "failed to fetch {url}: {}",
        response.status()
    );
    Ok(String::from_utf8_lossy(&body).into_owned())
}

/// Downloads `release` to `target_path`, returning the file's SHA-256 (hex).
async fn download_release(
    target_path: &Path,
    release: ReleaseAsset,
    client: Arc<HttpClientWithUrl>,
    mut on_progress: impl FnMut(Option<f32>),
) -> Result<String> {
    let mut target_file = File::create(&target_path).await?;
    let mut hasher = github_release::Sha256Hasher::default();

    let mut response = client.get(&release.url, Default::default(), true).await?;
    anyhow::ensure!(
        response.status().is_success(),
        "failed to download update: {:?}",
        response.status()
    );

    let total_bytes = response
        .headers()
        .get(http_client::http::header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|total_bytes| *total_bytes > 0);

    let mut downloaded_bytes: u64 = 0;
    let mut last_reported_percent: Option<u8> = None;
    let mut buffer = [0u8; 8192];
    let body = response.body_mut();
    loop {
        let bytes_read = body.read(&mut buffer).await?;
        if bytes_read == 0 {
            break;
        }
        target_file.write_all(&buffer[..bytes_read]).await?;
        hasher.update(&buffer[..bytes_read]);
        downloaded_bytes += bytes_read as u64;

        if let Some(total_bytes) = total_bytes {
            let fraction = (downloaded_bytes as f32 / total_bytes as f32).clamp(0.0, 1.0);
            // Only report when the whole-number percentage changes to avoid notifying the UI on every chunk.
            let percent = (fraction * 100.0) as u8;
            if last_reported_percent != Some(percent) {
                last_reported_percent = Some(percent);
                on_progress(Some(fraction));
            }
        }
    }
    target_file.flush().await?;
    if total_bytes.is_some() && last_reported_percent != Some(100) {
        on_progress(Some(1.0));
    }
    log::info!("downloaded update. path:{:?}", target_path);

    Ok(hasher.finish_hex())
}

async fn install_release_linux(
    temp_dir: &InstallerDir,
    downloaded_tar_gz: &Path,
    channel: &str,
    running_app_path: PathBuf,
) -> Result<Option<PathBuf>> {
    let home_dir = PathBuf::from(env::var("HOME").context("no HOME env var set")?);

    let extracted = temp_dir.path().join("zed");
    fs::create_dir_all(&extracted)
        .await
        .context("failed to create directory into which to extract update")?;

    let mut cmd = new_command("tar");
    cmd.arg("-xzf")
        .arg(&downloaded_tar_gz)
        .arg("-C")
        .arg(&extracted);
    let output = cmd
        .output()
        .await
        .with_context(|| "failed to extract: {cmd}")?;

    anyhow::ensure!(
        output.status.success(),
        "failed to extract {:?} to {:?}: {:?}",
        downloaded_tar_gz,
        extracted,
        String::from_utf8_lossy(&output.stderr)
    );

    let suffix = if channel != "stable" {
        format!("-{}", channel)
    } else {
        String::default()
    };
    let app_folder_name = format!("zed{}.app", suffix);

    let from = extracted.join(&app_folder_name);
    let mut to = home_dir.join(".local");

    let expected_suffix = format!("{}/libexec/zed-editor", app_folder_name);

    if let Some(prefix) = running_app_path
        .to_str()
        .and_then(|str| str.strip_suffix(&expected_suffix))
    {
        to = PathBuf::from(prefix);
    }

    let mut cmd = new_command("rsync");
    cmd.args(["-av", "--delete"]).arg(&from).arg(&to);
    let output = cmd
        .output()
        .await
        .with_context(|| "failed to rsync: {cmd}")?;

    anyhow::ensure!(
        output.status.success(),
        "failed to copy OTerminal update from {:?} to {:?}: {:?}",
        from,
        to,
        String::from_utf8_lossy(&output.stderr)
    );

    Ok(Some(to.join(expected_suffix)))
}

async fn install_release_macos(
    temp_dir: &InstallerDir,
    downloaded_dmg: &Path,
    running_app_path: PathBuf,
    background_executor: &BackgroundExecutor,
) -> Result<Option<PathBuf>> {
    let running_app_filename = running_app_path
        .file_name()
        .with_context(|| format!("invalid running app path {running_app_path:?}"))?;

    let mount_path = temp_dir.path().join("Zed");
    let mut mounted_app_path: OsString = mount_path.join(running_app_filename).into();

    mounted_app_path.push("/");
    let mut cmd = new_command("hdiutil");
    cmd.args(["attach", "-nobrowse"])
        .arg(&downloaded_dmg)
        .arg("-mountroot")
        .arg(temp_dir.path());
    let output = cmd
        .output()
        .await
        .with_context(|| "failed to mount: {cmd}")?;

    anyhow::ensure!(
        output.status.success(),
        "failed to mount: {:?}",
        String::from_utf8_lossy(&output.stderr)
    );

    let unmounter = MacOsUnmounter {
        mount_path: mount_path.clone(),
        background_executor,
    };

    let mut cmd = new_command("rsync");
    cmd.args(["-av", "--delete", "--exclude", "Icon?"])
        .arg(&mounted_app_path)
        .arg(&running_app_path);
    let rsync_output = cmd.output().await;

    // Await the unmount (even if rsync failed) so that the installer temp dir
    // can be deleted once this function returns.
    unmounter.unmount().await;

    let output = rsync_output.with_context(|| "failed to rsync: {cmd}")?;

    anyhow::ensure!(
        output.status.success(),
        "failed to copy app: {:?}",
        String::from_utf8_lossy(&output.stderr)
    );

    Ok(None)
}

/// Removes stale installer dirs from the system temp dir. Older Zed versions
/// leaked one per update by deleting the dir while the downloaded disk image
/// was still mounted inside it, which made the deletion fail silently.
#[cfg(any(rust_analyzer, all(not(target_os = "windows"), not(test))))]
async fn cleanup_stale_installer_dirs() {
    const STALE_INSTALLER_DIR_AGE: Duration = Duration::from_secs(24 * 60 * 60);

    let temp_dir = std::env::temp_dir();
    let Ok(mut entries) = fs::read_dir(&temp_dir).await else {
        log::warn!("failed to read temp dir {temp_dir:?} while cleaning up installer dirs");
        return;
    };
    while let Some(entry) = entries.next().await {
        let Ok(entry) = entry else {
            continue;
        };
        if !entry
            .file_name()
            .to_string_lossy()
            .starts_with(INSTALLER_DIR_PREFIX)
        {
            continue;
        }
        // Leave recent dirs alone, as they may belong to an update currently
        // in progress in another Zed instance.
        let is_stale = entry.metadata().await.ok().is_some_and(|metadata| {
            metadata.is_dir()
                && metadata.modified().ok().is_some_and(|modified| {
                    SystemTime::now()
                        .duration_since(modified)
                        .is_ok_and(|age| age > STALE_INSTALLER_DIR_AGE)
                })
        });
        if is_stale {
            if let Err(error) = fs::remove_dir_all(entry.path()).await {
                log::warn!(
                    "failed to remove stale installer dir {:?}: {error}",
                    entry.path()
                );
            } else {
                log::info!("removed stale installer dir {:?}", entry.path());
            }
        }
    }
}

async fn cleanup_windows() -> Result<()> {
    let parent = std::env::current_exe()?
        .parent()
        .context("No parent dir for oterminal.exe")?
        .to_owned();

    // keep in sync with crates/auto_update_helper/src/updater.rs
    _ = smol::fs::remove_dir(parent.join("updates")).await;
    _ = smol::fs::remove_dir(parent.join("install")).await;
    _ = smol::fs::remove_dir(parent.join("old")).await;

    Ok(())
}

/// Arguments for running the OTerminal Inno Setup installer as an update.
///
/// `/update=true` makes `zed.iss` stage the new binaries in `<app>\install`
/// and write `<app>\updates\versions.txt`; `tools\auto_update_helper.exe`
/// then swaps them in once OTerminal exits. `/DIR` pins the install to the
/// running copy, and `/NOCLOSEAPPLICATIONS` keeps the installer from closing
/// OTerminal: the user decides when to restart.
fn windows_installer_args(app_dir: &Path, log_path: &Path) -> Vec<OsString> {
    let mut dir_arg = OsString::from("/DIR=");
    dir_arg.push(app_dir);
    let mut log_arg = OsString::from("/LOG=");
    log_arg.push(log_path);
    [
        "/VERYSILENT",
        "/SUPPRESSMSGBOXES",
        "/NORESTART",
        "/NOCLOSEAPPLICATIONS",
        "/SP-",
        "/update=true",
        "/MERGETASKS=!desktopicon",
    ]
    .into_iter()
    .map(OsString::from)
    .chain([dir_arg, log_arg])
    .collect()
}

async fn install_release_windows(downloaded_installer: &Path) -> Result<Option<PathBuf>> {
    let app_dir = std::env::current_exe()?
        .parent()
        .context("No parent dir for oterminal.exe")?
        .to_owned();
    let log_path = downloaded_installer.with_extension("log");
    let mut cmd = new_command(downloaded_installer);
    cmd.args(windows_installer_args(&app_dir, &log_path));
    let output = cmd.output().await?;
    anyhow::ensure!(
        output.status.success(),
        "the OTerminal installer failed with {} (see {}): {:?}",
        output.status,
        log_path.display(),
        String::from_utf8_lossy(&output.stderr)
    );
    anyhow::ensure!(
        app_dir.join("updates").join("versions.txt").exists(),
        "the OTerminal installer did not stage the update (see {})",
        log_path.display()
    );
    // We return the path to the update helper program, because it will
    // perform the final steps of the update process, copying the new binary,
    // deleting the old one, and launching the new binary.
    let helper_path = app_dir.join("tools").join("auto_update_helper.exe");
    Ok(Some(helper_path))
}

pub async fn finalize_auto_update_on_quit() {
    let Some(installer_path) = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|p| p.join("updates")))
    else {
        return;
    };

    // The installer will create a flag file after it finishes updating
    let flag_file = installer_path.join("versions.txt");
    if flag_file.exists()
        && let Some(helper) = installer_path
            .parent()
            .map(|p| p.join("tools").join("auto_update_helper.exe"))
    {
        let mut command = util::command::new_command(helper);
        command.arg("--launch");
        command.arg("false");
        if let Ok(mut cmd) = command.spawn() {
            _ = cmd.status().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use client::Client;
    use clock::FakeSystemClock;
    use futures::channel::oneshot;
    use gpui::TestAppContext;
    use http_client::{FakeHttpClient, Response};
    use settings::default_settings;
    use std::{
        rc::Rc,
        sync::{
            Arc,
            atomic::{self, AtomicBool, AtomicUsize},
        },
    };
    use tempfile::tempdir;

    #[ctor::ctor(unsafe)]
    fn init_logger() {
        zlog::init_test();
    }

    use super::*;

    pub(super) struct InstallOverride(pub Rc<dyn Fn(&Path, &AsyncApp) -> Result<Option<PathBuf>>>);
    impl Global for InstallOverride {}

    /// Drives the executors until `condition` holds. File I/O in the update
    /// path completes on real threads, so a single `run_until_parked` is not
    /// always enough.
    async fn run_until(
        cx: &mut TestAppContext,
        mut condition: impl FnMut(&mut TestAppContext) -> bool,
    ) {
        for _ in 0..10_000 {
            if condition(cx) {
                return;
            }
            cx.background_executor.timer(Duration::from_millis(0)).await;
            cx.run_until_parked();
            std::thread::yield_now();
        }
        panic!("condition was never met");
    }

    fn idle_after_check(updater: &Entity<AutoUpdater>, cx: &mut TestAppContext) -> bool {
        updater.read_with(cx, |updater, _| {
            updater.pending_poll.is_none() && updater.status() == AutoUpdateStatus::Idle
        })
    }

    #[gpui::test]
    fn test_auto_update_defaults_to_true(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let mut store = SettingsStore::new(cx, &settings::default_settings());
            store
                .set_default_settings(&default_settings(), cx)
                .expect("Unable to set default settings");
            store
                .set_user_settings("{}", cx)
                .expect("Unable to set user settings");
            cx.set_global(store);
            assert!(AutoUpdateSetting::get_global(cx).0);
        });
    }

    #[gpui::test]
    async fn test_auto_update_downloads(cx: &mut TestAppContext) {
        cx.background_executor.allow_parking();
        zlog::init_test();
        let release_available = Arc::new(AtomicBool::new(false));
        let not_modified_responses = Arc::new(AtomicUsize::new(0));

        const UPDATE_CONTENTS: &str = "<fake-oterminal-update>";
        let (installer_tx, installer_rx) = oneshot::channel::<String>();

        cx.update(|cx| {
            settings::init(cx);

            let current_version = semver::Version::new(2, 0, 0);
            release_channel::init_test(current_version, ReleaseChannel::Stable, cx);

            let asset_name = Platform::current()
                .asset_name(&semver::Version::new(2, 0, 1))
                .expect("tests run on a supported platform");
            let digest = github_release::sha256_hex(UPDATE_CONTENTS.as_bytes());
            // The VS Code line's newer-looking v1.110.40 must be ignored.
            let old_releases = r#"[
                {"tag_name": "v1.110.40", "html_url": "https://github.com/x/y/releases/tag/v1.110.40",
                 "draft": false, "prerelease": false,
                 "assets": [{"name": "oterminal-1.110.40-win32-x64-user-setup.exe",
                             "browser_download_url": "https://test.example/vscode", "size": 1}]},
                {"tag_name": "v2.0.0", "html_url": "https://github.com/x/y/releases/tag/v2.0.0",
                 "draft": false, "prerelease": true, "assets": []}
            ]"#
            .to_string();
            let new_releases = format!(
                r#"[
                {{"tag_name": "v2.0.1", "html_url": "https://github.com/x/y/releases/tag/v2.0.1",
                  "draft": false, "prerelease": true,
                  "assets": [{{"name": "{asset_name}",
                               "browser_download_url": "https://test.example/new-download",
                               "size": {size}, "digest": "sha256:{digest}"}}]}}
            ]"#,
                size = UPDATE_CONTENTS.len()
            );

            let clock = Arc::new(FakeSystemClock::new());
            let release_available = Arc::clone(&release_available);
            let not_modified_responses = Arc::clone(&not_modified_responses);
            let installer_rx = Arc::new(parking_lot::Mutex::new(Some(installer_rx)));
            let fake_client_http = FakeHttpClient::create(move |req| {
                let release_available = release_available.load(atomic::Ordering::Relaxed);
                let not_modified_responses = not_modified_responses.clone();
                let installer_rx = installer_rx.clone();
                let (old_releases, new_releases) = (old_releases.clone(), new_releases.clone());
                async move {
                    if req.uri().host() == Some("api.github.com")
                        && req.uri().path() == "/repos/OverTimeHosting/OTerminal-Rust/releases"
                    {
                        let (etag, body) = if release_available {
                            ("\"new\"", new_releases)
                        } else {
                            ("\"old\"", old_releases)
                        };
                        let if_none_match = req
                            .headers()
                            .get("if-none-match")
                            .and_then(|value| value.to_str().ok());
                        if if_none_match == Some(etag) {
                            not_modified_responses.fetch_add(1, atomic::Ordering::SeqCst);
                            return Ok(Response::builder().status(304).body("".into()).unwrap());
                        }
                        return Ok(Response::builder()
                            .status(200)
                            .header("etag", etag)
                            .body(body.into())
                            .unwrap());
                    } else if req.uri().path() == "/new-download" {
                        return Ok(Response::builder()
                            .status(200)
                            .body({
                                let installer_rx = installer_rx.lock().take().unwrap();
                                installer_rx.await.unwrap().into()
                            })
                            .unwrap());
                    }
                    Ok(Response::builder().status(404).body("".into()).unwrap())
                }
            });
            let client = Client::new(clock, fake_client_http, cx);
            crate::init(client, cx);
        });

        let auto_updater = cx.update(|cx| AutoUpdater::get(cx).expect("auto updater should exist"));

        // The startup check finds nothing newer than 2.0.0 and caches the ETag.
        run_until(cx, |cx| {
            idle_after_check(&auto_updater, cx)
                && auto_updater.read_with(cx, |updater, _| updater.github_cache.is_some())
        })
        .await;
        auto_updater.read_with(cx, |updater, _| {
            assert_eq!(updater.current_version(), semver::Version::new(2, 0, 0));
        });

        // A second check with nothing new is answered from the ETag cache.
        cx.background_executor.advance_clock(POLL_INTERVAL);
        run_until(cx, |cx| {
            not_modified_responses.load(atomic::Ordering::SeqCst) == 1
                && idle_after_check(&auto_updater, cx)
        })
        .await;

        release_available.store(true, atomic::Ordering::SeqCst);
        cx.background_executor.advance_clock(POLL_INTERVAL);
        cx.background_executor.run_until_parked();

        loop {
            cx.background_executor.timer(Duration::from_millis(0)).await;
            cx.run_until_parked();
            let status = auto_updater.read_with(cx, |updater, _| updater.status());
            if matches!(status, AutoUpdateStatus::Downloading { .. }) {
                break;
            }
        }
        let status = auto_updater.read_with(cx, |updater, _| updater.status());
        assert_eq!(
            status,
            AutoUpdateStatus::Downloading {
                version: semver::Version::new(2, 0, 1),
                progress: None,
            }
        );

        installer_tx.send(UPDATE_CONTENTS.to_owned()).unwrap();

        let tmp_dir = Arc::new(tempdir().unwrap());

        cx.update(|cx| {
            let tmp_dir = tmp_dir.clone();
            cx.set_global(InstallOverride(Rc::new(move |target_path, _cx| {
                let tmp_dir = tmp_dir.clone();
                let dest_path = tmp_dir.path().join("oterminal");
                std::fs::copy(&target_path, &dest_path)?;
                Ok(Some(dest_path))
            })));
        });

        loop {
            cx.background_executor.timer(Duration::from_millis(0)).await;
            cx.run_until_parked();
            let status = auto_updater.read_with(cx, |updater, _| updater.status());
            if !matches!(status, AutoUpdateStatus::Downloading { .. }) {
                break;
            }
        }
        let status = auto_updater.read_with(cx, |updater, _| updater.status());
        assert_eq!(
            status,
            AutoUpdateStatus::Updated {
                version: semver::Version::new(2, 0, 1)
            }
        );
        cx.update(|cx| {
            assert_eq!(
                pending_release_notes_url(cx).as_deref(),
                Some("https://github.com/x/y/releases/tag/v2.0.1")
            );
        });
        let will_restart = cx.expect_restart();
        cx.update(|cx| cx.restart());
        let (path, arguments) = will_restart.await.unwrap();
        assert!(arguments.is_empty());
        let path = path.unwrap();
        assert_eq!(path, tmp_dir.path().join("oterminal"));
        assert_eq!(std::fs::read_to_string(path).unwrap(), UPDATE_CONTENTS);
    }

    #[gpui::test]
    async fn test_auto_update_rejects_checksum_mismatch(cx: &mut TestAppContext) {
        cx.background_executor.allow_parking();
        let downloads = Arc::new(AtomicUsize::new(0));
        let downloads_in_handler = downloads.clone();

        cx.update(|cx| {
            settings::init(cx);
            release_channel::init_test(
                semver::Version::new(2, 0, 0),
                ReleaseChannel::Stable,
                cx,
            );
            let asset_name = Platform::current()
                .asset_name(&semver::Version::new(2, 0, 1))
                .unwrap();
            let wrong_digest = github_release::sha256_hex(b"something else");
            let releases = format!(
                r#"[{{"tag_name": "v2.0.1", "html_url": "https://github.com/x/y/releases/tag/v2.0.1",
                     "draft": false, "prerelease": true,
                     "assets": [{{"name": "{asset_name}",
                                  "browser_download_url": "https://test.example/tampered",
                                  "size": 8, "digest": "sha256:{wrong_digest}"}}]}}]"#
            );
            let fake_client_http = FakeHttpClient::create(move |req| {
                let releases = releases.clone();
                let downloads = downloads_in_handler.clone();
                async move {
                    if req.uri().path() == "/repos/OverTimeHosting/OTerminal-Rust/releases" {
                        return Ok(Response::builder().status(200).body(releases.into()).unwrap());
                    }
                    if req.uri().path() == "/tampered" {
                        downloads.fetch_add(1, atomic::Ordering::SeqCst);
                        return Ok(Response::builder()
                            .status(200)
                            .body("tampered".into())
                            .unwrap());
                    }
                    Ok(Response::builder().status(404).body("".into()).unwrap())
                }
            });
            let client = Client::new(Arc::new(FakeSystemClock::new()), fake_client_http, cx);
            crate::init(client, cx);
        });

        let auto_updater = cx.update(|cx| AutoUpdater::get(cx).unwrap());
        // The automatic check at startup fails quietly...
        run_until(cx, |cx| {
            downloads.load(atomic::Ordering::SeqCst) == 1 && idle_after_check(&auto_updater, cx)
        })
        .await;
        // ...a manual one reports why.
        cx.update(|cx| {
            auto_updater.update(cx, |updater, cx| updater.poll(UpdateCheckType::Manual, cx))
        });
        run_until(cx, |cx| {
            auto_updater.read_with(cx, |updater, _| {
                updater.pending_poll.is_none()
                    && !matches!(
                        updater.status(),
                        AutoUpdateStatus::Idle
                            | AutoUpdateStatus::Checking
                            | AutoUpdateStatus::Downloading { .. }
                    )
            })
        })
        .await;
        assert_eq!(downloads.load(atomic::Ordering::SeqCst), 2);
        auto_updater.read_with(cx, |updater, _| match updater.status() {
            AutoUpdateStatus::Errored { error } => assert!(
                format!("{error:#}").contains("checksum mismatch"),
                "unexpected error: {error:#}"
            ),
            status => panic!("a tampered download must not be installed, got {status:?}"),
        });
    }

    #[gpui::test]
    async fn test_auto_update_backs_off_when_rate_limited(cx: &mut TestAppContext) {
        cx.background_executor.allow_parking();
        let requests = Arc::new(AtomicUsize::new(0));

        cx.update(|cx| {
            settings::init(cx);
            release_channel::init_test(semver::Version::new(2, 0, 0), ReleaseChannel::Stable, cx);
            let requests = requests.clone();
            let fake_client_http = FakeHttpClient::create(move |_req| {
                requests.fetch_add(1, atomic::Ordering::SeqCst);
                async move {
                    Ok(Response::builder()
                        .status(403)
                        .header("x-ratelimit-remaining", "0")
                        .header("retry-after", "3600")
                        .body("rate limited".into())
                        .unwrap())
                }
            });
            let client = Client::new(Arc::new(FakeSystemClock::new()), fake_client_http, cx);
            crate::init(client, cx);
        });

        let auto_updater = cx.update(|cx| AutoUpdater::get(cx).unwrap());
        run_until(cx, |cx| {
            requests.load(atomic::Ordering::SeqCst) == 1 && idle_after_check(&auto_updater, cx)
        })
        .await;
        auto_updater.read_with(cx, |updater, _| {
            // Automatic checks stay quiet...
            assert_eq!(updater.status(), AutoUpdateStatus::Idle);
            assert!(updater.rate_limited_until.is_some());
        });

        // ...and a manual check during the backoff does not hit GitHub again.
        cx.update(|cx| {
            auto_updater.update(cx, |updater, cx| updater.poll(UpdateCheckType::Manual, cx))
        });
        run_until(cx, |cx| {
            auto_updater.read_with(cx, |updater, _| {
                updater.pending_poll.is_none()
                    && matches!(updater.status(), AutoUpdateStatus::Errored { .. })
            })
        })
        .await;
        assert_eq!(requests.load(atomic::Ordering::SeqCst), 1);
        auto_updater.read_with(cx, |updater, _| match updater.status() {
            AutoUpdateStatus::Errored { error } => {
                assert!(error.to_string().contains("rate limit"), "{error:#}")
            }
            status => panic!("expected a rate limit error, got {status:?}"),
        });
    }

    #[test]
    fn test_windows_installer_args() {
        let args = windows_installer_args(
            Path::new(r"C:\Users\Jane Doe\AppData\Local\Programs\OTerminal"),
            Path::new(r"C:\Users\Jane Doe\AppData\Local\Programs\OTerminal\updates\setup.log"),
        );
        let args: Vec<String> = args
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        for expected in [
            "/VERYSILENT",
            "/SUPPRESSMSGBOXES",
            "/NORESTART",
            "/NOCLOSEAPPLICATIONS",
            "/update=true",
            r"/DIR=C:\Users\Jane Doe\AppData\Local\Programs\OTerminal",
        ] {
            assert!(
                args.iter().any(|arg| arg == expected),
                "missing {expected} in {args:?}"
            );
        }
        assert!(args.iter().any(|arg| arg.starts_with("/LOG=")));
    }

    #[gpui::test]
    async fn test_download_release_reports_progress(cx: &mut TestAppContext) {
        cx.background_executor.allow_parking();

        let body = vec![0u8; 20_000];
        let content_length = body.len();

        let client = FakeHttpClient::create(move |_req| {
            let body = body.clone();
            async move {
                Ok(Response::builder()
                    .status(200)
                    .header(
                        http_client::http::header::CONTENT_LENGTH,
                        body.len().to_string(),
                    )
                    .body(body.into())
                    .unwrap())
            }
        });

        let temp_dir = tempdir().unwrap();
        let target_path = temp_dir.path().join("zed-download");
        let release = ReleaseAsset {
            version: "1.0.0".to_string(),
            url: "https://test.example/download".to_string(),
        };

        let reported = Rc::new(std::cell::RefCell::new(Vec::<f32>::new()));
        download_release(&target_path, release, client, {
            let reported = reported.clone();
            move |fraction| {
                if let Some(fraction) = fraction {
                    reported.borrow_mut().push(fraction);
                }
            }
        })
        .await
        .unwrap();

        let reported = reported.borrow();
        assert!(
            reported.len() >= 2,
            "expected progress to be reported across multiple reads, got {reported:?}"
        );
        assert_eq!(
            reported.last().copied(),
            Some(1.0),
            "download should finish at 100%"
        );
        for fraction in reported.iter() {
            assert!(
                (0.0..=1.0).contains(fraction),
                "progress {fraction} out of range"
            );
        }
        for pair in reported.windows(2) {
            assert!(
                pair[0] <= pair[1],
                "progress must not decrease: {reported:?}"
            );
        }

        let downloaded_len = std::fs::metadata(&target_path).unwrap().len();
        assert_eq!(downloaded_len, content_length as u64);
    }

    #[gpui::test]
    async fn test_download_release_without_content_length_reports_no_progress(
        cx: &mut TestAppContext,
    ) {
        cx.background_executor.allow_parking();

        let body = vec![0u8; 20_000];
        let content_length = body.len();

        let client = FakeHttpClient::create(move |_req| {
            let body = body.clone();
            async move { Ok(Response::builder().status(200).body(body.into()).unwrap()) }
        });

        let temp_dir = tempdir().unwrap();
        let target_path = temp_dir.path().join("zed-download");
        let release = ReleaseAsset {
            version: "1.0.0".to_string(),
            url: "https://test.example/download".to_string(),
        };

        let reported = Rc::new(std::cell::RefCell::new(Vec::<Option<f32>>::new()));
        download_release(&target_path, release, client, {
            let reported = reported.clone();
            move |fraction| {
                reported.borrow_mut().push(fraction);
            }
        })
        .await
        .unwrap();

        assert!(
            reported.borrow().is_empty(),
            "progress should not be reported when the total size is unknown, got {:?}",
            reported.borrow()
        );

        let downloaded_len = std::fs::metadata(&target_path).unwrap().len();
        assert_eq!(downloaded_len, content_length as u64);
    }
}
