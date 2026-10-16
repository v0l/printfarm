use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum Link {
    MarlinUsb { port: String, baud: u32 },
}

impl Link {
    pub fn label(&self) -> &'static str {
        match self {
            Link::MarlinUsb { .. } => "Marlin over USB",
        }
    }

    pub fn summary(&self) -> String {
        match self {
            Link::MarlinUsb { port, baud } => format!("{port} at {baud} baud"),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PrinterConfig {
    pub id: String,
    pub name: String,
    #[serde(flatten)]
    pub link: Link,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub camera: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Config {
    #[serde(default, rename = "printer")]
    pub printers: Vec<PrinterConfig>,
}

fn home() -> Option<PathBuf> {
    std::env::var_os("PRINTFARM_HOME").map(PathBuf::from)
}

pub fn config_dir() -> PathBuf {
    home().unwrap_or_else(|| {
        dirs::config_dir().unwrap_or_else(|| PathBuf::from(".")).join("printfarm")
    })
}

pub fn data_dir() -> PathBuf {
    home()
        .unwrap_or_else(|| dirs::data_dir().unwrap_or_else(|| PathBuf::from(".")).join("printfarm"))
}

pub fn config_path() -> PathBuf {
    config_dir().join("printers.toml")
}

impl Config {
    pub fn load(path: &Path) -> Result<Config, String> {
        match std::fs::read_to_string(path) {
            Ok(text) => toml::from_str(&text).map_err(|e| format!("{}: {e}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
            Err(e) => Err(format!("{}: {e}", path.display())),
        }
    }

    pub fn save(&self, path: &Path) -> Result<(), String> {
        let text = toml::to_string_pretty(self).map_err(|e| e.to_string())?;
        write_atomic(path, text.as_bytes())
    }

    pub fn fresh_id(&self, name: &str) -> String {
        let base: String = name
            .to_ascii_lowercase()
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .collect::<String>()
            .split('-')
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join("-");
        let base = if base.is_empty() { "printer".to_string() } else { base };
        let taken = |id: &str| self.printers.iter().any(|p| p.id == id);
        if !taken(&base) {
            return base;
        }
        (2..).map(|n| format!("{base}-{n}")).find(|id| !taken(id)).unwrap()
    }
}

pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes).map_err(|e| format!("{}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("{}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_toml() {
        let c = Config {
            printers: vec![PrinterConfig {
                id: "ender".into(),
                name: "Ender-3 Pro".into(),
                link: Link::MarlinUsb { port: "/dev/ttyUSB0".into(), baud: 1_000_000 },
                camera: Some("http://cam/snapshot.jpg".into()),
            }],
        };
        let text = toml::to_string_pretty(&c).unwrap();
        assert!(text.contains("kind = \"marlin-usb\""), "{text}");
        assert_eq!(toml::from_str::<Config>(&text).unwrap(), c);
    }

    #[test]
    fn ids_are_unique() {
        let mut c = Config::default();
        assert_eq!(c.fresh_id("Ender-3 Pro"), "ender-3-pro");
        c.printers.push(PrinterConfig {
            id: "ender-3-pro".into(),
            name: String::new(),
            link: Link::MarlinUsb { port: String::new(), baud: 0 },
            camera: None,
        });
        assert_eq!(c.fresh_id("Ender 3 Pro"), "ender-3-pro-2");
        assert_eq!(c.fresh_id("!!"), "printer");
    }
}
