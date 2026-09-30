//! OTerminal: update discovery through GitHub Releases.
//!
//! Releases of the Rust (Zed-based) OTerminal are published by
//! `.github/workflows/rust-release.yml` to the same repository as the older
//! VS Code-based OTerminal. The two lines are told apart by their tags:
//! VS Code releases are tagged `v1.110.x`, Rust releases `v2.x.y` and later
//! (see [`MIN_RUST_LINE_MAJOR`]). Rust releases are also published as
//! pre-releases with asset names the VS Code updater does not match, so
//! neither line can pick up the other's installers.
//!
//! Everything in this module is pure (no I/O) so it can be unit tested.

use std::time::Duration;

use anyhow::{Context as _, Result};
use http_client::http::{HeaderMap, StatusCode};
use semver::Version;
use serde::Deserialize;
use sha2::{Digest as _, Sha256};

/// `owner/repo` the releases are published to.
pub const GITHUB_REPOSITORY: &str = "OverTimeHosting/Oterminal";

/// The releases listing (newest first). 50 per page is plenty: the VS Code
/// line is no longer released from here once the Rust line takes over, and
/// until then we only need the newest few Rust releases.
pub const RELEASES_API_URL: &str =
    "https://api.github.com/repos/OverTimeHosting/Oterminal/releases?per_page=50";

/// Human-facing releases page.
pub const RELEASES_PAGE_URL: &str = "https://github.com/OverTimeHosting/Oterminal/releases";

/// Tags below this major version belong to the VS Code-based OTerminal.
pub const MIN_RUST_LINE_MAJOR: u64 = 2;

/// Name of the checksum manifest uploaded with every release (`sha256sum` format).
pub const CHECKSUMS_ASSET_NAME: &str = "SHA256SUMS.txt";

/// Prefix of every downloadable OTerminal (Rust line) asset.
pub const ASSET_PREFIX: &str = "OTerminal-";

/// Lower and upper bounds for backing off after GitHub rate limits us.
const MIN_RATE_LIMIT_BACKOFF: Duration = Duration::from_secs(60);
const MAX_RATE_LIMIT_BACKOFF: Duration = Duration::from_secs(6 * 60 * 60);
const DEFAULT_RATE_LIMIT_BACKOFF: Duration = Duration::from_secs(30 * 60);

#[derive(Debug, Clone, Deserialize)]
pub struct GithubRelease {
    pub tag_name: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub html_url: String,
    #[serde(default)]
    pub draft: bool,
    #[serde(default)]
    pub prerelease: bool,
    #[serde(default)]
    pub assets: Vec<GithubAsset>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GithubAsset {
    pub name: String,
    pub browser_download_url: String,
    #[serde(default)]
    pub size: u64,
    /// GitHub computes this for every uploaded asset, e.g. `"sha256:ab12…"`.
    #[serde(default)]
    pub digest: Option<String>,
}

/// The release/asset the updater should install.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateCandidate {
    pub version: Version,
    pub tag: String,
    pub prerelease: bool,
    /// The release page, used for "View Release Notes" / manual downloads.
    pub html_url: String,
    pub asset_name: String,
    pub download_url: String,
    pub size: u64,
    /// SHA-256 from GitHub's asset `digest` field, lowercase hex.
    pub sha256_from_digest: Option<String>,
    /// URL of the release's `SHA256SUMS.txt`, if it has one.
    pub checksums_url: Option<String>,
}

/// An operating system / CPU pair, using Rust's `std::env::consts` names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Platform {
    pub os: &'static str,
    pub arch: &'static str,
}

impl Platform {
    pub fn current() -> Self {
        Self {
            os: std::env::consts::OS,
            arch: std::env::consts::ARCH,
        }
    }

    /// The suffix (after `OTerminal-<version>`) of this platform's update
    /// asset. Keep in sync with the release workflow.
    ///
    /// The Windows name intentionally does not end in `win32-x64-user-setup.exe`,
    /// which is what the VS Code-based OTerminal's updater looks for.
    pub fn asset_suffix(&self) -> Option<String> {
        let arch = self.arch;
        match self.os {
            "windows" => Some(format!("-windows-{arch}-setup.exe")),
            "macos" => Some(format!("-macos-{arch}.dmg")),
            "linux" => Some(format!("-linux-{arch}.tar.gz")),
            _ => None,
        }
    }

