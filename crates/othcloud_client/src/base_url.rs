const PRODUCTION_BASE_URL: &str = "https://othcloud.xyz";
const DEVELOPMENT_BASE_URL: &str = "http://localhost:3001";

/// The OTHCloud origin every request and page link is resolved against.
///
/// Debug builds talk to a local OTHCloud (`OTHCLOUD_DEV_BASE_URL`, default
/// `http://localhost:3001`). Release builds use othcloud.xyz; `OTHCLOUD_BASE_URL`
/// exists as an escape hatch for self-hosted or staging deployments.
pub fn base_url() -> String {
    let (variable, fallback) = if cfg!(debug_assertions) {
        ("OTHCLOUD_DEV_BASE_URL", DEVELOPMENT_BASE_URL)
    } else {
        ("OTHCLOUD_BASE_URL", PRODUCTION_BASE_URL)
    };
    std::env::var(variable)
        .ok()
        .map(|value| value.trim().trim_end_matches('/').to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| fallback.to_string())
}

/// Turns a site-relative path (as returned by the OTHCloud API) into a full URL.
/// Absolute URLs are returned unchanged.
pub fn absolute_url(path_or_url: &str) -> String {
    join_url(&base_url(), path_or_url)
}

pub(crate) fn join_url(base: &str, path_or_url: &str) -> String {
    if path_or_url.starts_with("http://") || path_or_url.starts_with("https://") {
        return path_or_url.to_string();
    }
    let base = base.trim_end_matches('/');
    if path_or_url.starts_with('/') {
        format!("{base}{path_or_url}")
    } else {
        format!("{base}/{path_or_url}")
    }
}

/// The host part of a base URL, used in "Can't reach …" messages.
pub fn host_of(url: &str) -> &str {
    let without_scheme = url.split_once("://").map_or(url, |(_, rest)| rest);
    without_scheme
        .split(['/', '?', '#'])
        .next()
        .unwrap_or(without_scheme)
}

/// Site-relative paths of OTHCloud pages the desktop app opens in the browser.
pub mod pages {
    pub fn pair() -> &'static str {
        "/desktop-pair"
    }

    pub fn github_connect() -> &'static str {
        "/desktop-github"
    }

    pub fn dashboard() -> &'static str {
        "/dashboard"
    }

    pub fn games() -> &'static str {
        "/dashboard/games"
    }

    pub fn services() -> &'static str {
        "/dashboard/services"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_join_url() {
        assert_eq!(
            join_url("https://othcloud.xyz", "/dashboard"),
            "https://othcloud.xyz/dashboard"
        );
        assert_eq!(
            join_url("https://othcloud.xyz/", "dashboard"),
            "https://othcloud.xyz/dashboard"
        );
        assert_eq!(
            join_url("https://othcloud.xyz", "https://github.com/x"),
            "https://github.com/x"
        );
        assert_eq!(
            join_url("http://localhost:3001", "http://other/x"),
            "http://other/x"
        );
    }

    #[test]
    fn test_host_of() {
        assert_eq!(host_of("https://othcloud.xyz"), "othcloud.xyz");
        assert_eq!(host_of("http://localhost:3001/api"), "localhost:3001");
        assert_eq!(host_of("othcloud.xyz"), "othcloud.xyz");
    }
}
