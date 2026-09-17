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
}
