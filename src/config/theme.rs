use serde::Deserialize;
use tracing::warn;

/// Theme configuration: pick a built-in or override individual tokens.
///
/// ```toml
/// [theme]
/// name = "tokyo-night"  # built-in: catppuccin, terminal, dracula, nord, etc.
///
/// [theme.custom]        # override individual tokens on top of the base
/// accent = "#f5c2e7"
/// red = "#ff6188"
/// ```
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct ThemeConfig {
    /// Built-in theme name. Default: "catppuccin".
    pub name: Option<String>,
    /// Follow host terminal light/dark appearance and switch between theme names.
    pub auto_switch: bool,
    /// Theme name used when `auto_switch` selects a dark appearance.
    pub dark_name: Option<String>,
    /// Theme name used when `auto_switch` selects a light appearance.
    pub light_name: Option<String>,
    /// Custom overrides — applied on top of the selected base theme.
    pub custom: Option<CustomThemeColors>,
}

/// Per-token color overrides. All fields optional — only set what you want to change.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct CustomThemeColors {
    pub accent: Option<String>,
    pub panel_bg: Option<String>,
    pub surface0: Option<String>,
    pub surface1: Option<String>,
    pub surface_dim: Option<String>,
    pub overlay0: Option<String>,
    pub overlay1: Option<String>,
    pub text: Option<String>,
    pub subtext0: Option<String>,
    pub mauve: Option<String>,
    pub green: Option<String>,
    pub yellow: Option<String>,
    pub red: Option<String>,
    pub blue: Option<String>,
    pub teal: Option<String>,
    pub peach: Option<String>,
}

/// Default colour for each sidebar mark level, indexed by level (0 = parked).
///
/// Level 0 is deliberately left to the theme: a fixed grey would be wrong on
/// the light palettes, and the point of that level is to read at the same
/// weight as the agent and status text. Levels 1-3 stay fixed, as the single
/// mark colour did before levels existed.
fn default_mark_colors() -> [Option<ratatui::style::Color>; 4] {
    use ratatui::style::Color;
    [
        None,
        Some(Color::Rgb(0x89, 0xb4, 0xfa)), // blue
        Some(Color::Rgb(0xcb, 0xa6, 0xf7)), // purple
        Some(Color::Rgb(0xf5, 0xc2, 0xe7)), // pink
    ]
}

/// Resolve `ui.sidebar_highlight_colors` over the defaults. A missing or empty
/// entry keeps the default for that level, so a config can set just one.
///
/// `legacy_high` is the older scalar `ui.sidebar_highlight_color`, which named
/// the only colour there was; it now sets the top level and the array wins
/// over it.
pub fn resolve_mark_colors(
    configured: &[String],
    legacy_high: &str,
) -> [Option<ratatui::style::Color>; 4] {
    let mut colors = default_mark_colors();
    if !legacy_high.trim().is_empty() {
        colors[3] = Some(parse_color(legacy_high));
    }
    for (slot, value) in configured.iter().enumerate().take(colors.len()) {
        if value.trim().is_empty() {
            continue;
        }
        colors[slot] = Some(parse_color(value));
    }
    if configured.len() > colors.len() {
        warn!(
            "ui.sidebar_highlight_colors has {} entries; only the first {} are used",
            configured.len(),
            colors.len()
        );
    }
    colors
}

