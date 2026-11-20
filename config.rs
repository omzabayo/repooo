use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use uuid::Uuid;

pub type TabId = Uuid;
pub type ProfileId = Uuid;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Theme {
    pub font_family: String,
    pub font_size: u32,
    pub font_weight: u32,
}
impl Default for Theme {
    fn default() -> Self {
        Self {
            font_family: String::new(),
            font_size: 14,
            font_weight: 400,
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub theme: Theme,
    pub width: u32,
    pub height: u32,
    pub performance_mode: crate::performance::PerformanceMode,
    pub lazy_restore: bool,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            theme: Theme::default(),
            width: 1280,
            height: 820,
            performance_mode: Default::default(),
            lazy_restore: true,
        }
    }
}
impl Config {
    pub fn load(root: &Path) -> anyhow::Result<Self> {
        let path = root.join("config.toml");
        if !path.exists() {
            let c = Self::default();
            c.save(root)?;
            return Ok(c);
        }
        let mut c: Self = toml::from_str(&std::fs::read_to_string(path)?)?;
        c.width = c.width.clamp(900, 7680);
        c.height = c.height.clamp(600, 4320);
        c.theme.font_size = c.theme.font_size.clamp(10, 24);
        c.theme.font_weight = c.theme.font_weight.clamp(100, 900);
        Ok(c)
    }
    pub fn save(&self, root: &Path) -> anyhow::Result<()> {
        std::fs::write(root.join("config.toml"), toml::to_string_pretty(self)?)?;
        Ok(())
    }
}
pub fn data_root() -> anyhow::Result<PathBuf> {
    let executable = std::env::current_exe()?;
    let directory = executable
        .parent()
        .ok_or_else(|| anyhow::anyhow!("Executable directory is unavailable"))?;
    let portable = directory.join("portable-data.txt");
    let p = if let Some(path) = std::env::var_os("TABPILOT_DATA_DIR") {
        PathBuf::from(path)
    } else if portable.exists() {
        let text = std::fs::read_to_string(portable)?;
        let relative = PathBuf::from(text.trim());
        anyhow::ensure!(
            !relative.as_os_str().is_empty()
                && relative
                    .components()
                    .all(|c| matches!(c, std::path::Component::Normal(_))),
            "portable-data.txt must contain a relative subfolder path"
        );
        directory.join(relative)
    } else {
        std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("data"))
            .join("TabPilot")
    };
    std::fs::create_dir_all(&p)?;
    Ok(p)
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum MouseButton {
    Left,
    Right,
    Middle,
}
impl MouseButton {
    pub fn cdp(&self) -> &'static str {
        match self {
            Self::Left => "left",
            Self::Right => "right",
            Self::Middle => "middle",
        }
    }
    pub fn mask(&self) -> u8 {
        match self {
            Self::Left => 1,
            Self::Right => 2,
            Self::Middle => 4,
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "lowercase")]
pub enum Target {
    Element {
        selector: String,
    },
    Coordinate {
        x: f64,
        y: f64,
        viewport_width: f64,
        viewport_height: f64,
    },
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Preset {
    pub target: Target,
    pub button: MouseButton,
    pub interval_ms: u64,
    pub delay_ms: u64,
    pub count: Option<u64>,
    pub timeout_ms: u64,
}
impl Default for Preset {
    fn default() -> Self {
        Self {
            target: Target::Element {
                selector: "#counter".into(),
            },
            button: MouseButton::Left,
            interval_ms: 1000,
            delay_ms: 0,
            count: Some(10),
            timeout_ms: 5000,
        }
    }
}
impl Preset {
    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            (50..=86_400_000).contains(&self.interval_ms),
            "Interval must be 50 to 86400000 ms"
        );
        anyhow::ensure!(self.delay_ms <= 86_400_000, "Delay exceeds one day");
        anyhow::ensure!(
            (100..=120_000).contains(&self.timeout_ms),
            "Timeout must be 100 to 120000 ms"
        );
        anyhow::ensure!(
            self.count != Some(0),
            "Count must be positive or blank for continuous"
        );
        match &self.target {
            Target::Element { selector } => anyhow::ensure!(
                !selector.trim().is_empty() && selector.len() <= 4096,
                "Selector is empty or too long"
            ),
            Target::Coordinate {
                x,
                y,
                viewport_width: w,
                viewport_height: h,
            } => anyhow::ensure!(
                x.is_finite()
                    && y.is_finite()
                    && w.is_finite()
                    && h.is_finite()
                    && *w > 0.0
                    && *h > 0.0
                    && *x >= 0.0
                    && *y >= 0.0
                    && x < w
                    && y < h,
                "Coordinates must be inside the recorded viewport"
            ),
        }
        Ok(())
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Profile {
    pub id: ProfileId,
    pub tab_id: TabId,
    pub name: String,
    pub url: String,
    pub preset: Preset,
    pub open: bool,
}
impl Profile {
    pub fn new(index: usize) -> Self {
        Self {
            id: Uuid::new_v4(),
            tab_id: Uuid::new_v4(),
            name: format!("Account {index:02}"),
            url: "about:blank".into(),
            preset: Preset::default(),
            open: true,
        }
    }
}
pub fn validated_url(input: &str) -> anyhow::Result<String> {
    if input == "about:blank" {
        return Ok(input.into());
    }
    let input = input.trim();
    let normalized = if !input.contains(':') {
        format!("https://{input}")
    } else {
        input.to_string()
    };
    let url = url::Url::parse(&normalized)?;
    anyhow::ensure!(
        matches!(url.scheme(), "http" | "https"),
        "Only http and https addresses are allowed"
    );
    anyhow::ensure!(
        url.username().is_empty() && url.password().is_none(),
        "Credentials in addresses are not allowed"
    );
    Ok(url.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reject_invalid_coordinates() {
        let p = Preset {
            target: Target::Coordinate {
                x: f64::NAN,
                y: 0.0,
                viewport_width: 100.0,
                viewport_height: 100.0,
            },
            ..Preset::default()
        };
        assert!(p.validate().is_err());
    }
    #[test]
    fn reject_credentials_and_scripts() {
        assert!(validated_url("https://user:secret@example.com").is_err());
        assert!(validated_url("javascript:alert(1)").is_err());
    }
    #[test]
    fn validate_defaults_and_theme() {
        assert!(Preset::default().validate().is_ok());
        assert!(Theme::default().font_family.is_empty());
    }
}
