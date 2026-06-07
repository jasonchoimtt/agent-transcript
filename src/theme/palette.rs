use std::collections::HashMap;

use ratatui::style::Color;
use serde::{Deserialize, Deserializer};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ColorVar {
    Primary,
    PrimaryLight,
    Accent,
    AccentLight,
    Fg,
    Bg,
    Muted,
    Custom(String),
}

impl<'de> Deserialize<'de> for ColorVar {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        let s = String::deserialize(de)?;
        Ok(match s.as_str() {
            "primary" => ColorVar::Primary,
            "primary_light" => ColorVar::PrimaryLight,
            "accent" => ColorVar::Accent,
            "accent_light" => ColorVar::AccentLight,
            "fg" => ColorVar::Fg,
            "bg" => ColorVar::Bg,
            "muted" => ColorVar::Muted,
            // Anything else is treated as a palette custom-map key (e.g. "error", "json_key").
            _ => ColorVar::Custom(s),
        })
    }
}

#[derive(Clone)]
pub struct Palette {
    pub primary: Color,
    pub primary_light: Color,
    pub accent: Color,
    pub accent_light: Color,
    pub fg: Color,
    pub bg: Color,
    pub muted: Color,
    pub custom: HashMap<String, Color>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum ColorDef {
    Rgb([u8; 3]),
    Named(String),
}

impl TryFrom<ColorDef> for Color {
    type Error = String;
    fn try_from(cd: ColorDef) -> Result<Self, String> {
        match cd {
            ColorDef::Rgb([r, g, b]) => Ok(Color::Rgb(r, g, b)),
            ColorDef::Named(name) => named_color(&name),
        }
    }
}

fn named_color(name: &str) -> Result<Color, String> {
    match name {
        "Reset" => Ok(Color::Reset),
        "Black" => Ok(Color::Black),
        "Red" => Ok(Color::Red),
        "Green" => Ok(Color::Green),
        "Yellow" => Ok(Color::Yellow),
        "Blue" => Ok(Color::Blue),
        "Magenta" => Ok(Color::Magenta),
        "Cyan" => Ok(Color::Cyan),
        "Gray" | "Grey" => Ok(Color::Gray),
        "DarkGray" | "DarkGrey" => Ok(Color::DarkGray),
        "LightRed" => Ok(Color::LightRed),
        "LightGreen" => Ok(Color::LightGreen),
        "LightYellow" => Ok(Color::LightYellow),
        "LightBlue" => Ok(Color::LightBlue),
        "LightMagenta" => Ok(Color::LightMagenta),
        "LightCyan" => Ok(Color::LightCyan),
        "White" => Ok(Color::White),
        _ => Err(format!("unknown color name: {name}")),
    }
}

#[derive(Deserialize)]
struct RawPalette {
    primary: ColorDef,
    primary_light: ColorDef,
    accent: ColorDef,
    accent_light: ColorDef,
    fg: ColorDef,
    bg: ColorDef,
    muted: ColorDef,
    #[serde(default)]
    custom: HashMap<String, ColorDef>,
}

impl<'de> Deserialize<'de> for Palette {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        let raw = RawPalette::deserialize(de)?;
        let custom = raw
            .custom
            .into_iter()
            .map(|(k, v)| Color::try_from(v).map(|c| (k, c)))
            .collect::<Result<HashMap<_, _>, _>>()
            .map_err(serde::de::Error::custom)?;
        Ok(Self {
            primary: raw.primary.try_into().map_err(serde::de::Error::custom)?,
            primary_light: raw
                .primary_light
                .try_into()
                .map_err(serde::de::Error::custom)?,
            accent: raw.accent.try_into().map_err(serde::de::Error::custom)?,
            accent_light: raw
                .accent_light
                .try_into()
                .map_err(serde::de::Error::custom)?,
            fg: raw.fg.try_into().map_err(serde::de::Error::custom)?,
            bg: raw.bg.try_into().map_err(serde::de::Error::custom)?,
            muted: raw.muted.try_into().map_err(serde::de::Error::custom)?,
            custom,
        })
    }
}

impl Palette {
    pub fn resolve(&self, var: &ColorVar) -> Color {
        match var {
            ColorVar::Primary => self.primary,
            ColorVar::PrimaryLight => self.primary_light,
            ColorVar::Accent => self.accent,
            ColorVar::AccentLight => self.accent_light,
            ColorVar::Fg => self.fg,
            ColorVar::Bg => self.bg,
            ColorVar::Muted => self.muted,
            ColorVar::Custom(name) => self.custom.get(name).copied().unwrap_or(self.fg),
        }
    }
}
