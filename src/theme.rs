use ratatui::style::{Color, Style};
use serde::{Deserialize, Deserializer};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Theme {
    pub foreground: Color,
    pub terminal_foreground: Color,
    pub background: Color,
    pub secondary: Color,
    pub muted: Color,
    pub success: Color,
    pub warning: Color,
    pub error: Color,
    pub info: Color,
    pub accent: Color,
    pub browse_accent: Color,
    pub selection_background: Color,
    pub inverse: Color,
    pub header_background: Color,
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            foreground: Color::White,
            terminal_foreground: Color::Reset,
            background: Color::Reset,
            secondary: Color::Gray,
            muted: Color::DarkGray,
            success: Color::Green,
            warning: Color::Yellow,
            error: Color::Red,
            info: Color::Cyan,
            accent: Color::Magenta,
            browse_accent: Color::Yellow,
            selection_background: Color::Indexed(237),
            inverse: Color::Black,
            header_background: Color::Black,
        }
    }
}

impl Theme {
    pub fn base_style(&self) -> Style {
        Style::default()
            .fg(self.terminal_foreground)
            .bg(self.background)
    }

    /// `(config key, color)` pairs in `[options.theme]` field order. Used to
    /// persist a theme (e.g. an applied preset) back to `config.toml`:
    /// `Color`'s `Display` impl round-trips through this struct's
    /// `Deserialize` (`"#RRGGBB"` / palette index / color name), so writing
    /// `color.to_string()` for every field is always valid regardless of how
    /// that field was originally set.
    pub fn fields(&self) -> [(&'static str, Color); 14] {
        [
            ("foreground", self.foreground),
            ("terminal_foreground", self.terminal_foreground),
            ("background", self.background),
            ("secondary", self.secondary),
            ("muted", self.muted),
            ("success", self.success),
            ("warning", self.warning),
            ("error", self.error),
            ("info", self.info),
            ("accent", self.accent),
            ("browse_accent", self.browse_accent),
            ("selection_background", self.selection_background),
            ("inverse", self.inverse),
            ("header_background", self.header_background),
        ]
    }
}

impl<'de> Deserialize<'de> for Theme {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let values = std::collections::BTreeMap::<String, toml::Value>::deserialize(deserializer)?;
        let mut theme = Self::default();
        for (name, value) in values {
            let field = match name.as_str() {
                "foreground" => &mut theme.foreground,
                "terminal_foreground" => &mut theme.terminal_foreground,
                "background" => &mut theme.background,
                "secondary" => &mut theme.secondary,
                "muted" => &mut theme.muted,
                "success" => &mut theme.success,
                "warning" => &mut theme.warning,
                "error" => &mut theme.error,
                "info" => &mut theme.info,
                "accent" => &mut theme.accent,
                "browse_accent" => &mut theme.browse_accent,
                "selection_background" => &mut theme.selection_background,
                "inverse" => &mut theme.inverse,
                "header_background" => &mut theme.header_background,
                _ => {
                    eprintln!("Warning: unknown options.theme.{name}; ignoring field");
                    continue;
                }
            };
            let color = match value {
                toml::Value::String(ref s) => s.trim().parse::<Color>().ok(),
                toml::Value::Integer(n) => u8::try_from(n).ok().map(Color::Indexed),
                _ => None,
            };
            if let Some(color) = color {
                *field = color;
            } else {
                eprintln!("Warning: invalid color for options.theme.{name}; using default {field}");
            }
        }
        Ok(theme)
    }
}

/// Built-in copy-paste presets (also documented in `docs/cli.md`'s "TUI
/// theme" section). Ported from each colorscheme's published palette; not
/// pixel-perfect matches to any specific Neovim plugin version. Listed here
/// alphabetically; `builtin_presets()` preserves this order.
pub const BUILTIN_PRESET_NAMES: &[&str] = &[
    "catppuccin-mocha",
    "dracula",
    "gruvbox-dark",
    "nord",
    "tokyo-night-storm",
];

/// Resolves a built-in preset by name, or `None` if `name` isn't one of
/// [`BUILTIN_PRESET_NAMES`]. Parses through [`Theme`]'s own `Deserialize` so
/// the presets can never drift from what a user could paste into
/// `[options.theme]` themselves.
pub fn builtin_preset(name: &str) -> Option<Theme> {
    let toml_str = match name {
        "catppuccin-mocha" => {
            r##"
            foreground = "#cdd6f4"
            background = "#1e1e2e"
            terminal_foreground = "#cdd6f4"
            secondary = "#bac2de"
            muted = "#6c7086"
            success = "#a6e3a1"
            warning = "#f9e2af"
            error = "#f38ba8"
            info = "#89dceb"
            accent = "#cba6f7"
            browse_accent = "#f9e2af"
            selection_background = "#313244"
            inverse = "#1e1e2e"
            header_background = "#1e1e2e"
            "##
        }
        "dracula" => {
            r##"
            foreground = "#f8f8f2"
            background = "#282a36"
            terminal_foreground = "#f8f8f2"
            secondary = "#f8f8f2"
            muted = "#6272a4"
            success = "#50fa7b"
            warning = "#f1fa8c"
            error = "#ff5555"
            info = "#8be9fd"
            accent = "#bd93f9"
            browse_accent = "#ffb86c"
            selection_background = "#44475a"
            inverse = "#282a36"
            header_background = "#282a36"
            "##
        }
        "gruvbox-dark" => {
            r##"
            foreground = "#ebdbb2"
            background = "#282828"
            terminal_foreground = "#ebdbb2"
            secondary = "#d5c4a1"
            muted = "#928374"
            success = "#b8bb26"
            warning = "#fabd2f"
            error = "#fb4934"
            info = "#83a598"
            accent = "#d3869b"
            browse_accent = "#fabd2f"
            selection_background = "#3c3836"
            inverse = "#282828"
            header_background = "#282828"
            "##
        }
        "nord" => {
            r##"
            foreground = "#d8dee9"
            background = "#2e3440"
            terminal_foreground = "#d8dee9"
            secondary = "#e5e9f0"
            muted = "#4c566a"
            success = "#a3be8c"
            warning = "#ebcb8b"
            error = "#bf616a"
            info = "#88c0d0"
            accent = "#b48ead"
            browse_accent = "#ebcb8b"
            selection_background = "#434c5e"
            inverse = "#2e3440"
            header_background = "#2e3440"
            "##
        }
        "tokyo-night-storm" => {
            r##"
            foreground = "#c0caf5"
            background = "#1a1b26"
            terminal_foreground = "#c0caf5"
            secondary = "#a9b1d6"
            muted = "#565f89"
            success = "#9ece6a"
            warning = "#e0af68"
            error = "#f7768e"
            info = "#7dcfff"
            accent = "#bb9af7"
            browse_accent = "#e0af68"
            selection_background = "#283457"
            inverse = "#1a1b26"
            header_background = "#1a1b26"
            "##
        }
        _ => return None,
    };
    Some(toml::from_str(toml_str).expect("built-in theme preset must parse"))
}

