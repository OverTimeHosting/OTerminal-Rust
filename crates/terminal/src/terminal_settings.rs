use collections::{HashMap, IndexMap};
use gpui::{FontFallbacks, FontFeatures, FontWeight, Pixels};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

pub use settings::AlternateScroll;

use settings::{
    IntoGpui, PathHyperlinkRegex, RegisterSetting, ShowScrollbar, TerminalBell, TerminalBlink,
    TerminalDockPosition, TerminalLineHeight, VenvSettings, WorkingDirectory,
    merge_from::MergeFrom,
};
use task::Shell;
use theme_settings::FontFamilyName;

#[derive(Copy, Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct Toolbar {
    pub breadcrumbs: bool,
}

#[derive(Clone, Debug, Deserialize, RegisterSetting)]
pub struct TerminalSettings {
    pub shell: Shell,
    pub working_directory: WorkingDirectory,
    pub font_size: Option<Pixels>, // todo(settings_refactor) can be non-optional...
    pub font_family: Option<FontFamilyName>,
    pub font_fallbacks: Option<FontFallbacks>,
    pub font_features: Option<FontFeatures>,
    pub font_weight: Option<FontWeight>,
    pub line_height: TerminalLineHeight,
    pub env: HashMap<String, String>,
    pub cursor_shape: CursorShape,
    pub blinking: TerminalBlink,
    pub alternate_scroll: AlternateScroll,
    pub option_as_meta: bool,
    pub copy_on_select: bool,
    pub keep_selection_on_copy: bool,
    pub open_links_in_mouse_mode: bool,
    pub button: bool,
    pub dock: TerminalDockPosition,
    pub starts_open: bool,
    pub flexible: bool,
    pub default_width: Pixels,
    pub default_height: Pixels,
    pub detect_venv: VenvSettings,
    pub max_scroll_history_lines: Option<usize>,
    pub scroll_multiplier: f32,
    pub toolbar: Toolbar,
    pub scrollbar: ScrollbarSettings,
    pub minimum_contrast: f32,
    pub path_hyperlink_regexes: Vec<String>,
    pub path_hyperlink_timeout_ms: u64,
    pub show_count_badge: bool,
    pub bell: TerminalBell,
    /// Named terminal profiles from settings, filtered to the current platform.
    pub profiles: IndexMap<String, TerminalProfile>,
    /// The profile used for new terminals, if any.
    pub default_profile: Option<String>,
}

#[derive(Copy, Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct ScrollbarSettings {
    /// When to show the scrollbar in the terminal.
    ///
    /// Default: inherits editor scrollbar settings
    pub show: Option<ShowScrollbar>,
}

/// A resolved terminal profile: a named program launch configuration.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct TerminalProfile {
    /// The program to run. `None` means the configured shell.
    pub program: Option<String>,
    pub args: Vec<String>,
    pub env: HashMap<String, String>,
    pub working_directory: Option<String>,
    pub icon: Option<String>,
}

impl From<settings::TerminalProfileContent> for TerminalProfile {
    fn from(content: settings::TerminalProfileContent) -> Self {
        Self {
            program: content.program.filter(|program| !program.trim().is_empty()),
            args: content.args.unwrap_or_default(),
            env: content.env.unwrap_or_default(),
            working_directory: content
                .working_directory
                .filter(|directory| !directory.trim().is_empty()),
            icon: content.icon,
        }
    }
}

/// The platform name used by terminal profiles: "windows", "macos" or "linux".
pub fn current_profile_platform() -> &'static str {
    if cfg!(target_os = "windows") {
        "windows"
    } else if cfg!(target_os = "macos") {
        "macos"
    } else {
        "linux"
    }
}

/// Whether a profile restricted to `platform` should be offered on this OS.
/// `None`, `""` and `"all"` match every platform; "darwin"/"mac"/"osx" are
/// accepted as aliases for macOS and "win32"/"win" for Windows.
pub fn profile_applies_to_current_platform(platform: Option<&str>) -> bool {
    let Some(platform) = platform.map(|p| p.trim().to_ascii_lowercase()) else {
        return true;
    };
    let normalized = match platform.as_str() {
        "" | "all" | "any" | "*" => return true,
        "win" | "win32" | "win64" | "windows" => "windows",
        "mac" | "osx" | "darwin" | "macos" => "macos",
        "unix" | "posix" => return !cfg!(target_os = "windows"),
        "linux" | "freebsd" => "linux",
        other => other,
    };
    normalized == current_profile_platform()
}

fn settings_shell_to_task_shell(shell: settings::Shell) -> Shell {
    match shell {
        settings::Shell::System => Shell::System,
        settings::Shell::Program(program) => Shell::Program(program),
        settings::Shell::WithArguments {
            program,
            args,
            title_override,
        } => Shell::WithArguments {
            program,
            args,
            title_override,
        },
    }
}

