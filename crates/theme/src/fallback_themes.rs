use std::sync::Arc;

use gpui::{FontStyle, FontWeight, HighlightStyle, Hsla, WindowBackgroundAppearance};

use crate::{
    AccentColors, Appearance, DEFAULT_DARK_THEME, PlayerColors, StatusColors,
    StatusColorsRefinement, SyntaxTheme, SystemColors, Theme, ThemeColors, ThemeColorsRefinement,
    ThemeFamily, ThemeStyles, default_color_scales,
};

/// The default (fallback) theme family for OTerminal.
///
/// This is used to construct the default theme fallback values, as well as to
/// have a theme available at compile time for tests.
pub fn zed_default_themes() -> ThemeFamily {
    ThemeFamily {
        id: "zed-default".to_string(),
        name: "Zed Default".into(),
        author: "".into(),
        themes: vec![zed_default_dark()],
        scales: default_color_scales(),
    }
}

// If a theme customizes a foreground version of a status color, but does not
// customize the background color, then use a partly-transparent version of the
// foreground color for the background color.
/// Applies default status color backgrounds from their foreground counterparts.
pub fn apply_status_color_defaults(status: &mut StatusColorsRefinement) {
    for (fg_color, bg_color) in [
        (&status.deleted, &mut status.deleted_background),
        (&status.created, &mut status.created_background),
        (&status.modified, &mut status.modified_background),
        (&status.conflict, &mut status.conflict_background),
        (&status.error, &mut status.error_background),
        (&status.hidden, &mut status.hidden_background),
    ] {
        if bg_color.is_none()
            && let Some(fg_color) = fg_color
        {
            *bg_color = Some(fg_color.opacity(0.25));
        }
    }
}

/// Applies default theme color values derived from player colors.
pub fn apply_theme_color_defaults(
    theme_colors: &mut ThemeColorsRefinement,
    player_colors: &PlayerColors,
) {
    if theme_colors.element_selection_background.is_none() {
        let mut selection = player_colors.local().selection;
        if selection.a == 1.0 {
            selection.a = 0.25;
        }
        theme_colors.element_selection_background = Some(selection);
    }
}

/// Converts a `0xRRGGBBAA` literal into an [`Hsla`].
fn hex(rgba: u32) -> Hsla {
    gpui::rgba(rgba).into()
}