/// Parse a color string into a ratatui Color.
/// Supports: hex (#rrggbb, #rgb), named colors, rgb(r,g,b), and reset aliases.
pub fn parse_color(s: &str) -> ratatui::style::Color {
    use ratatui::style::Color;
    let s = s.trim().to_lowercase();

    match s.as_str() {
        "reset" | "default" | "none" | "transparent" => return Color::Reset,
        _ => {}
    }

    if let Some(hex) = s.strip_prefix('#') {
        if hex.len() == 6 {
            if let (Ok(r), Ok(g), Ok(b)) = (
                u8::from_str_radix(&hex[0..2], 16),
                u8::from_str_radix(&hex[2..4], 16),
                u8::from_str_radix(&hex[4..6], 16),
            ) {
                return Color::Rgb(r, g, b);
            }
        } else if hex.len() == 3 {
            let chars: Vec<u8> = hex
                .chars()
                .filter_map(|c| u8::from_str_radix(&c.to_string(), 16).ok())
                .collect();
            if chars.len() == 3 {
                return Color::Rgb(chars[0] * 17, chars[1] * 17, chars[2] * 17);
            }
        }
    }

    if let Some(inner) = s.strip_prefix("rgb(").and_then(|s| s.strip_suffix(')')) {
        let parts: Vec<&str> = inner.split(',').collect();
        if parts.len() == 3 {
            if let (Ok(r), Ok(g), Ok(b)) = (
                parts[0].trim().parse::<u8>(),
                parts[1].trim().parse::<u8>(),
                parts[2].trim().parse::<u8>(),
            ) {
                return Color::Rgb(r, g, b);
            }
        }
    }

    match s.as_str() {
        "black" => Color::Black,
        "red" => Color::Red,
        "green" => Color::Green,
        "yellow" => Color::Yellow,
        "blue" => Color::Blue,
        "magenta" | "purple" => Color::Magenta,
        "cyan" => Color::Cyan,
        "white" => Color::White,
        "gray" | "grey" => Color::Gray,
        "darkgray" | "darkgrey" => Color::DarkGray,
        "lightred" => Color::LightRed,
        "lightgreen" => Color::LightGreen,
        "lightyellow" => Color::LightYellow,
        "lightblue" => Color::LightBlue,
        "lightmagenta" => Color::LightMagenta,
        "lightcyan" => Color::LightCyan,
        _ => {
            warn!(color = s, "unknown color, defaulting to cyan");
            Color::Cyan
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    #[test]
    fn theme_name_parses() {
        let toml = r#"
[theme]
name = "dracula"
"#;
        let config: Config = toml::from_str(toml).unwrap();
        assert_eq!(config.theme.name.as_deref(), Some("dracula"));
    }

    /// The parked level stays unset so the palette supplies it, and a config
    /// that names only one level keeps the defaults for the others.
    #[test]
    fn mark_colours_fill_in_around_what_the_config_sets() {
        use ratatui::style::Color;

        let defaults = resolve_mark_colors(&[], "");
        assert_eq!(defaults[0], None);
        assert_eq!(defaults[3], Some(Color::Rgb(0xf5, 0xc2, 0xe7)));

        let partial =
            resolve_mark_colors(&[String::new(), String::new(), "#ff0000".to_string()], "");
        assert_eq!(partial[0], None);
        assert_eq!(partial[1], defaults[1]);
        assert_eq!(partial[2], Some(Color::Rgb(0xff, 0, 0)));
        assert_eq!(partial[3], defaults[3]);
    }

    /// The older scalar named the only mark colour there was, so it keeps
    /// working as the top level — and the array wins where both are set.
    #[test]
    fn the_legacy_scalar_sets_the_top_level_and_yields_to_the_array() {
        use ratatui::style::Color;

        let legacy_only = resolve_mark_colors(&[], "#00ff00");
        assert_eq!(legacy_only[3], Some(Color::Rgb(0, 0xff, 0)));

        let both = resolve_mark_colors(
            &[
                String::new(),
                String::new(),
                String::new(),
                "#0000ff".to_string(),
            ],
            "#00ff00",
        );
        assert_eq!(both[3], Some(Color::Rgb(0, 0, 0xff)));
    }

    #[test]
    fn parse_color_accepts_reset_aliases() {
        use ratatui::style::Color;

        for value in ["reset", "default", "none", "transparent"] {
            assert_eq!(parse_color(value), Color::Reset, "value: {value}");
        }
    }

    #[test]
    fn theme_auto_switch_fields_parse() {
        let toml = r#"
[theme]
name = "catppuccin"
auto_switch = true
dark_name = "tokyo-night"
light_name = "catppuccin-latte"
"#;
        let config: Config = toml::from_str(toml).unwrap();
        assert_eq!(config.theme.name.as_deref(), Some("catppuccin"));
        assert!(config.theme.auto_switch);
        assert_eq!(config.theme.dark_name.as_deref(), Some("tokyo-night"));
        assert_eq!(config.theme.light_name.as_deref(), Some("catppuccin-latte"));
    }

    #[test]
    fn theme_custom_overrides_parse() {
        let toml = r##"
[theme]
name = "nord"

[theme.custom]
panel_bg = "#1e1e2e"
accent = "#ff79c6"
red = "rgb(255, 85, 85)"
"##;
        let config: Config = toml::from_str(toml).unwrap();
        assert_eq!(config.theme.name.as_deref(), Some("nord"));
        let custom = config.theme.custom.as_ref().unwrap();
        assert_eq!(custom.panel_bg.as_deref(), Some("#1e1e2e"));
        assert_eq!(custom.accent.as_deref(), Some("#ff79c6"));
        assert_eq!(custom.red.as_deref(), Some("rgb(255, 85, 85)"));
        assert!(custom.green.is_none());
    }

    #[test]
    fn theme_defaults_when_missing() {
        let config: Config = toml::from_str("").unwrap();
        assert!(config.theme.name.is_none());
        assert!(!config.theme.auto_switch);
        assert!(config.theme.dark_name.is_none());
        assert!(config.theme.light_name.is_none());
        assert!(config.theme.custom.is_none());
    }
}
