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

/// The app's own runtime: llama-server and model files that are started directly, without the
/// Bonsai-demo folder. The paths may point anywhere (files are used where they are, e.g. straight
/// into a Bonsai-demo folder) or into the app's storage (downloads and imports, see
/// [`crate::runtime`]). A model set here takes precedence over `bonsai_dir`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct RuntimeConfig {
    /// llama-server.exe to start. Unset: the newest server build in the app's storage, else the
    /// one of `bonsai_dir`.
    pub server_exe: Option<PathBuf>,
    /// Model GGUF (set = the app's runtime is used instead of the Bonsai-demo launcher).
    pub model: Option<PathBuf>,
    /// Vision projector (`*mmproj*.gguf`); unset = no image input.
    pub mmproj: Option<PathBuf>,
    /// Storage for downloads and imports (`%LOCALAPPDATA%\BonsaiH3Writer\runtime` when unset).
    pub dir: Option<PathBuf>,
    /// Where models are stored (`<dir>\models` when unset), e.g. on another drive.
    pub models_dir: Option<PathBuf>,
    /// Hugging Face token for gated or private repos (else the `HF_TOKEN` environment variable).
    pub hf_token: Option<String>,
}

impl RuntimeConfig {
    /// A model is chosen: Bonsai starts from these files, not from `bonsai_dir`'s launcher.
    pub fn is_active(&self) -> bool {
        self.model.as_ref().is_some_and(|m| !m.as_os_str().is_empty())
    }
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
    /// The app's own runtime (model and server files started directly).
    pub runtime: RuntimeConfig,
    /// `BONSAI_NGL`: model layers on the GPU (`None` = all, 0 for the CPU build).
    pub gpu_layers: Option<u32>,
    /// `BONSAI_IMAGE_MAX_TOKENS`: image token cap (`None` = 1024 unless CUDA/HIP, 0 = no cap).
    pub image_max_tokens: Option<u32>,
    /// `BONSAI_KV4=1`: 4-bit (q4_0) KV cache, ~3.5x less context memory.
    pub kv_cache_4bit: bool,
    /// Extra llama-server arguments (`--threads 8 --alias "x"`), appended last.
    pub extra_args: String,
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
    /// Write workspace: when a picture is picked for a reference, Bonsai describes it once
    /// (`crate::describe`) into the empty description, so prompts are written from text.
    pub auto_describe: bool,
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
            runtime: RuntimeConfig::default(),
            gpu_layers: None,
            image_max_tokens: None,
            kv_cache_4bit: false,
            extra_args: String::new(),
            writer_params: GenParams::prompt_writer(),
            chat_params: GenParams::chat(),
            last_mode: Mode::R2v,
            default_duration: 6,
            save_history: true,
            auto_describe: true,
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

    /// [`extra_args`](Self::extra_args) split like a command line.
    pub fn extra_server_args(&self) -> Vec<String> {
        crate::runtime::split_args(&self.extra_args)
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
        // The launcher's own names, so both the launcher script and a direct start read them.
        if let Some(ngl) = self.gpu_layers {
            env.push(("BONSAI_NGL".to_string(), ngl.to_string()));
        }
        if self.kv_cache_4bit {
            env.push(("BONSAI_KV4".to_string(), "1".to_string()));
        }
        if let Some(cap) = self.image_max_tokens {
            env.push(("BONSAI_IMAGE_MAX_TOKENS".to_string(), cap.to_string()));
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
    fn runtime_settings_round_trip_and_old_files_load() {
        let dir = std::env::temp_dir().join(format!("bh3-cfg-rt-{}", std::process::id()));
        let path = dir.join("config.json");
        let cfg = AppConfig {
            runtime: RuntimeConfig {
                server_exe: Some(PathBuf::from(r"C:\rt\bin\b\llama-server.exe")),
                model: Some(PathBuf::from(r"E:\models\M-PTQ1_0.gguf")),
                mmproj: None,
                dir: None,
                models_dir: Some(PathBuf::from(r"E:\models")),
                hf_token: Some("hf_x".into()),
            },
            gpu_layers: Some(40),
            image_max_tokens: Some(0),
            kv_cache_4bit: true,
            extra_args: "--threads 8".into(),
            ..AppConfig::default()
        };
        cfg.save_to(&path).unwrap();
        assert_eq!(AppConfig::load_from(&path).unwrap(), cfg);
        assert!(cfg.runtime.is_active());
        // A config of version 0.1.0 has none of these fields.
        std::fs::write(&path, r#"{"bonsai_dir": "D:\\Bonsai-demo", "ctx_size": 16384}"#).unwrap();
        let old = AppConfig::load_from(&path).unwrap();
        assert_eq!(old.runtime, RuntimeConfig::default());
        assert!(!old.runtime.is_active() && old.gpu_layers.is_none() && old.extra_args.is_empty());
        assert_eq!(cfg.extra_server_args(), vec!["--threads".to_string(), "8".to_string()]);
        assert_eq!(
            cfg.launcher_env(),
            vec![
                ("BONSAI_NGL".to_string(), "40".to_string()),
                ("BONSAI_KV4".to_string(), "1".to_string()),
                ("BONSAI_IMAGE_MAX_TOKENS".to_string(), "0".to_string()),
            ]
        );
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