/// The in-code fallback theme. It mirrors the "OTHCloud Dark" palette from
/// `assets/themes/othcloud/othcloud.json` (neutral shadcn surfaces with a single
/// blue accent) so that even a failed asset load still looks like OTHCloud.
pub(crate) fn zed_default_dark() -> Theme {
    let bg = hex(0x0a0a0aff);
    let editor = bg;
    let elevated_surface = hex(0x171717ff);
    let hover = hex(0x262626ff);
    let border = hex(0xffffff1a);
    let border_variant = hex(0xffffff0f);
    let foreground = hex(0xfafafaff);
    let muted = hex(0xa1a1a1ff);
    let placeholder = hex(0x737373ff);
    let disabled = hex(0x525252ff);
    let transparent = SystemColors::default().transparent;

    let accent = hex(0x1447e6ff);
    let blue = hex(0x2b7fffff);
    let gray = hex(0x737373ff);
    let green = hex(0x00bc7dff);
    let orange = hex(0xfe9a00ff);
    let purple = hex(0xc27affff);
    let red = hex(0xff6467ff);
    let teal = hex(0x00d3f2ff);
    let yellow = hex(0xffb900ff);
    let light_blue = hex(0x51a2ffff);
    let light_green = hex(0x05df72ff);

    const ADDED_COLOR: Hsla = Hsla {
        h: 160. / 360.,
        s: 1.0,
        l: 0.37,
        a: 1.0,
    };
    const WORD_ADDED_COLOR: Hsla = Hsla {
        h: 160. / 360.,
        s: 1.0,
        l: 0.37,
        a: 0.35,
    };
    const MODIFIED_COLOR: Hsla = Hsla {
        h: 36. / 360.,
        s: 1.0,
        l: 0.50,
        a: 1.0,
    };
    const REMOVED_COLOR: Hsla = Hsla {
        h: 357. / 360.,
        s: 0.96,
        l: 0.58,
        a: 1.0,
    };
    const WORD_DELETED_COLOR: Hsla = Hsla {
        h: 357. / 360.,
        s: 0.96,
        l: 0.58,
        a: 0.40,
    };

    let mut player = PlayerColors::dark();
    if let Some(local) = player.0.first_mut() {
        local.cursor = foreground;
        local.background = foreground;
        local.selection = hex(0xffffff26);
    }

    Theme {
        id: "othcloud_dark".to_string(),
        name: DEFAULT_DARK_THEME.into(),
        appearance: Appearance::Dark,
        styles: ThemeStyles {
            window_background_appearance: WindowBackgroundAppearance::Opaque,
            system: SystemColors::default(),
            accents: AccentColors(Arc::from(vec![accent])),
            colors: ThemeColors {
                border,
                border_variant,
                border_focused: placeholder,
                border_selected: muted,
                border_transparent: transparent,
                border_disabled: border_variant,
                elevated_surface_background: elevated_surface,
                surface_background: bg,
                background: bg,
                element_background: elevated_surface,
                element_hover: hover,
                element_active: hover,
                element_selected: hover,
                element_disabled: elevated_surface,
                element_selection_background: accent.opacity(0.3),
                drop_target_background: hex(0xffffff14),
                drop_target_border: hex(0xffffff26),
                ghost_element_background: transparent,
                ghost_element_hover: hover,
                ghost_element_active: hover,
                ghost_element_selected: hover,
                ghost_element_disabled: transparent,
                text: foreground,
                text_muted: muted,
                text_placeholder: placeholder,
                text_disabled: disabled,
                text_accent: foreground,
                icon: foreground,
                icon_muted: muted,
                icon_disabled: disabled,
                icon_placeholder: placeholder,
                icon_accent: foreground,
                debugger_accent: red,
                status_bar_background: bg,
                title_bar_background: bg,
                title_bar_inactive_background: bg,
                toolbar_background: bg,
                tab_bar_background: bg,
                tab_inactive_background: bg,
                tab_active_background: elevated_surface,
                search_match_background: accent.opacity(0.3),
                search_active_match_background: accent.opacity(0.6),

                editor_background: editor,
                editor_gutter_background: editor,
                editor_subheader_background: elevated_surface,
                editor_active_line_background: hex(0xffffff0a),
                editor_highlighted_line_background: hex(0xffffff0f),
                editor_debugger_active_line_background: orange.opacity(0.12),
                editor_line_number: disabled,
                editor_active_line_number: foreground,
                editor_hover_line_number: muted,
                editor_invisible: hex(0x404040ff),
                editor_wrap_guide: border_variant,
                editor_active_wrap_guide: border,
                editor_indent_guide: border_variant,
                editor_indent_guide_active: hex(0xffffff26),
                editor_document_highlight_read_background: hex(0xffffff14),
                editor_document_highlight_write_background: hex(0xffffff1f),
                editor_document_highlight_bracket_background: hex(0xffffff1f),
                editor_diff_hunk_added_background: ADDED_COLOR.opacity(0.12),
                editor_diff_hunk_added_hollow_background: ADDED_COLOR.opacity(0.06),
                editor_diff_hunk_added_hollow_border: ADDED_COLOR.opacity(0.36),
                editor_diff_hunk_deleted_background: REMOVED_COLOR.opacity(0.12),
                editor_diff_hunk_deleted_hollow_background: REMOVED_COLOR.opacity(0.06),
                editor_diff_hunk_deleted_hollow_border: REMOVED_COLOR.opacity(0.36),

                terminal_background: bg,
                terminal_ansi_background: bg,
                terminal_foreground: foreground,
                terminal_bright_foreground: hex(0xffffffff),
                terminal_dim_foreground: muted,
                terminal_ansi_black: hex(0x171717ff),
                terminal_ansi_red: hex(0xfb2c36ff),
                terminal_ansi_green: hex(0x00c950ff),
                terminal_ansi_yellow: hex(0xfe9a00ff),
                terminal_ansi_blue: blue,
                terminal_ansi_magenta: hex(0xad46ffff),
                terminal_ansi_cyan: hex(0x00b8dbff),
                terminal_ansi_white: hex(0xe5e5e5ff),
                terminal_ansi_bright_black: disabled,
                terminal_ansi_bright_red: red,
                terminal_ansi_bright_green: light_green,
                terminal_ansi_bright_yellow: yellow,
                terminal_ansi_bright_blue: light_blue,
                terminal_ansi_bright_magenta: purple,
                terminal_ansi_bright_cyan: teal,
                terminal_ansi_bright_white: foreground,
                terminal_ansi_dim_black: hex(0x0f0f0fff),
                terminal_ansi_dim_red: hex(0x6a181cff),
                terminal_ansi_dim_green: hex(0x065626ff),
                terminal_ansi_dim_yellow: hex(0x6c4406ff),
                terminal_ansi_dim_blue: hex(0x17396cff),
                terminal_ansi_dim_magenta: hex(0x4b226cff),
                terminal_ansi_dim_cyan: hex(0x06505eff),
                terminal_ansi_dim_white: hex(0x626262ff),
                panel_background: bg,
                panel_focused_border: hex(0xffffff26),
                panel_indent_guide: border_variant,
                panel_indent_guide_hover: hex(0xffffff26),
                panel_indent_guide_active: hex(0xffffff26),
                panel_overlay_background: elevated_surface,
                panel_overlay_hover: hover,
                pane_focused_border: border,
                pane_group_border: border,
                scrollbar_thumb_background: border,
                scrollbar_thumb_hover_background: muted.opacity(0.3),
                scrollbar_thumb_active_background: muted.opacity(0.4),
                scrollbar_thumb_border: transparent,
                scrollbar_track_background: transparent,
                scrollbar_track_border: transparent,
                minimap_thumb_background: border,
                minimap_thumb_hover_background: muted.opacity(0.3),
                minimap_thumb_active_background: muted.opacity(0.4),
                minimap_thumb_border: transparent,
                editor_foreground: foreground,
                editor_code_lens_foreground: None,
                link_text_hover: light_blue,
                version_control_added: ADDED_COLOR,
                version_control_deleted: REMOVED_COLOR,
                version_control_modified: MODIFIED_COLOR,
                version_control_renamed: blue,
                version_control_conflict: orange,
                version_control_ignored: gray,
                version_control_word_added: WORD_ADDED_COLOR,
                version_control_word_deleted: WORD_DELETED_COLOR,
                version_control_conflict_marker_ours: green.alpha(0.1),
                version_control_conflict_marker_theirs: blue.alpha(0.1),

                vim_normal_background: transparent,
                vim_insert_background: transparent,
                vim_replace_background: transparent,
                vim_visual_background: transparent,
                vim_visual_line_background: transparent,
                vim_visual_block_background: transparent,
                vim_yank_background: accent.opacity(0.3),
                vim_helix_jump_label_foreground: red,
                vim_helix_normal_background: transparent,
                vim_helix_select_background: transparent,
                vim_normal_foreground: transparent,
                vim_insert_foreground: transparent,
                vim_replace_foreground: transparent,
                vim_visual_foreground: transparent,
                vim_visual_line_foreground: transparent,
                vim_visual_block_foreground: transparent,
                vim_helix_normal_foreground: transparent,
                vim_helix_select_foreground: transparent,
            },
            status: StatusColors {
                conflict: orange,
                conflict_background: orange.opacity(0.15),
                conflict_border: orange.opacity(0.4),
                created: green,
                created_background: green.opacity(0.15),
                created_border: green.opacity(0.4),
                deleted: hex(0xfb2c36ff),
                deleted_background: hex(0xfb2c3626),
                deleted_border: hex(0xfb2c3666),
                error: red,
                error_background: red.opacity(0.15),
                error_border: red.opacity(0.4),
                hidden: gray,
                hidden_background: gray.opacity(0.15),
                hidden_border: gray.opacity(0.4),
                hint: blue,
                hint_background: blue.opacity(0.15),
                hint_border: blue.opacity(0.4),
                ignored: gray,
                ignored_background: gray.opacity(0.15),
                ignored_border: gray.opacity(0.4),
                info: blue,
                info_background: blue.opacity(0.15),
                info_border: blue.opacity(0.4),
                modified: orange,
                modified_background: orange.opacity(0.15),
                modified_border: orange.opacity(0.4),
                predictive: gray,
                predictive_background: gray.opacity(0.15),
                predictive_border: gray.opacity(0.4),
                renamed: blue,
                renamed_background: blue.opacity(0.15),
                renamed_border: blue.opacity(0.4),
                success: green,
                success_background: green.opacity(0.15),
                success_border: green.opacity(0.4),
                unreachable: muted,
                unreachable_background: muted.opacity(0.15),
                unreachable_border: muted.opacity(0.4),
                warning: orange,
                warning_background: orange.opacity(0.15),
                warning_border: orange.opacity(0.4),
            },
            player,
            syntax: Arc::new(SyntaxTheme::new(vec![
                ("attribute".into(), yellow.into()),
                ("boolean".into(), yellow.into()),
                (
                    "comment".into(),
                    HighlightStyle {
                        color: Some(gray),
                        font_style: Some(FontStyle::Italic),
                        ..HighlightStyle::default()
                    },
                ),
                (
                    "comment.doc".into(),
                    HighlightStyle {
                        color: Some(gray),
                        font_style: Some(FontStyle::Italic),
                        ..HighlightStyle::default()
                    },
                ),
                ("constant".into(), yellow.into()),
                ("constructor".into(), light_blue.into()),
                ("embedded".into(), foreground.into()),
                (
                    "emphasis".into(),
                    HighlightStyle {
                        font_style: Some(FontStyle::Italic),
                        ..HighlightStyle::default()
                    },
                ),
                (
                    "emphasis.strong".into(),
                    HighlightStyle {
                        font_weight: Some(FontWeight::BOLD),
                        ..HighlightStyle::default()
                    },
                ),
                ("enum".into(), teal.into()),
                ("function".into(), light_blue.into()),
                ("function.method".into(), light_blue.into()),
                ("function.definition".into(), light_blue.into()),
                ("hint".into(), gray.into()),
                ("keyword".into(), purple.into()),
                ("label".into(), light_blue.into()),
                ("link_text".into(), light_blue.into()),
                (
                    "link_uri".into(),
                    HighlightStyle {
                        color: Some(teal),
                        font_style: Some(FontStyle::Italic),
                        ..HighlightStyle::default()
                    },
                ),
                ("number".into(), yellow.into()),
                ("operator".into(), muted.into()),
                ("predictive".into(), gray.into()),
                ("preproc".into(), purple.into()),
                ("primary".into(), foreground.into()),
                ("property".into(), hex(0xe5e5e5ff).into()),
                ("punctuation".into(), muted.into()),
                ("punctuation.bracket".into(), muted.into()),
                ("punctuation.delimiter".into(), muted.into()),
                ("punctuation.list_marker".into(), red.into()),
                ("punctuation.special".into(), purple.into()),
                ("string".into(), light_green.into()),
                ("string.escape".into(), muted.into()),
                ("string.regex".into(), yellow.into()),
                ("string.special".into(), yellow.into()),
                ("string.special.symbol".into(), yellow.into()),
                ("tag".into(), light_blue.into()),
                ("text.literal".into(), light_green.into()),
                ("title".into(), foreground.into()),
                ("type".into(), teal.into()),
                ("variable".into(), foreground.into()),
                ("variable.special".into(), purple.into()),
                ("variant".into(), light_blue.into()),
                ("diff.plus".into(), light_green.into()),
                ("diff.minus".into(), red.into()),
            ])),
        },
    }
}