    /// The full asset file name for `version` on this platform.
    pub fn asset_name(&self, version: &Version) -> Option<String> {
        Some(format!("{ASSET_PREFIX}{version}{}", self.asset_suffix()?))
    }
}

pub fn parse_releases(json: &[u8]) -> Result<Vec<GithubRelease>> {
    serde_json::from_slice(json).context("failed to parse the GitHub releases response")
}

/// Parses a Rust-line release tag (`v2.0.3`, `2.1.0-beta.1`). Returns `None`
/// for anything else, in particular the VS Code line's `v1.110.x` tags.
pub fn rust_line_version(tag: &str) -> Option<Version> {
    let version: Version = tag.trim().strip_prefix('v').unwrap_or(tag).parse().ok()?;
    (version.major >= MIN_RUST_LINE_MAJOR).then_some(version)
}

/// Whether `candidate` should replace `current`. Build metadata is ignored,
/// pre-release identifiers are respected (`2.1.0-beta.1 < 2.1.0`).
pub fn is_newer(candidate: &Version, current: &Version) -> bool {
    let strip = |version: &Version| {
        let mut version = version.clone();
        version.build = semver::BuildMetadata::EMPTY;
        version
    };
    strip(candidate) > strip(current)
}

/// Picks the newest installable Rust-line release for `platform`, if it is
/// newer than `current`.
pub fn select_update(
    releases: &[GithubRelease],
    current: &Version,
    include_prereleases: bool,
    platform: Platform,
) -> Option<UpdateCandidate> {
    let suffix = platform.asset_suffix()?.to_ascii_lowercase();

    releases
        .iter()
        .filter(|release| !release.draft)
        .filter(|release| include_prereleases || !release.prerelease)
        .filter_map(|release| {
            let version = rust_line_version(&release.tag_name)?;
            if !include_prereleases && !version.pre.is_empty() {
                return None;
            }
            let asset = release.assets.iter().find(|asset| {
                let name = asset.name.to_ascii_lowercase();
                name.starts_with(&ASSET_PREFIX.to_ascii_lowercase()) && name.ends_with(&suffix)
            })?;
            let checksums_url = release
                .assets
                .iter()
                .find(|asset| asset.name.eq_ignore_ascii_case(CHECKSUMS_ASSET_NAME))
                .map(|asset| asset.browser_download_url.clone());
            Some(UpdateCandidate {
                version,
                tag: release.tag_name.clone(),
                prerelease: release.prerelease,
                html_url: release.html_url.clone(),
                asset_name: asset.name.clone(),
                download_url: asset.browser_download_url.clone(),
                size: asset.size,
                sha256_from_digest: asset.digest.as_deref().and_then(parse_sha256_digest),
                checksums_url,
            })
        })
        .max_by(|a, b| a.version.cmp(&b.version))
        .filter(|candidate| is_newer(&candidate.version, current))
}

/// Parses GitHub's asset digest (`sha256:<hex>`) into lowercase hex.
pub fn parse_sha256_digest(digest: &str) -> Option<String> {
    let hex = digest.trim().strip_prefix("sha256:")?;
    is_sha256_hex(hex).then(|| hex.to_ascii_lowercase())
}

/// Finds `asset_name`'s hash in a `sha256sum`-style manifest
/// (`<hex>  <name>` or `<hex> *<name>`).
pub fn parse_checksums_file(contents: &str, asset_name: &str) -> Option<String> {
    contents.lines().find_map(|line| {
        let mut parts = line.split_whitespace();
        let hash = parts.next()?;
        let name = parts.next()?.trim_start_matches('*');
        (name == asset_name && is_sha256_hex(hash)).then(|| hash.to_ascii_lowercase())
    })
}

