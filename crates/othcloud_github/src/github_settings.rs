use settings::{RegisterSetting, Settings, SettingsContent};

/// `"othcloud"` settings used by the GitHub integration.
#[derive(Clone, Debug, Default, RegisterSetting)]
pub struct OthcloudGithubSettings {
    /// Client id of a GitHub OAuth App with device flow enabled, if configured.
    pub github_oauth_client_id: Option<String>,
}

impl Settings for OthcloudGithubSettings {
    fn from_settings(content: &SettingsContent) -> Self {
        Self {
            github_oauth_client_id: content
                .othcloud
                .as_ref()
                .and_then(|othcloud| othcloud.github_oauth_client_id.as_deref())
                .map(str::trim)
                .filter(|client_id| !client_id.is_empty())
                .map(ToString::to_string),
        }
    }
}