impl settings::Settings for TerminalSettings {
    fn from_settings(content: &settings::SettingsContent) -> Self {
        let user_content = content.terminal.clone().unwrap();
        // Note: we allow a subset of "terminal" settings in the project files.
        let mut project_content = user_content.project.clone();
        project_content.merge_from_option(content.project.terminal.as_ref());
        TerminalSettings {
            shell: settings_shell_to_task_shell(project_content.shell.unwrap()),
            working_directory: project_content.working_directory.unwrap(),
            font_size: user_content.font_size.map(|s| s.into_gpui()),
            font_family: user_content.font_family,
            font_fallbacks: user_content.font_fallbacks.map(|fallbacks| {
                FontFallbacks::from_fonts(
                    fallbacks
                        .into_iter()
                        .map(|family| family.0.to_string())
                        .collect(),
                )
            }),
            font_features: user_content.font_features.map(|f| f.into_gpui()),
            font_weight: user_content.font_weight.map(|w| w.into_gpui()),
            line_height: user_content.line_height.unwrap(),
            env: project_content.env.unwrap(),
            cursor_shape: user_content.cursor_shape.unwrap().into(),
            blinking: user_content.blinking.unwrap(),
            alternate_scroll: user_content.alternate_scroll.unwrap(),
            option_as_meta: user_content.option_as_meta.unwrap(),
            copy_on_select: user_content.copy_on_select.unwrap(),
            keep_selection_on_copy: user_content.keep_selection_on_copy.unwrap(),
            open_links_in_mouse_mode: user_content.open_links_in_mouse_mode.unwrap(),
            button: user_content.button.unwrap(),
            dock: user_content.dock.unwrap(),
            starts_open: user_content.starts_open.unwrap(),
            default_width: user_content.default_width.unwrap().into_gpui(),
            default_height: user_content.default_height.unwrap().into_gpui(),
            flexible: user_content.flexible.unwrap(),
            detect_venv: project_content.detect_venv.unwrap(),
            scroll_multiplier: user_content.scroll_multiplier.unwrap(),
            max_scroll_history_lines: user_content.max_scroll_history_lines,
            toolbar: Toolbar {
                breadcrumbs: user_content.toolbar.unwrap().breadcrumbs.unwrap(),
            },
            scrollbar: ScrollbarSettings {
                show: user_content.scrollbar.unwrap().show,
            },
            minimum_contrast: user_content.minimum_contrast.unwrap(),
            path_hyperlink_regexes: project_content
                .path_hyperlink_regexes
                .unwrap()
                .into_iter()
                .map(|regex| match regex {
                    PathHyperlinkRegex::SingleLine(regex) => regex,
                    PathHyperlinkRegex::MultiLine(regex) => regex.join("\n"),
                })
                .collect(),
            path_hyperlink_timeout_ms: project_content.path_hyperlink_timeout_ms.unwrap(),
            show_count_badge: user_content.show_count_badge.unwrap(),
            bell: user_content.bell.unwrap(),
            profiles: user_content
                .profiles
                .unwrap_or_default()
                .into_iter()
                .filter(|(_, profile)| {
                    profile_applies_to_current_platform(profile.platform.as_deref())
                })
                .map(|(name, profile)| (name, TerminalProfile::from(profile)))
                .collect(),
            default_profile: user_content.default_profile,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CursorShape {
    /// Cursor is a block like `█`.
    #[default]
    Block,
    /// Cursor is an underscore like `_`.
    Underline,
    /// Cursor is a vertical bar like `⎸`.
    Bar,
    /// Cursor is a hollow box like `▯`.
    Hollow,
}

impl From<settings::CursorShapeContent> for CursorShape {
    fn from(value: settings::CursorShapeContent) -> Self {
        match value {
            settings::CursorShapeContent::Block => CursorShape::Block,
            settings::CursorShapeContent::Underline => CursorShape::Underline,
            settings::CursorShapeContent::Bar => CursorShape::Bar,
            settings::CursorShapeContent::Hollow => CursorShape::Hollow,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_platform_filter() {
        assert!(profile_applies_to_current_platform(None));
        assert!(profile_applies_to_current_platform(Some("all")));
        assert!(profile_applies_to_current_platform(Some("")));
        assert!(profile_applies_to_current_platform(Some(
            current_profile_platform()
        )));
        let (other, alias) = if cfg!(target_os = "windows") {
            ("linux", "win32")
        } else if cfg!(target_os = "macos") {
            ("windows", "osx")
        } else {
            ("osx", "linux")
        };
        assert!(!profile_applies_to_current_platform(Some(other)));
        assert!(profile_applies_to_current_platform(Some(alias)));
    }
}