/// All built-in presets as `(name, theme)` pairs, in [`BUILTIN_PRESET_NAMES`]
/// order.
pub fn builtin_presets() -> Vec<(&'static str, Theme)> {
    BUILTIN_PRESET_NAMES
        .iter()
        .map(|&name| {
            (
                name,
                builtin_preset(name).expect("every listed preset name must resolve"),
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn theme_defaults_preserve_original_colors() {
        let parsed = crate::config::parse_config("[options]")
            .unwrap()
            .options
            .theme;
        assert_eq!(parsed, Theme::default());
        assert_eq!(parsed.foreground, Color::White);
        assert_eq!(parsed.terminal_foreground, Color::Reset);
        assert_eq!(parsed.background, Color::Reset);
        assert_eq!(parsed.secondary, Color::Gray);
        assert_eq!(parsed.muted, Color::DarkGray);
        assert_eq!(parsed.success, Color::Green);
        assert_eq!(parsed.warning, Color::Yellow);
        assert_eq!(parsed.error, Color::Red);
        assert_eq!(parsed.info, Color::Cyan);
        assert_eq!(parsed.accent, Color::Magenta);
        assert_eq!(parsed.browse_accent, Color::Yellow);
        assert_eq!(parsed.selection_background, Color::Indexed(237));
        assert_eq!(parsed.inverse, Color::Black);
        assert_eq!(parsed.header_background, Color::Black);
        assert_eq!(toml::from_str::<Theme>("").unwrap(), parsed);
    }

    #[test]
    fn theme_invalid_values_fall_back_independently() {
        for value in [
            "-1",
            "256",
            "true",
            "1.5",
            "[]",
            "{}",
            "\"#xyzxyz\"",
            "\"\"",
        ] {
            let theme: Theme = toml::from_str(&format!("success = {value}\ninfo = 255")).unwrap();
            assert_eq!(theme.success, Color::Green, "{value}");
            assert_eq!(theme.info, Color::Indexed(255));
        }
        let theme: Theme =
            toml::from_str("foreground = \"reset\"\nmuted = \"0\"\nterminal_foreground = \"blue\"")
                .unwrap();
        assert_eq!(theme.foreground, Color::Reset);
        assert_eq!(theme.muted, Color::Indexed(0));
        assert_eq!(theme.base_style().fg, Some(Color::Blue));
    }

    #[test]
    fn theme_fields_round_trip_through_display_and_parse() {
        // Persistence (list TUI theme picker) writes `color.to_string()` for
        // every field and relies on it re-parsing back to the same `Color`
        // through this struct's `Deserialize`. Cover all three `Color`
        // variants a theme can hold: Rgb (hex-defined preset), Indexed
        // (palette index), and a bare named color.
        let theme: Theme = toml::from_str(
            "foreground = \"#cdd6f4\"\nselection_background = 237\nmuted = \"darkgray\"",
        )
        .unwrap();
        for (name, color) in theme.fields() {
            let reparsed: Color = color.to_string().parse().unwrap_or_else(|_| {
                panic!("field {name} = {color} did not round-trip through Display/FromStr")
            });
            assert_eq!(reparsed, color, "field {name}");
        }
    }

    #[test]
    fn builtin_preset_resolves_every_listed_name() {
        for &name in BUILTIN_PRESET_NAMES {
            let theme = builtin_preset(name).unwrap_or_else(|| panic!("{name} must resolve"));
            // Every documented preset is defined entirely in hex, so every
            // field must come back as `Color::Rgb`, never a silent
            // Theme::default() fallback from a typo'd hex string.
            for (field, color) in theme.fields() {
                assert!(
                    matches!(color, Color::Rgb(..)),
                    "{name}.{field} = {color:?} is not Rgb (typo'd hex?)"
                );
            }
        }
    }

    #[test]
    fn builtin_preset_rejects_unknown_name() {
        assert_eq!(builtin_preset("not-a-real-preset"), None);
    }

    #[test]
    fn builtin_presets_matches_names_in_order() {
        let presets = builtin_presets();
        let names: Vec<&str> = presets.iter().map(|(n, _)| *n).collect();
        assert_eq!(names, BUILTIN_PRESET_NAMES);
    }
}
