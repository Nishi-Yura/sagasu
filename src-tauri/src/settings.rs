use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};


#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Folders whose file names are indexed.
    pub roots: Vec<String>,
    /// Folder names or path fragments to skip while indexing.
    pub excludes: Vec<String>,
    /// Index text inside documents for full-text search.
    pub content_enabled: bool,
    /// Skip content extraction for files larger than this.
    pub content_max_mb: u64,
    pub hotkey: String,
    pub autostart: bool,
    pub show_hidden: bool,
    /// Look for a new version on GitHub at startup.
    pub check_updates: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            roots: default_roots(),
            excludes: default_excludes(),
            content_enabled: true,
            content_max_mb: 30,
            hotkey: "Alt+Space".into(),
            autostart: false,
            show_hidden: false,
            check_updates: true,
        }
    }
}

fn default_roots() -> Vec<String> {
    let mut roots = Vec::new();
    if let Some(home) = dirs::home_dir() {
        roots.push(home.to_string_lossy().into_owned());
    }
    roots
}

pub fn default_excludes() -> Vec<String> {
    [
        "AppData",
        "node_modules",
        ".git",
        "$Recycle.Bin",
        "System Volume Information",
        "C:\\Windows",
        "C:\\Program Files",
        "C:\\Program Files (x86)",
        "C:\\ProgramData",
        ".cache",
        ".cargo",
        ".rustup",
        ".npm",
        "__pycache__",
        ".venv",
        "target\\debug",
        "target\\release",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

impl Settings {
    pub fn load(path: &Path) -> Self {
        let Ok(s) = std::fs::read_to_string(path) else { return Self::default() };
        // Notepad and PowerShell 5 write a UTF-8 BOM, which serde_json rejects.
        match serde_json::from_str(s.trim_start_matches('\u{feff}')) {
            Ok(v) => v,
            Err(e) => {
                log::warn!("settings.json is invalid ({e}); using defaults");
                Self::default()
            }
        }
    }

    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(path, serde_json::to_string_pretty(self)?)?;
        Ok(())
    }

    pub fn root_paths(&self) -> Vec<PathBuf> {
        self.roots.iter().map(PathBuf::from).filter(|p| p.exists()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loads_with_bom_and_partial_fields() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("settings.json");
        std::fs::write(&p, "\u{feff}{\"roots\":[\"C:\\\\Demo\"]}").unwrap();
        let s = Settings::load(&p);
        assert_eq!(s.roots, vec!["C:\\Demo".to_string()]);
        assert_eq!(s.hotkey, "Alt+Space", "missing fields fall back to defaults");
    }
}