fn is_sha256_hex(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// Combines the two possible sources of the expected hash. Fails when there
/// is none, or when they disagree.
pub fn expected_sha256(
    from_digest: Option<&str>,
    from_checksums_file: Option<&str>,
) -> Result<String> {
    match (from_digest, from_checksums_file) {
        (Some(digest), Some(file)) => {
            anyhow::ensure!(
                digest.eq_ignore_ascii_case(file),
                "the release's SHA256SUMS.txt ({file}) disagrees with GitHub's asset digest ({digest})"
            );
            Ok(digest.to_ascii_lowercase())
        }
        (Some(hash), None) | (None, Some(hash)) => Ok(hash.to_ascii_lowercase()),
        (None, None) => {
            anyhow::bail!("the release has no SHA-256 checksum for this asset; refusing to install")
        }
    }
}

/// Incremental SHA-256, fed while downloading.
#[derive(Default)]
pub struct Sha256Hasher(Sha256);

impl Sha256Hasher {
    pub fn update(&mut self, bytes: &[u8]) {
        self.0.update(bytes);
    }

    pub fn finish_hex(self) -> String {
        format!("{:x}", self.0.finalize())
    }
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256Hasher::default();
    hasher.update(bytes);
    hasher.finish_hex()
}

pub fn verify_sha256(actual: &str, expected: &str) -> Result<()> {
    anyhow::ensure!(
        actual.eq_ignore_ascii_case(expected),
        "checksum mismatch for the downloaded update: expected {expected}, got {actual}"
    );
    Ok(())
}

/// If `status`/`headers` say GitHub rate limited us, how long to wait before
/// asking again. `now_unix_secs` is the current Unix time.
pub fn rate_limit_backoff(
    status: StatusCode,
    headers: &HeaderMap,
    now_unix_secs: u64,
) -> Option<Duration> {
    let header = |name: &str| {
        headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(str::trim)
    };
    let retry_after = header("retry-after").and_then(|value| value.parse::<u64>().ok());
    let remaining = header("x-ratelimit-remaining").and_then(|value| value.parse::<u64>().ok());
    let reset = header("x-ratelimit-reset").and_then(|value| value.parse::<u64>().ok());

    let is_rate_limited = status == StatusCode::TOO_MANY_REQUESTS
        || (status == StatusCode::FORBIDDEN && (remaining == Some(0) || retry_after.is_some()));
    if !is_rate_limited {
        return None;
    }

    let wait = if let Some(seconds) = retry_after {
        Duration::from_secs(seconds)
    } else if let Some(reset) = reset {
        Duration::from_secs(reset.saturating_sub(now_unix_secs))
    } else {
        DEFAULT_RATE_LIMIT_BACKOFF
    };
    Some(wait.clamp(MIN_RATE_LIMIT_BACKOFF, MAX_RATE_LIMIT_BACKOFF))
}

#[cfg(test)]
mod tests {
    use super::*;
    use http_client::http::HeaderValue;

    const WINDOWS_X64: Platform = Platform {
        os: "windows",
        arch: "x86_64",
    };

    /// Trimmed-down but structurally faithful `GET /repos/{owner}/{repo}/releases` response.
    const RELEASES_JSON: &str = r#"[
      {
        "url": "https://api.github.com/repos/OverTimeHosting/Oterminal/releases/3",
        "html_url": "https://github.com/OverTimeHosting/Oterminal/releases/tag/v1.110.34",
        "tag_name": "v1.110.34",
        "name": "OTerminal 1.110.34",
        "draft": false,
        "prerelease": false,
        "published_at": "2026-09-29T10:00:00Z",
        "assets": [
          {
            "name": "oterminal-1.110.34-win32-x64-user-setup.exe",
            "browser_download_url": "https://github.com/OverTimeHosting/Oterminal/releases/download/v1.110.34/oterminal-1.110.34-win32-x64-user-setup.exe",
            "size": 120000000,
            "digest": "sha256:1111111111111111111111111111111111111111111111111111111111111111"
          }
        ]
      },
      {
        "html_url": "https://github.com/OverTimeHosting/Oterminal/releases/tag/v2.0.3",
        "tag_name": "v2.0.3",
        "name": "OTerminal 2.0.3",
        "draft": true,
        "prerelease": true,
        "assets": [
          {
            "name": "OTerminal-2.0.3-windows-x86_64-setup.exe",
            "browser_download_url": "https://example.invalid/draft.exe",
            "size": 1
          }
        ]
      },
      {
        "html_url": "https://github.com/OverTimeHosting/Oterminal/releases/tag/v2.0.2",
        "tag_name": "v2.0.2",
        "name": "OTerminal 2.0.2",
        "draft": false,
        "prerelease": true,
        "body": "- fix things",
        "assets": [
          {
            "name": "OTerminal-2.0.2-windows-x86_64-setup.exe",
            "browser_download_url": "https://github.com/OverTimeHosting/Oterminal/releases/download/v2.0.2/OTerminal-2.0.2-windows-x86_64-setup.exe",
            "size": 95000000,
            "digest": "sha256:ABCDEFabcdef0123456789abcdef0123456789abcdef0123456789abcdef0123"
          },
          {
            "name": "SHA256SUMS.txt",
            "browser_download_url": "https://github.com/OverTimeHosting/Oterminal/releases/download/v2.0.2/SHA256SUMS.txt",
            "size": 100,
            "digest": null
          }
        ]
      },
      {
        "html_url": "https://github.com/OverTimeHosting/Oterminal/releases/tag/v2.0.1",
        "tag_name": "v2.0.1",
        "draft": false,
        "prerelease": false,
        "assets": [
          {
            "name": "OTerminal-2.0.1-windows-x86_64-setup.exe",
            "browser_download_url": "https://github.com/OverTimeHosting/Oterminal/releases/download/v2.0.1/OTerminal-2.0.1-windows-x86_64-setup.exe",
            "size": 94000000
          }
        ]
      },
      {
        "html_url": "https://github.com/OverTimeHosting/Oterminal/releases/tag/v1.110.33",
        "tag_name": "v1.110.33",
        "draft": false,
        "prerelease": false,
        "assets": []
      }
    ]"#;

    fn releases() -> Vec<GithubRelease> {
        parse_releases(RELEASES_JSON.as_bytes()).unwrap()
    }

    #[test]
    fn test_parse_releases() {
        let releases = releases();
        assert_eq!(releases.len(), 5);
        assert_eq!(releases[0].tag_name, "v1.110.34");
        assert!(releases[1].draft);
        assert!(releases[2].prerelease);
        assert_eq!(releases[2].assets.len(), 2);
        assert_eq!(releases[2].assets[1].digest, None);
        assert_eq!(releases[3].name, None);
        assert!(parse_releases(b"{\"message\":\"Not Found\"}").is_err());
    }

    #[test]
    fn test_rust_line_version_ignores_vscode_tags() {
        assert_eq!(rust_line_version("v2.0.0"), Some(Version::new(2, 0, 0)));
        assert_eq!(rust_line_version("2.3.4"), Some(Version::new(2, 3, 4)));
        assert_eq!(rust_line_version("v10.0.1"), Some(Version::new(10, 0, 1)));
        assert_eq!(
            rust_line_version("v2.1.0-beta.1"),
            Some("2.1.0-beta.1".parse().unwrap())
        );
        assert_eq!(rust_line_version("v1.110.33"), None);
        assert_eq!(rust_line_version("v1.999.0"), None);
        assert_eq!(rust_line_version("v0.1.0"), None);
        assert_eq!(rust_line_version("nightly"), None);
        assert_eq!(rust_line_version("v2.0"), None);
    }

    #[test]
    fn test_select_update_picks_newest_rust_release_with_asset() {
        let candidate = select_update(&releases(), &Version::new(2, 0, 0), true, WINDOWS_X64)
            .expect("an update should be available");
        // v1.110.34 is ignored (VS Code line) and the v2.0.3 draft is skipped.
        assert_eq!(candidate.version, Version::new(2, 0, 2));
        assert_eq!(candidate.tag, "v2.0.2");
        assert!(candidate.prerelease);
        assert_eq!(
            candidate.asset_name,
            "OTerminal-2.0.2-windows-x86_64-setup.exe"
        );
        assert_eq!(candidate.size, 95000000);
        assert_eq!(
            candidate.html_url,
            "https://github.com/OverTimeHosting/Oterminal/releases/tag/v2.0.2"
        );
        assert_eq!(
            candidate.sha256_from_digest.as_deref(),
            Some("abcdefabcdef0123456789abcdef0123456789abcdef0123456789abcdef0123")
        );
        assert_eq!(
            candidate.checksums_url.as_deref(),
            Some(
                "https://github.com/OverTimeHosting/Oterminal/releases/download/v2.0.2/SHA256SUMS.txt"
            )
        );
    }

    #[test]
    fn test_select_update_respects_prerelease_setting() {
        let candidate = select_update(&releases(), &Version::new(2, 0, 0), false, WINDOWS_X64)
            .expect("the stable v2.0.1 should be offered");
        assert_eq!(candidate.version, Version::new(2, 0, 1));
        assert_eq!(candidate.sha256_from_digest, None);
        assert_eq!(candidate.checksums_url, None);
    }

    #[test]
    fn test_select_update_requires_newer_version() {
        assert_eq!(
            select_update(&releases(), &Version::new(2, 0, 2), true, WINDOWS_X64),
            None
        );
        assert_eq!(
            select_update(&releases(), &Version::new(2, 5, 0), true, WINDOWS_X64),
            None
        );
        // Build metadata on the running version must not block an update.
        let mut current = Version::new(2, 0, 1);
        current.build = semver::BuildMetadata::new("stable.abc123").unwrap();
        assert_eq!(
            select_update(&releases(), &current, true, WINDOWS_X64).map(|c| c.version),
            Some(Version::new(2, 0, 2))
        );
    }

    #[test]
    fn test_select_update_never_offers_vscode_line_to_old_versions() {
        // Even a (hypothetical) Rust build reporting 1.0.0 only sees v2+ releases.
        let candidate =
            select_update(&releases(), &Version::new(1, 0, 0), true, WINDOWS_X64).unwrap();
        assert_eq!(candidate.version, Version::new(2, 0, 2));
    }

    #[test]
    fn test_select_update_asset_selection_by_platform() {
        let windows_arm = Platform {
            os: "windows",
            arch: "aarch64",
        };
        let linux = Platform {
            os: "linux",
            arch: "x86_64",
        };
        let freebsd = Platform {
            os: "freebsd",
            arch: "x86_64",
        };
        for platform in [windows_arm, linux, freebsd] {
            assert_eq!(
                select_update(&releases(), &Version::new(2, 0, 0), true, platform),
                None,
                "{platform:?} has no asset"
            );
        }
        assert_eq!(
            WINDOWS_X64.asset_name(&Version::new(2, 0, 5)).as_deref(),
            Some("OTerminal-2.0.5-windows-x86_64-setup.exe")
        );
        assert_eq!(
            linux.asset_name(&Version::new(2, 0, 5)).as_deref(),
            Some("OTerminal-2.0.5-linux-x86_64.tar.gz")
        );
        assert_eq!(freebsd.asset_name(&Version::new(2, 0, 5)), None);
        // The VS Code updater matches /win32-x64-user-setup\.exe$/i; ours must not.
        assert!(
            !WINDOWS_X64
                .asset_name(&Version::new(2, 0, 5))
                .unwrap()
                .to_ascii_lowercase()
                .ends_with("win32-x64-user-setup.exe")
        );
    }

    #[test]
    fn test_version_comparison() {
        let v = |s: &str| s.parse::<Version>().unwrap();
        assert!(is_newer(&v("2.0.1"), &v("2.0.0")));
        assert!(is_newer(&v("2.1.0"), &v("2.0.99")));
        assert!(is_newer(&v("10.0.0"), &v("9.9.9")));
        assert!(is_newer(&v("2.1.0-beta.2"), &v("2.1.0-beta.1")));
        assert!(is_newer(&v("2.1.0"), &v("2.1.0-beta.2")));
        assert!(!is_newer(&v("2.1.0-beta.2"), &v("2.1.0")));
        assert!(!is_newer(&v("2.0.0"), &v("2.0.0")));
        assert!(!is_newer(&v("2.0.0+ci.5"), &v("2.0.0+stable.abc")));
        assert!(!is_newer(&v("1.110.40"), &v("2.0.0")));
    }

    #[test]
    fn test_sha256_helpers() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        let mut hasher = Sha256Hasher::default();
        hasher.update(b"a");
        hasher.update(b"bc");
        assert_eq!(hasher.finish_hex(), sha256_hex(b"abc"));

        let expected = sha256_hex(b"abc");
        assert!(verify_sha256(&expected, &expected.to_ascii_uppercase()).is_ok());
        assert!(verify_sha256(&sha256_hex(b"abd"), &expected).is_err());

        assert_eq!(
            parse_sha256_digest(
                "sha256:BA7816BF8F01CFEA414140DE5DAE2223B00361A396177A9CB410FF61F20015AD"
            )
            .as_deref(),
            Some(expected.as_str())
        );
        assert_eq!(parse_sha256_digest("sha512:abc"), None);
        assert_eq!(parse_sha256_digest("sha256:xyz"), None);
    }

    #[test]
    fn test_parse_checksums_file() {
        let hash_a = sha256_hex(b"a");
        let hash_b = sha256_hex(b"b");
        let contents = format!(
            "{hash_a}  OTerminal-2.0.2-windows-x86_64-setup.exe\n{hash_b} *OTerminal-2.0.2-linux-x86_64.tar.gz\n\nnot a line\n"
        );
        assert_eq!(
            parse_checksums_file(&contents, "OTerminal-2.0.2-windows-x86_64-setup.exe"),
            Some(hash_a.clone())
        );
        assert_eq!(
            parse_checksums_file(&contents, "OTerminal-2.0.2-linux-x86_64.tar.gz"),
            Some(hash_b.clone())
        );
        assert_eq!(parse_checksums_file(&contents, "missing.exe"), None);
        // CRLF manifests (written on Windows) still parse.
        let crlf = contents.replace('\n', "\r\n");
        assert_eq!(
            parse_checksums_file(&crlf, "OTerminal-2.0.2-windows-x86_64-setup.exe"),
            Some(hash_a.clone())
        );

        assert_eq!(expected_sha256(Some(&hash_a), None).unwrap(), hash_a);
        assert_eq!(expected_sha256(None, Some(&hash_b)).unwrap(), hash_b);
        assert_eq!(
            expected_sha256(Some(&hash_a), Some(&hash_a.to_ascii_uppercase())).unwrap(),
            hash_a
        );
        assert!(expected_sha256(Some(&hash_a), Some(&hash_b)).is_err());
        assert!(expected_sha256(None, None).is_err());
    }

    #[test]
    fn test_rate_limit_backoff() {
        let now = 1_000_000;
        let headers = |pairs: &[(&'static str, &str)]| {
            let mut map = HeaderMap::new();
            for (name, value) in pairs {
                map.insert(*name, HeaderValue::from_str(value).unwrap());
            }
            map
        };

        assert_eq!(rate_limit_backoff(StatusCode::OK, &headers(&[]), now), None);
        // A plain 403 (e.g. blocked) is not a rate limit.
        assert_eq!(
            rate_limit_backoff(
                StatusCode::FORBIDDEN,
                &headers(&[("x-ratelimit-remaining", "12")]),
                now
            ),
            None
        );
        // Primary rate limit: wait until the reset time.
        assert_eq!(
            rate_limit_backoff(
                StatusCode::FORBIDDEN,
                &headers(&[
                    ("x-ratelimit-remaining", "0"),
                    ("x-ratelimit-reset", "1001200")
                ]),
                now
            ),
            Some(Duration::from_secs(1200))
        );
        // Secondary rate limit: honour Retry-After, but never less than a minute.
        assert_eq!(
            rate_limit_backoff(
                StatusCode::TOO_MANY_REQUESTS,
                &headers(&[("retry-after", "5")]),
                now
            ),
            Some(Duration::from_secs(60))
        );
        assert_eq!(
            rate_limit_backoff(
                StatusCode::FORBIDDEN,
                &headers(&[("retry-after", "600")]),
                now
            ),
            Some(Duration::from_secs(600))
        );
        // Unknown reset: default backoff; absurd reset: capped.
        assert_eq!(
            rate_limit_backoff(StatusCode::TOO_MANY_REQUESTS, &headers(&[]), now),
            Some(DEFAULT_RATE_LIMIT_BACKOFF)
        );
        assert_eq!(
            rate_limit_backoff(
                StatusCode::FORBIDDEN,
                &headers(&[
                    ("x-ratelimit-remaining", "0"),
                    ("x-ratelimit-reset", "99999999999")
                ]),
                now
            ),
            Some(MAX_RATE_LIMIT_BACKOFF)
        );
    }
}
