//! Persistent settings (`%APPDATA%\BonsaiH3Writer\config.json`).

use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::Context;
use serde::{Deserialize, Serialize};

use crate::gen::{GenParams, Mode};

/// Environment variable that overrides the config/data directory (tests, portable use).
pub const CONFIG_DIR_ENV: &str = "BH3_CONFIG_DIR";

/// One extra environment variable passed to the Bonsai launcher (e.g. `BONSAI_KV4=1`).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct EnvVar {
    pub key: String,
    pub value: String,
}

/// All persistent settings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    /// Bonsai-demo folder (the one with `start_bonsai.bat`, `scripts\`, `bin\`, `models\`).
    pub bonsai_dir: Option<PathBuf>,
    /// llama-server address. The Bonsai launcher always uses port 8080.
    pub server_url: String,
    /// Start Bonsai automatically when a prompt or chat message needs it.
    pub auto_start: bool,
    /// Stop Bonsai after this many idle minutes if this app started it (0 = never).
    /// Frees the 8 GB GPU for ComfyUI.
    pub auto_stop_idle_minutes: u32,
    /// Stop Bonsai when the app closes, if the app started it.
    pub stop_on_exit: bool,
    /// Refuse to auto-start while other programs use more than this much VRAM (MB, 0 = never refuse).
    pub vram_busy_mb: u32,
    /// Context size passed as `BONSAI_CTX` (0 = launcher default).
    pub ctx_size: u32,
    /// `BONSAI_MMPROJ_CPU=1`: vision projector in system RAM (frees ~0.9 GB VRAM).
    pub mmproj_cpu: bool,
    /// Extra environment variables for the launcher.
    pub extra_env: Vec<EnvVar>,
    /// Seconds to wait for the model to load.
    pub start_timeout_secs: u32,
    /// Defaults for prompt writing.
    pub writer_params: GenParams,
    /// Defaults for free chat.
    pub chat_params: GenParams,
    /// Last prompt mode used.
    pub last_mode: Mode,
    /// Default clip length (seconds) in the request builder.
    pub default_duration: u32,
    /// Save every written prompt to the history folder.
    pub save_history: bool,
    /// Show Bonsai's reasoning text when thinking is on.
    pub show_reasoning: bool,
    /// UI zoom factor.
    pub ui_scale: f32,
    /// Accent colour (Blender orange by default).
    pub accent: [u8; 3],
    /// Use a monospace font for prompt text.
    pub monospace_prompts: bool,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            bonsai_dir: None,
            server_url: "http://127.0.0.1:8080".to_string(),
            auto_start: true,
            auto_stop_idle_minutes: 10,
            stop_on_exit: true,
            vram_busy_mb: 1500,
            ctx_size: 0,
            mmproj_cpu: false,
            extra_env: Vec::new(),
            start_timeout_secs: 300,
            writer_params: GenParams::prompt_writer(),
            chat_params: GenParams::chat(),
            last_mode: Mode::R2v,
            default_duration: 6,
            save_history: true,
            show_reasoning: true,
            ui_scale: 1.0,
            accent: [232, 125, 13],
            monospace_prompts: true,
        }
    }
}

