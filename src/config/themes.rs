//! Built-in palettes. Named JSON themes and the picker resolve to the same Theme;
//! existing custom theme objects remain editable without a schema migration.
use super::Theme;
use serde::{de::Error, Deserialize, Deserializer};

pub const PRESETS: &[(&str, &str)] = &[
    ("haxx0r", "Black with phosphor greens"),
    ("BnP", "Black with shades of pink"),
    ("solarized", "Solarized dark, warm accents on deep teal"),
    ("mama_j", "White and gray on black"),
    ("diet_soda", "Near-black with green and pink"),
    ("blue", "Midnight blue with ice and cobalt"),
];

pub fn preset(name: &str) -> Option<Theme> {
    // Each row is foreground, accent, user, tool, error, border, muted, success,
    // warning. Keeping semantic roles separate preserves contrast in every view.
    let (background, colors, syntax) = match name.to_ascii_lowercase().as_str() {
        "haxx0r" => (
            "#000000",
            [
                "#a4f5b4", "#39ff78", "#7de89b", "#54c979", "#d8ffe1", "#245c36", "#62a875",
                "#39ff78", "#b6ed86",
            ],
            "haxx0r",
        ),
        "bnp" => (
            "#000000",
            [
                "#f7bddc", "#ff58af", "#ff91c8", "#d981b2", "#ffe0ef", "#64314c", "#ad7292",
                "#ffc1e4", "#f58ac0",
            ],
            "BnP",
        ),
        "solarized" => (
            "#002b36",
            [
                "#93a1a1", "#268bd2", "#859900", "#2aa198", "#dc322f", "#586e75", "#839496",
                "#859900", "#b58900",
            ],
            "Solarized (dark)",
        ),
        "mama_j" => (
            "#000000",
            [
                "#ffffff", "#ffffff", "#dedede", "#bcbcbc", "#ffffff", "#555555", "#999999",
                "#eeeeee", "#cccccc",
            ],
            "mama_j",
        ),
        "diet_soda" => (
            "#101214",
            [
                "#bbefc5", "#ff75ba", "#73e69c", "#e69bc4", "#ff9bcf", "#394c43", "#8cae98",
                "#70eba1", "#ffc2e1",
            ],
            "diet_soda",
        ),
        "blue" => (
            "#081222",
            [
                "#c1d9f5", "#65adff", "#9ad6ff", "#6e9ee8", "#d0dfff", "#29456b", "#7b9cbe",
                "#a2dcff", "#aac2ff",
            ],
            "blue",
        ),
        _ => return None,
    };
    let [foreground, accent, user, tool, error, border, muted, success, warning] = colors;
    Some(Theme {
        background: background.into(),
        foreground: foreground.into(),
        accent: accent.into(),
        user: user.into(),
        assistant: foreground.into(),
        tool: tool.into(),
        error: error.into(),
        border: border.into(),
        muted: muted.into(),
        success: success.into(),
        warning: warning.into(),
        cta_background: accent.into(),
        cta_foreground: background.into(),
        syntax_theme: syntax.into(),
        ..Theme::default()
    })
}

pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Theme, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Setting {
        Name(String),
        Custom(Box<Theme>),
    }
    match Setting::deserialize(deserializer)? {
        Setting::Name(name) => {
            preset(&name).ok_or_else(|| D::Error::custom(format!("Unknown theme: {name}")))
        }
        Setting::Custom(theme) => Ok(*theme),
    }
}

/// Retain bundled syntax scopes and font styles, but remap their colors to the
/// selected palette. This keeps monochrome themes monochrome inside code too.
pub fn syntax_themes() -> syntect::highlighting::ThemeSet {
    use syntect::highlighting::{Color, ThemeSet};
    let mut themes = ThemeSet::load_defaults();
    let base = themes.themes["base16-ocean.dark"].clone();
    let color = |hex: &str| {
        let rgb = u32::from_str_radix(&hex[1..], 16).unwrap();
        Color {
            r: (rgb >> 16) as u8,
            g: (rgb >> 8) as u8,
            b: rgb as u8,
            a: 255,
        }
    };
    for (name, _) in PRESETS.iter().filter(|(name, _)| *name != "solarized") {
        let palette = preset(name).unwrap();
        let mut syntax = base.clone();
        syntax.settings.foreground = Some(color(&palette.foreground));
        syntax.settings.background = Some(color(&palette.background));
        for rule in &mut syntax.scopes {
            if let Some(old) = rule.style.foreground {
                let hex = match (old.r, old.g, old.b) {
                    (0xbf, 0x61, 0x6a) => &palette.error,
                    (0xd0, 0x87, 0x70) | (0xeb, 0xcb, 0x8b) => &palette.warning,
                    (0xa3, 0xbe, 0x8c) => &palette.user,
                    (0x96, 0xb5, 0xb4) | (0x8f, 0xa1, 0xb3) => &palette.tool,
                    (0xb4, 0x8e, 0xad) => &palette.accent,
                    (0x65, 0x73, 0x7e) => &palette.muted,
                    _ => &palette.foreground,
                };
                rule.style.foreground = Some(color(hex));
            }
        }
        themes.themes.insert(name.to_string(), syntax);
    }
    themes
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    #[test]
    fn named_and_custom_themes_validate_and_roundtrip() {
        let syntax = syntax_themes();
        for (name, _) in PRESETS {
            let config: Config =
                serde_json::from_value(serde_json::json!({"theme": name})).unwrap();
            config.validate().unwrap();
            assert!(syntax.themes.contains_key(&config.theme.syntax_theme));
            let reloaded: Config =
                serde_json::from_value(serde_json::to_value(&config).unwrap()).unwrap();
            assert_eq!(reloaded.theme, config.theme);
        }
        let custom: Config = serde_json::from_value(
            serde_json::json!({"theme":{"background":"#123456","ascii":true}}),
        )
        .unwrap();
        assert_eq!(custom.theme.background, "#123456");
        assert!(custom.theme.ascii);
        assert!(serde_json::from_value::<Config>(serde_json::json!({"theme":"missing"})).is_err());
    }
}