/// `%APPDATA%\BonsaiH3Writer` (or `$BH3_CONFIG_DIR`). Created on first use.
pub fn config_dir() -> PathBuf {
    let dir = match std::env::var_os(CONFIG_DIR_ENV) {
        Some(d) if !d.is_empty() => PathBuf::from(d),
        _ => dirs::config_dir().unwrap_or_else(std::env::temp_dir).join(crate::APP_ID),
    };
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// Path of `config.json`.
pub fn config_path() -> PathBuf {
    config_dir().join("config.json")
}

impl AppConfig {
    /// Load the config; a missing or unreadable file gives the defaults.
    pub fn load() -> Self {
        Self::load_from(&config_path()).unwrap_or_default()
    }

    /// Load from a specific file.
    pub fn load_from(path: &Path) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))
    }

    /// Like [`load`](Self::load), plus a warning to show when `config.json` exists but cannot be
    /// read: the defaults are used, and the file is first copied to `config.json.bad-<time>` so
    /// the next save cannot lose it. A missing file gives the defaults without a warning.
    pub fn load_with_warning() -> (Self, Option<String>) {
        Self::load_with_warning_from(&config_path())
    }

    /// [`load_with_warning`](Self::load_with_warning) for a specific file.
    pub fn load_with_warning_from(path: &Path) -> (Self, Option<String>) {
        let err = match Self::load_from(path) {
            Ok(cfg) => return (cfg, None),
            Err(_) if !path.exists() => return (Self::default(), None),
            Err(e) => e,
        };
        let mut backup = path.as_os_str().to_owned();
        backup.push(format!(".bad-{}", chrono::Local::now().format("%Y%m%d-%H%M%S")));
        let backup = PathBuf::from(backup);
        let kept = match std::fs::copy(path, &backup) {
            Ok(_) => format!("The old file was kept as {}.", backup.display()),
            Err(copy_err) => format!("It could not be copied ({copy_err}), so the next save replaces it."),
        };
        (Self::default(), Some(format!("Settings could not be loaded ({err:#}); using the defaults. {kept}")))
    }

    /// Save atomically (write a temp file, then rename).
    pub fn save(&self) -> anyhow::Result<()> {
        self.save_to(&config_path())
    }

    /// Save to a specific file. The temp file is flushed to disk before the rename, so a power
    /// cut leaves the old file or the new one, not an empty one.
    pub fn save_to(&self, path: &Path) -> anyhow::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
        }
        let tmp = path.with_extension("json.tmp");
        let text = serde_json::to_string_pretty(self)?;
        let mut file = File::create(&tmp).with_context(|| format!("writing {}", tmp.display()))?;
        file.write_all(text.as_bytes())
            .and_then(|()| file.sync_all())
            .with_context(|| format!("writing {}", tmp.display()))?;
        drop(file);
        std::fs::rename(&tmp, path).with_context(|| format!("writing {}", path.display()))?;
        Ok(())
    }

    /// Server URL without a trailing slash.
    pub fn base_url(&self) -> String {
        self.server_url.trim().trim_end_matches('/').to_string()
    }

    /// Environment pairs for the launcher from the settings.
    pub fn launcher_env(&self) -> Vec<(String, String)> {
        let mut env = Vec::new();
        if self.ctx_size > 0 {
            env.push(("BONSAI_CTX".to_string(), self.ctx_size.to_string()));
        }
        if self.mmproj_cpu {
            env.push(("BONSAI_MMPROJ_CPU".to_string(), "1".to_string()));
        }
        env.extend(
            self.extra_env
                .iter()
                // Windows cannot hold a name with '=' or spaces: such a row is left out.
                .filter(|e| {
                    let key = e.key.trim();
                    !key.is_empty() && !key.contains('=') && !key.chars().any(char::is_whitespace)
                })
                .map(|e| (e.key.trim().to_string(), e.value.clone())),
        );
        env
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_defaults_for_missing_fields() {
        let dir = std::env::temp_dir().join(format!("bh3-cfg-test-{}", std::process::id()));
        let path = dir.join("config.json");
        let cfg =
            AppConfig { bonsai_dir: Some(PathBuf::from(r"D:\Bonsai-demo")), ctx_size: 8192, ..AppConfig::default() };
        cfg.save_to(&path).unwrap();
        assert_eq!(AppConfig::load_from(&path).unwrap(), cfg);
        std::fs::write(&path, r#"{"auto_start": false}"#).unwrap();
        let partial = AppConfig::load_from(&path).unwrap();
        assert!(!partial.auto_start);
        assert_eq!(partial.server_url, "http://127.0.0.1:8080");
        assert!(!dir.join("config.json.tmp").exists(), "temp file renamed");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn unreadable_config_is_kept_and_reported() {
        let dir = std::env::temp_dir().join(format!("bh3-cfg-bad-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("config.json");
        // Missing: defaults, no warning, no backup.
        assert_eq!(AppConfig::load_with_warning_from(&path), (AppConfig::default(), None));
        let cfg = AppConfig { ctx_size: 4096, ..AppConfig::default() };
        cfg.save_to(&path).unwrap();
        assert_eq!(AppConfig::load_with_warning_from(&path), (cfg, None));

        let broken = "{\"ctx_size\": 4096,, }";
        std::fs::write(&path, broken).unwrap();
        let (loaded, warning) = AppConfig::load_with_warning_from(&path);
        assert_eq!(loaded, AppConfig::default());
        let warning = warning.expect("a warning");
        assert!(warning.contains("using the defaults") && warning.contains("config.json.bad-"), "{warning}");
        let backups: Vec<PathBuf> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.file_name().is_some_and(|n| n.to_string_lossy().starts_with("config.json.bad-")))
            .collect();
        assert_eq!(backups.len(), 1, "{backups:?}");
        assert_eq!(std::fs::read_to_string(&backups[0]).unwrap(), broken);
        // A save afterwards replaces config.json; the copy stays.
        AppConfig::default().save_to(&path).unwrap();
        assert_eq!(std::fs::read_to_string(&backups[0]).unwrap(), broken);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn launcher_env_from_settings() {
        let cfg = AppConfig {
            ctx_size: 16384,
            mmproj_cpu: true,
            extra_env: vec![EnvVar { key: "BONSAI_KV4".into(), value: "1".into() }, EnvVar::default()],
            ..AppConfig::default()
        };
        assert_eq!(
            cfg.launcher_env(),
            vec![
                ("BONSAI_CTX".to_string(), "16384".to_string()),
                ("BONSAI_MMPROJ_CPU".to_string(), "1".to_string()),
                ("BONSAI_KV4".to_string(), "1".to_string()),
            ]
        );
    }
}
