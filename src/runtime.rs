//! Model and runtime manager: run Bonsai without the Bonsai-demo folder.
//!
//! - **Catalog** of the Bonsai 2 GGUFs this app knows ([`CATALOG`], sizes and sha256 from the
//!   Hugging Face file list), plus any Hugging Face repo / file ([`ModelSource`]) or a local
//!   `.gguf` that stays where it is (the config only stores its path).
//! - **Server builds**: PrismML's llama.cpp fork (the Bonsai 2 formats PTQ1_0 / PQ2_0 do not load
//!   in stock llama.cpp), release [`RELEASE_TAG`] from GitHub. [`choose_backend`] picks the build
//!   like the demo's `setup.ps1`: `nvidia-smi` "CUDA Version: X.Y" >= 13.3 -> CUDA 13.3, >= 12.4 ->
//!   CUDA 12.4, else Vulkan when `vulkaninfo` exists, else CPU. A CUDA build is the release zip +
//!   the CUDA runtime zip, a Vulkan build the CPU zip + the Vulkan zip, extracted into one folder.
//!   Only `llama-server.exe` and its DLLs are kept ([`keep_server_file`]).
//! - **Downloads** ([`download_file`]): resumable (`<file>.part` + HTTP `Range`), sha256 checked
//!   before the file is renamed into place, progress callback, cancel flag ([`Cancelled`] keeps
//!   the `.part`), clear errors for 404 / 401 / 403 / a full disk. Redirects are followed by hand
//!   so `Range` survives them and a token is never sent to another host.
//! - **Storage**: `%LOCALAPPDATA%\BonsaiH3Writer\runtime\` (or `runtime.dir`): `bin\<tag>-<backend>\`
//!   for server builds, `models\<owner>--<repo>\` for downloads and `models\local\` for imports
//!   (or `runtime.models_dir`, e.g. on another drive). [`scan_models`] lists what is there.
//! - **Import** from a Bonsai-demo folder ([`RuntimeTask::Import`]): copy (default) or move the
//!   server build, the model and its projector into the app's storage.
//! - [`run_task`] runs one of these jobs (CLI directly, the GUI on a service thread).
//! - [`missing_system_dlls`]: the server build needs the Visual C++ runtime (VCRUNTIME140,
//!   MSVCP140, VCOMP140); a fresh PC may lack it, so it is checked before a start.

use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{bail, Context};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::config::AppConfig;

/// Release of PrismML's llama.cpp fork the server builds come from.
pub const RELEASE_TAG: &str = "prism-b10787-73bfe6c";
/// Download base of the release assets (`<base>/<tag>/<asset>`).
pub const RELEASE_DOWNLOAD_URL: &str = "https://github.com/PrismML-Eng/llama.cpp/releases/download";
/// GitHub API of the releases (`<base>/<tag>` lists the assets with their sha256 digests).
pub const RELEASE_API_URL: &str = "https://api.github.com/repos/PrismML-Eng/llama.cpp/releases/tags";
/// Hugging Face (`<base>/<repo>/resolve/main/<file>`, `<base>/api/models/<repo>/tree/main`).
pub const HF_BASE_URL: &str = "https://huggingface.co";
/// Environment variable with a Hugging Face token (used when the config has none).
pub const HF_TOKEN_ENV: &str = "HF_TOKEN";
/// Microsoft's Visual C++ 2015-2022 redistributable (x64), which the server build needs.
pub const VC_REDIST_URL: &str = "https://aka.ms/vs/17/release/vc_redist.x64.exe";
/// Marker written into each server build folder.
pub const BUILD_INFO_FILE: &str = "build.json";
/// The llama-server web UI defaults copied from a Bonsai-demo folder (MCP servers of the web UI).
pub const WEBUI_CONFIG_FILE: &str = "webui-config.json";
/// Folder under `models\` for files imported from a Bonsai-demo folder.
pub const LOCAL_MODELS_DIR: &str = "local";

const SERVER_EXE: &str = if cfg!(windows) { "llama-server.exe" } else { "llama-server" };
const USER_AGENT: &str = concat!("bonsai-h3-writer/", env!("CARGO_PKG_VERSION"));
/// Read / write buffer of downloads and copies.
const CHUNK: usize = 1024 * 1024;
/// A download needs this much free space on top of the file.
const DISK_RESERVE: u64 = 64 * 1024 * 1024;
/// Progress is reported at most this often (and on every new file).
const PROGRESS_EVERY: Duration = Duration::from_millis(200);
/// Speed is averaged over this window.
const RATE_WINDOW: Duration = Duration::from_secs(6);
/// Redirects followed by hand (GitHub and Hugging Face redirect once or twice to a CDN).
const MAX_REDIRECTS: usize = 8;
/// Tries of the Hugging Face file list (it sometimes times out).
const TREE_TRIES: usize = 3;

// ---------------------------------------------------------------------------------------------
// Catalog

/// One file of a catalog model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct CatalogFile {
    pub name: &'static str,
    pub bytes: u64,
    /// sha256 (hex) from the Hugging Face file list (the LFS oid).
    pub sha256: &'static str,
}

/// A model the app can download by id.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct CatalogModel {
    /// `27b-ptq1`, ... (what `bh3 models download <id>` takes).
    pub id: &'static str,
    pub title: &'static str,
    /// Hugging Face repo (`owner/name`).
    pub repo: &'static str,
    pub model: CatalogFile,
    /// The vision projector that pairs with it.
    pub mmproj: Option<CatalogFile>,
    /// The recommended model (Download Bonsai).
    pub default: bool,
    pub note: &'static str,
}

impl CatalogModel {
    /// Model + projector bytes.
    pub fn total_bytes(&self) -> u64 {
        self.model.bytes + self.mmproj.map_or(0, |m| m.bytes)
    }

    /// What to download for it.
    pub fn source(&self) -> ModelSource {
        ModelSource {
            repo: self.repo.to_string(),
            file: self.model.name.to_string(),
            mmproj: self.mmproj.map(|m| m.name.to_string()),
        }
    }
}

const REPO_27B: &str = "prism-ml/Ternary-Bonsai-2-27B-gguf";
const MMPROJ_27B: CatalogFile = CatalogFile {
    name: "Ternary-Bonsai-2-27B-mmproj-Q8_0.gguf",
    bytes: 629_246_976,
    sha256: "6807ede61d570bb86ba34b756a0fa109edc33668604de867c6ea6d8f1d631903",
};

/// The Bonsai 2 models (sizes and sha256 from the Hugging Face file lists, 2026-10-28).
pub const CATALOG: [CatalogModel; 5] = [
    CatalogModel {
        id: "27b-ptq1",
        title: "Bonsai 2 27B PTQ1_0 (recommended)",
        repo: REPO_27B,
        model: CatalogFile {
            name: "Ternary-Bonsai-2-27B-PTQ1_0.gguf",
            bytes: 5_946_648_928,
            sha256: "53107f530aa52eb00912263ab1ee29bd199261c87cd7b4ad4ca1318c1fe33ee3",
        },
        mmproj: Some(MMPROJ_27B),
        default: true,
        note: "1.75 bits per weight, ~7.7 GB VRAM with 16k context: fits an 8 GB GPU",
    },
    CatalogModel {
        id: "27b-pq2",
        title: "Bonsai 2 27B PQ2_0",
        repo: REPO_27B,
        model: CatalogFile {
            name: "Ternary-Bonsai-2-27B-PQ2_0.gguf",
            bytes: 7_206_168_928,
            sha256: "3907dc1658db1f78a9826bf8d5bcb8dc65db0d466388937af57f2294fae62ec1",
        },
        mmproj: Some(MMPROJ_27B),
        default: false,
        note: "2.13 bits per weight, faster prompt processing; needs more than 8 GB VRAM",
    },
    CatalogModel {
        id: "9b-ptq1",
        title: "Bonsai 2 9B PTQ1_0",
        repo: "prism-ml/Ternary-Bonsai-2-9B-gguf",
        model: CatalogFile {
            name: "Ternary-Bonsai-2-9B-PTQ1_0.gguf",
            bytes: 1_997_486_976,
            sha256: "3561df7ac7d26a71ffa991ac2db740b10841c3a350aa2676ccd3ff5d428b4e8b",
        },
        mmproj: Some(CatalogFile {
            name: "Ternary-Bonsai-2-9B-mmproj-Q8_0.gguf",
            bytes: 624_229_664,
            sha256: "e0cb6f6988cea323ef798cd77d019bbb819af75582f034eaab0e7232314953db",
        }),
        default: false,
        note: "Smaller and faster; leaves room on the GPU, weaker H3 prompts",
    },
    CatalogModel {
        id: "4b-ptq1",
        title: "Bonsai 2 4B PTQ1_0",
        repo: "prism-ml/Ternary-Bonsai-2-4B-gguf",
        model: CatalogFile {
            name: "Ternary-Bonsai-2-4B-PTQ1_0.gguf",
            bytes: 949_549_920,
            sha256: "56711263ec9a9ffeaa74a3a0e58dece8127cf9268404746c721113ca76994d60",
        },
        mmproj: Some(CatalogFile {
            name: "Ternary-Bonsai-2-4B-mmproj-Q8_0.gguf",
            bytes: 366_894_624,
            sha256: "db939d5f9b7014137810205e91d05e5f54afd72b0b3af9d33bd7d82f7906a55d",
        }),
        default: false,
        note: "Small: runs next to ComfyUI or on the CPU",
    },
    CatalogModel {
        id: "2b-ptq1",
        title: "Bonsai 2 2B PTQ1_0",
        repo: "prism-ml/Ternary-Bonsai-2-2B-gguf",
        model: CatalogFile {
            name: "Ternary-Bonsai-2-2B-PTQ1_0.gguf",
            bytes: 429_183_456,
            sha256: "36061ad406df8a4903a6db005be350c50fabfaf717f9ef0acf4b76adcd9929fa",
        },
        mmproj: Some(CatalogFile {
            name: "Ternary-Bonsai-2-2B-mmproj-Q8_0.gguf",
            bytes: 364_664_352,
            sha256: "ac9934e8d7966d0d62442c202ec8fdad99a045dea79366c1a4a246a46f5c0533",
        }),
        default: false,
        note: "Tiny: for tests and very small machines",
    },
];

/// The recommended model (27B PTQ1_0).
pub fn default_model() -> &'static CatalogModel {
    CATALOG.iter().find(|m| m.default).unwrap_or(&CATALOG[0])
}

/// Catalog entry by id (any case).
pub fn catalog_model(id: &str) -> Option<&'static CatalogModel> {
    let id = id.trim();
    CATALOG.iter().find(|m| m.id.eq_ignore_ascii_case(id))
}

/// Catalog entry whose model file is `file_name` (any case).
pub fn catalog_by_file(file_name: &str) -> Option<&'static CatalogModel> {
    CATALOG.iter().find(|m| m.model.name.eq_ignore_ascii_case(file_name))
}

/// Known size and sha256 of a file in a catalog repo.
fn catalog_file(repo: &str, file: &str) -> Option<CatalogFile> {
    CATALOG
        .iter()
        .filter(|m| m.repo.eq_ignore_ascii_case(repo))
        .find_map(|m| std::iter::once(m.model).chain(m.mmproj).find(|f| f.name.eq_ignore_ascii_case(file)))
}

/// A model on Hugging Face: repo, GGUF and optional vision projector.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelSource {
    /// `owner/name`.
    pub repo: String,
    /// File in the repo (may contain `/`).
    pub file: String,
    pub mmproj: Option<String>,
}

impl ModelSource {
    /// A catalog id (`27b-ptq1`), `hf:owner/name/file.gguf`, `owner/name/file.gguf` or a
    /// `https://huggingface.co/owner/name/(resolve|blob)/main/file.gguf` link. `mmproj` (a file of
    /// the same repo) replaces the catalog's projector; `none` downloads none.
    pub fn parse(spec: &str, mmproj: Option<&str>) -> anyhow::Result<Self> {
        let spec = spec.trim();
        let mut source = if let Some(model) = catalog_model(spec) {
            model.source()
        } else {
            let rest = spec.strip_prefix("hf:").unwrap_or(spec);
            let rest = rest
                .strip_prefix("https://huggingface.co/")
                .or_else(|| rest.strip_prefix("http://huggingface.co/"))
                .or_else(|| rest.strip_prefix("huggingface.co/"))
                .unwrap_or(rest);
            let parts: Vec<&str> = rest.split('/').filter(|p| !p.is_empty()).collect();
            if parts.len() < 3 {
                bail!(
                    "'{spec}' is neither a catalog id ({}) nor hf:<owner>/<repo>/<file.gguf>",
                    CATALOG.iter().map(|m| m.id).collect::<Vec<_>>().join(", ")
                );
            }
            // A pasted link: owner/name/resolve/main/file or owner/name/blob/main/file.
            let file_parts =
                if parts.len() > 4 && matches!(parts[2], "resolve" | "blob") { &parts[4..] } else { &parts[2..] };
            let file = file_parts.join("/");
            let file = file.split(['?', '#']).next().unwrap_or(&file).to_string();
            if !file.to_ascii_lowercase().ends_with(".gguf") {
                bail!("'{file}' is not a .gguf file");
            }
            ModelSource { repo: format!("{}/{}", parts[0], parts[1]), file, mmproj: None }
        };
        if let Some(mmproj) = mmproj.map(str::trim).filter(|m| !m.is_empty()) {
            source.mmproj = if mmproj.eq_ignore_ascii_case("none") { None } else { Some(mmproj.to_string()) };
        }
        if source.repo.split('/').count() != 2 || source.repo.contains("..") || source.file.contains("..") {
            bail!("'{}' is not a valid Hugging Face repo / file", spec);
        }
        Ok(source)
    }

    /// `hf:owner/name/file`.
    pub fn spec(&self) -> String {
        format!("hf:{}/{}", self.repo, self.file)
    }

    /// The catalog entry for this exact repo + file.
    pub fn catalog(&self) -> Option<&'static CatalogModel> {
        CATALOG
            .iter()
            .find(|m| m.repo.eq_ignore_ascii_case(&self.repo) && m.model.name.eq_ignore_ascii_case(&self.file))
    }

    /// Folder the files go to under `models_dir`.
    pub fn dest_dir(&self, models_dir: &Path) -> PathBuf {
        models_dir.join(repo_dir_name(&self.repo))
    }

    /// Model path after the download.
    pub fn model_path(&self, models_dir: &Path) -> PathBuf {
        self.dest_dir(models_dir).join(file_leaf(&self.file))
    }

    /// Projector path after the download.
    pub fn mmproj_path(&self, models_dir: &Path) -> Option<PathBuf> {
        self.mmproj.as_ref().map(|m| self.dest_dir(models_dir).join(file_leaf(m)))
    }
}

/// `owner/name` -> `owner--name` (one folder per repo, like the Hugging Face cache).
pub fn repo_dir_name(repo: &str) -> String {
    repo.trim().replace('/', "--").replace(['\\', ':'], "_")
}

/// `owner--name` -> `owner/name`.
fn repo_from_dir_name(name: &str) -> Option<String> {
    let (owner, repo) = name.split_once("--")?;
    (!owner.is_empty() && !repo.is_empty()).then(|| format!("{owner}/{repo}"))
}

/// Last path part of a repo file (`sub/x.gguf` -> `x.gguf`).
fn file_leaf(file: &str) -> &str {
    file.rsplit('/').next().unwrap_or(file)
}

// ---------------------------------------------------------------------------------------------
// Paths

/// `%LOCALAPPDATA%\BonsaiH3Writer\runtime`; with `BH3_CONFIG_DIR` set, `<config dir>\runtime`
/// (tests and portable use keep everything in one place).
pub fn default_runtime_dir() -> PathBuf {
    match std::env::var_os(crate::config::CONFIG_DIR_ENV) {
        Some(d) if !d.is_empty() => PathBuf::from(d).join("runtime"),
        _ => dirs::data_local_dir().unwrap_or_else(std::env::temp_dir).join(crate::APP_ID).join("runtime"),
    }
}

/// The runtime folder (`runtime.dir`, else [`default_runtime_dir`]).
pub fn runtime_dir(cfg: &AppConfig) -> PathBuf {
    cfg.runtime.dir.clone().filter(|d| !d.as_os_str().is_empty()).unwrap_or_else(default_runtime_dir)
}

/// Where models are stored (`runtime.models_dir`, else `<runtime>\models`).
pub fn models_dir(cfg: &AppConfig) -> PathBuf {
    cfg.runtime
        .models_dir
        .clone()
        .filter(|d| !d.as_os_str().is_empty())
        .unwrap_or_else(|| runtime_dir(cfg).join("models"))
}

/// `<runtime>\bin`.
pub fn bin_dir(cfg: &AppConfig) -> PathBuf {
    runtime_dir(cfg).join("bin")
}

/// `<runtime>\bin\<tag>-<backend>`.
pub fn build_dir(cfg: &AppConfig, tag: &str, backend: Backend) -> PathBuf {
    bin_dir(cfg).join(format!("{tag}-{}", backend.id()))
}

/// `<runtime>\downloads` (zips while they download).
pub fn downloads_dir(cfg: &AppConfig) -> PathBuf {
    runtime_dir(cfg).join("downloads")
}

/// The Hugging Face token: `runtime.hf_token`, else `HF_TOKEN`.
pub fn hf_token(cfg: &AppConfig) -> Option<String> {
    cfg.runtime
        .hf_token
        .clone()
        .or_else(|| std::env::var(HF_TOKEN_ENV).ok())
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
}

/// Free bytes on the drive of `path` (the nearest existing folder). `None` when unknown.
pub fn free_space(path: &Path) -> Option<u64> {
    let existing = path.ancestors().find(|p| p.is_dir())?;
    disk_free(existing)
}

#[cfg(windows)]
fn disk_free(dir: &Path) -> Option<u64> {
    use std::os::windows::ffi::OsStrExt;
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetDiskFreeSpaceExW(dir: *const u16, free: *mut u64, total: *mut u64, total_free: *mut u64) -> i32;
    }
    let wide: Vec<u16> = dir.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
    let mut free = 0u64;
    // SAFETY: `wide` is NUL-terminated and outlives the call; the out pointers are valid or null.
    let ok = unsafe { GetDiskFreeSpaceExW(wide.as_ptr(), &mut free, std::ptr::null_mut(), std::ptr::null_mut()) };
    (ok != 0).then_some(free)
}

#[cfg(not(windows))]
fn disk_free(_dir: &Path) -> Option<u64> {
    None
}

/// `5.5 GB` / `600 MB` (binary units, like Explorer).
pub fn fmt_bytes(bytes: u64) -> String {
    let mb = bytes as f64 / (1024.0 * 1024.0);
    if mb >= 1024.0 {
        format!("{:.1} GB", mb / 1024.0)
    } else {
        format!("{mb:.0} MB")
    }
}

// ---------------------------------------------------------------------------------------------
// Server builds

/// Which llama-server build.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Backend {
    #[serde(rename = "cuda-12.4")]
    Cuda124,
    #[serde(rename = "cuda-13.3")]
    Cuda133,
    #[serde(rename = "vulkan")]
    Vulkan,
    #[serde(rename = "cpu")]
    Cpu,
}

impl Backend {
    pub const ALL: [Backend; 4] = [Backend::Cuda133, Backend::Cuda124, Backend::Vulkan, Backend::Cpu];

    /// `cuda-12.4`, `cuda-13.3`, `vulkan`, `cpu`.
    pub fn id(self) -> &'static str {
        match self {
            Backend::Cuda124 => "cuda-12.4",
            Backend::Cuda133 => "cuda-13.3",
            Backend::Vulkan => "vulkan",
            Backend::Cpu => "cpu",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Backend::Cuda124 => "CUDA 12.4",
            Backend::Cuda133 => "CUDA 13.3",
            Backend::Vulkan => "Vulkan",
            Backend::Cpu => "CPU",
        }
    }

    pub fn from_id(id: &str) -> Option<Self> {
        let id = id.trim().to_ascii_lowercase();
        Self::ALL.into_iter().find(|b| b.id() == id || b.id().replace(['-', '.'], "") == id.replace(['-', '.'], ""))
    }

    /// Release assets, extracted into one folder in this order.
    pub fn assets(self, tag: &str) -> Vec<String> {
        match self {
            Backend::Cuda124 | Backend::Cuda133 => {
                let cuda = if self == Backend::Cuda124 { "12.4" } else { "13.3" };
                vec![
                    format!("llama-{tag}-bin-win-cuda-{cuda}-x64.zip"),
                    format!("cudart-llama-bin-win-cuda-{cuda}-x64.zip"),
                ]
            }
            Backend::Vulkan => {
                vec![format!("llama-{tag}-bin-win-cpu-x64.zip"), format!("llama-{tag}-bin-win-vulkan-x64.zip")]
            }
            Backend::Cpu => vec![format!("llama-{tag}-bin-win-cpu-x64.zip")],
        }
    }

    /// Download size of [`RELEASE_TAG`]'s assets.
    pub fn download_bytes(self) -> u64 {
        self.assets(RELEASE_TAG).iter().map(|a| known_asset(a).map_or(0, |k| k.bytes)).sum()
    }

    /// A GPU build (CUDA / Vulkan).
    pub fn is_gpu(self) -> bool {
        self != Backend::Cpu
    }
}

impl std::fmt::Display for Backend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.id())
    }
}

/// Sizes and sha256 digests of [`RELEASE_TAG`]'s Windows x64 assets (GitHub releases API,
/// 2026-10-28); other tags are looked up in the API at download time.
const KNOWN_ASSETS: [CatalogFile; 6] = [
    CatalogFile {
        name: "llama-prism-b10787-73bfe6c-bin-win-cuda-12.4-x64.zip",
        bytes: 314_639_598,
        sha256: "64ca6810aae0e6bbb28d22f888bd009b9bdd0c24c5af8a6a7490b3eaf75ccd57",
    },
    CatalogFile {
        name: "llama-prism-b10787-73bfe6c-bin-win-cuda-13.3-x64.zip",
        bytes: 179_514_746,
        sha256: "20c45aecad3f4ede81c64ababc03f1dc10103fbd4cfb09244a16d21a3dd1db65",
    },
    CatalogFile {
        name: "cudart-llama-bin-win-cuda-12.4-x64.zip",
        bytes: 391_443_627,
        sha256: "8c79a9b226de4b3cacfd1f83d24f962d0773be79f1e7b75c6af4ded7e32ae1d6",
    },
    CatalogFile {
        name: "cudart-llama-bin-win-cuda-13.3-x64.zip",
        bytes: 390_970_417,
        sha256: "1462a050eb4c684921ba51dcc4cc488a036674c3e73e9945ee705b854808d03e",
    },
    CatalogFile {
        name: "llama-prism-b10787-73bfe6c-bin-win-vulkan-x64.zip",
        bytes: 31_308_770,
        sha256: "16d9482224e53d462eb210a766c8d94f0461cdca4d1c7263b2ba7b97092bf739",
    },
    CatalogFile {
        name: "llama-prism-b10787-73bfe6c-bin-win-cpu-x64.zip",
        bytes: 19_539_022,
        sha256: "ab1ea7cfc23fdac244088fd6f1762d0efa24d3d79fe18d4f16c496cc00775728",
    },
];

fn known_asset(name: &str) -> Option<CatalogFile> {
    KNOWN_ASSETS.iter().find(|a| a.name == name).copied()
}

/// The build [`choose_backend`] picked and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BackendChoice {
    pub backend: Backend,
    /// `CUDA Version` of the NVIDIA driver (`None`: no driver / nvidia-smi).
    pub cuda: Option<(u32, u32)>,
    pub reason: String,
}

/// `CUDA Version: 13.0` (or `CUDA UMD Version`) from `nvidia-smi`'s banner.
pub fn cuda_version(nvidia_smi: &str) -> Option<(u32, u32)> {
    let re = regex::Regex::new(r"CUDA(?:\s+UMD)?\s+Version:\s+(\d+)\.(\d+)").ok()?;
    let caps = re.captures(nvidia_smi)?;
    Some((caps[1].parse().ok()?, caps[2].parse().ok()?))
}

/// The build for this PC, as the demo's `setup.ps1` picks it: CUDA >= 13.3 -> 13.3, >= 12.4 ->
/// 12.4; else Vulkan when `vulkaninfo` exists, else CPU. `nvidia_smi`: its output, `None` when
/// it is missing.
pub fn choose_backend(nvidia_smi: Option<&str>, vulkan: bool) -> BackendChoice {
    let cuda = nvidia_smi.and_then(cuda_version);
    let fallback = if vulkan { Backend::Vulkan } else { Backend::Cpu };
    let fallback_note =
        if vulkan { "the Vulkan build is used" } else { "the CPU build is used (slow: a 27B answer takes minutes)" };
    let (backend, reason) = match cuda {
        Some((major, minor)) if (major, minor) >= (13, 3) => {
            (Backend::Cuda133, format!("NVIDIA driver with CUDA {major}.{minor}: CUDA 13.3 build"))
        }
        Some((major, minor)) if (major, minor) >= (12, 4) => {
            (Backend::Cuda124, format!("NVIDIA driver with CUDA {major}.{minor}: CUDA 12.4 build"))
        }
        Some((major, minor)) => (
            fallback,
            format!(
                "The NVIDIA driver only supports CUDA {major}.{minor} (12.4 or newer is needed) - update the driver \
                 for the GPU build; until then {fallback_note}"
            ),
        ),
        None if nvidia_smi.is_some() => {
            (fallback, format!("nvidia-smi did not report a CUDA version - {fallback_note}"))
        }
        None => (
            fallback,
            format!(
                "No NVIDIA driver found (nvidia-smi is missing) - install the NVIDIA driver for the CUDA build; \
                 until then {fallback_note}"
            ),
        ),
    };
    BackendChoice { backend, cuda, reason }
}

/// [`choose_backend`] for this PC (runs `nvidia-smi`, looks for `vulkaninfo` on PATH).
pub fn detect_backend() -> BackendChoice {
    let smi = crate::gpu::nvidia_smi_candidates().into_iter().find_map(|exe| {
        let out = crate::server::run_hidden(&exe, std::iter::empty::<&str>(), Duration::from_secs(8)).ok()?;
        (out.success || !out.stdout.is_empty()).then_some(out.stdout)
    });
    choose_backend(smi.as_deref(), on_path(if cfg!(windows) { "vulkaninfo.exe" } else { "vulkaninfo" }))
}

/// A program file on PATH.
fn on_path(name: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|path| std::env::split_paths(&path).any(|dir| dir.join(name).is_file()))
}

/// Files of a release zip (or a Bonsai-demo `bin` folder) a server build needs: `llama-server.exe`
/// and the DLLs, without the other tools (`llama-cli.exe`, ...) and their `*-impl.dll`.
pub fn keep_server_file(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    if name == SERVER_EXE {
        return true;
    }
    if !name.ends_with(".dll") {
        return false;
    }
    !name.ends_with("-impl.dll") || name == "llama-server-impl.dll"
}

/// Extract the server files of a zip into `dest` (flat: folders in the zip are dropped).
/// Returns the file names written.
pub fn extract_server_zip(zip_path: &Path, dest: &Path) -> anyhow::Result<Vec<String>> {
    let file = File::open(zip_path).with_context(|| format!("opening {}", zip_path.display()))?;
    let mut archive = zip::ZipArchive::new(io::BufReader::new(file))
        .with_context(|| format!("{} is not a zip file", zip_path.display()))?;
    fs::create_dir_all(dest).with_context(|| format!("creating {}", dest.display()))?;
    let mut written = Vec::new();
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i).with_context(|| format!("reading {}", zip_path.display()))?;
        if entry.is_dir() {
            continue;
        }
        // Only the bare file name counts, so an entry can never land outside `dest`.
        let Some(name) = entry.enclosed_name().and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
        else {
            continue;
        };
        if !keep_server_file(&name) {
            continue;
        }
        let out_path = dest.join(&name);
        let mut out = File::create(&out_path).with_context(|| format!("writing {}", out_path.display()))?;
        io::copy(&mut entry, &mut out).map_err(|e| io_error(e, &out_path))?;
        written.push(name);
    }
    Ok(written)
}

/// `build.json` in a server build folder.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuildInfo {
    pub tag: String,
    pub backend: String,
    /// Where it came from (asset names or the imported folder).
    #[serde(default)]
    pub source: Vec<String>,
}

/// A server build in `<runtime>\bin`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct InstalledBuild {
    pub dir: PathBuf,
    pub exe: PathBuf,
    pub tag: String,
    /// `cuda-13.3`, ... (from `build.json`, the demo's `.llama_release` or the folder name).
    pub backend: String,
}

/// Server builds under `<runtime>\bin` (folders with `llama-server.exe`), GPU builds first.
pub fn installed_builds(cfg: &AppConfig) -> Vec<InstalledBuild> {
    let Ok(entries) = fs::read_dir(bin_dir(cfg)) else { return Vec::new() };
    let mut builds: Vec<InstalledBuild> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|dir| dir.is_dir() && !file_name(dir).starts_with('.'))
        .filter_map(|dir| {
            let exe = dir.join(SERVER_EXE);
            if !exe.is_file() {
                return None;
            }
            let (tag, backend) = match read_build_info(&dir) {
                Some(info) => (info.tag, info.backend),
                None => {
                    let name = file_name(&dir);
                    let backend = Backend::ALL.into_iter().find(|b| name.ends_with(b.id())).map_or("?", Backend::id);
                    (name.trim_end_matches(backend).trim_end_matches('-').to_string(), backend.to_string())
                }
            };
            Some(InstalledBuild { dir, exe, tag, backend })
        })
        .collect();
    let rank = |b: &InstalledBuild| {
        Backend::from_id(&b.backend).map_or(9, |k| Backend::ALL.iter().position(|x| *x == k).unwrap_or(9))
    };
    builds.sort_by_key(|b| (b.tag != RELEASE_TAG, rank(b), file_name(&b.dir)));
    builds
}

fn read_build_info(dir: &Path) -> Option<BuildInfo> {
    serde_json::from_str(&fs::read_to_string(dir.join(BUILD_INFO_FILE)).ok()?).ok()
}

/// `cuda`, `hip`, `vulkan` or `cpu` for a llama-server.exe: by its folder name (`bin\cuda`,
/// `bin\<tag>-cuda-13.3`), else by the backend DLLs next to it. `None` when unknown.
pub fn build_kind(exe: &Path) -> Option<&'static str> {
    let dir = exe.parent()?;
    let name = file_name(dir).to_ascii_lowercase();
    for kind in ["cuda", "hip", "vulkan", "cpu"] {
        if name == kind || name.contains(&format!("-{kind}")) {
            return Some(kind);
        }
    }
    for (dll, kind) in [("ggml-cuda.dll", "cuda"), ("ggml-hip.dll", "hip"), ("ggml-vulkan.dll", "vulkan")] {
        if dir.join(dll).is_file() {
            return Some(kind);
        }
    }
    dir.join("ggml-base.dll").is_file().then_some("cpu")
}

// ---------------------------------------------------------------------------------------------
// Installed models

/// A model GGUF found in the models folder (or configured elsewhere).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct InstalledModel {
    /// File name without `.gguf` (what `bh3 models use` / `remove` take, besides a catalog id).
    pub id: String,
    pub path: PathBuf,
    pub bytes: u64,
    /// The vision projector next to it.
    pub mmproj: Option<PathBuf>,
    pub mmproj_bytes: Option<u64>,
    /// Hugging Face repo (from the `owner--name` folder).
    pub repo: Option<String>,
    /// Catalog entry with the same file name.
    pub catalog_id: Option<&'static str>,
    /// Inside the app's models folder (only those can be deleted from the app).
    pub managed: bool,
}

/// A model GGUF (not a projector, drafter, KV bias or partial download).
pub fn is_model_file(path: &Path) -> bool {
    let name = file_name(path).to_ascii_lowercase();
    name.ends_with(".gguf")
        && !["mmproj", "dspark", "dflash", "kv-bias"].iter().any(|w| name.contains(w))
        && !name.starts_with('.')
}

fn is_mmproj_file(path: &Path) -> bool {
    let name = file_name(path).to_ascii_lowercase();
    name.ends_with(".gguf") && name.contains("mmproj")
}

/// The vision projector for `model`: a `*mmproj*.gguf` in the same folder, the one sharing the
/// longest name prefix with the model (`Ternary-Bonsai-2-27B-...` pairs with
/// `Ternary-Bonsai-2-27B-mmproj-Q8_0.gguf`).
pub fn pair_mmproj(model: &Path) -> Option<PathBuf> {
    let dir = model.parent()?;
    let model_name = file_name(model).to_ascii_lowercase();
    let mut found: Vec<PathBuf> =
        fs::read_dir(dir).ok()?.flatten().map(|e| e.path()).filter(|p| p.is_file() && is_mmproj_file(p)).collect();
    found.sort();
    found.into_iter().max_by_key(|p| {
        let name = file_name(p).to_ascii_lowercase();
        let common = model_name.chars().zip(name.chars()).take_while(|(a, b)| a == b).count();
        // Q8_0 before BF16 at the same prefix (smaller, what the demo uses).
        (common, name.contains("q8_0"))
    })
}

/// Model GGUFs under `dir` (two folder levels), sorted by path.
pub fn scan_models(dir: &Path) -> Vec<InstalledModel> {
    let mut paths = Vec::new();
    collect_files(dir, 0, 2, &mut paths);
    paths.retain(|p| is_model_file(p));
    paths.sort_by_cached_key(|p| p.to_string_lossy().to_lowercase());
    paths.into_iter().map(|p| describe_model(&p, Some(dir))).collect()
}

/// Facts about one model file (`models_dir`: the app's folder, for `managed` and `repo`).
pub fn describe_model(path: &Path, models_dir: Option<&Path>) -> InstalledModel {
    let name = file_name(path);
    let mmproj = pair_mmproj(path);
    let managed = models_dir.is_some_and(|d| is_inside(path, d));
    let repo = path.parent().filter(|_| managed).and_then(|p| repo_from_dir_name(&file_name(p)));
    InstalledModel {
        id: name.strip_suffix(".gguf").or_else(|| name.strip_suffix(".GGUF")).unwrap_or(&name).to_string(),
        bytes: fs::metadata(path).map(|m| m.len()).unwrap_or(0),
        mmproj_bytes: mmproj.as_ref().and_then(|m| fs::metadata(m).ok()).map(|m| m.len()),
        mmproj,
        repo,
        catalog_id: catalog_by_file(&name).map(|m| m.id),
        managed,
        path: path.to_path_buf(),
    }
}

/// The model `key` names: a catalog id, an id / file name (any case) or a path.
pub fn find_model<'a>(models: &'a [InstalledModel], key: &str) -> Option<&'a InstalledModel> {
    let key = key.trim();
    let lower = key.to_ascii_lowercase();
    models
        .iter()
        .find(|m| m.catalog_id.is_some_and(|id| id.eq_ignore_ascii_case(key)))
        .or_else(|| models.iter().find(|m| m.id.to_ascii_lowercase() == lower))
        .or_else(|| models.iter().find(|m| file_name(&m.path).to_ascii_lowercase() == lower))
        .or_else(|| models.iter().find(|m| same_file(&m.path, Path::new(key))))
}

/// Delete a model of the app's models folder: the GGUF, its `.part`, and its projector when no
/// other model in that folder uses it. Files outside `models_dir` are never deleted.
pub fn remove_model(model: &InstalledModel, models_dir: &Path) -> anyhow::Result<Vec<PathBuf>> {
    if !is_inside(&model.path, models_dir) {
        bail!(
            "{} is not in the app's models folder ({}): it is your own file, so it is not deleted",
            model.path.display(),
            models_dir.display()
        );
    }
    let mut removed = Vec::new();
    remove_file_if_exists(&model.path, &mut removed)?;
    remove_file_if_exists(&part_path(&model.path), &mut removed)?;
    if let Some(mmproj) = &model.mmproj {
        let shared = model
            .path
            .parent()
            .map(|dir| scan_models(dir).iter().any(|m| m.mmproj.as_deref().is_some_and(|p| same_file(p, mmproj))))
            .unwrap_or(false);
        if !shared && is_inside(mmproj, models_dir) {
            remove_file_if_exists(mmproj, &mut removed)?;
        }
    }
    if let Some(dir) = model.path.parent() {
        // An empty repo folder goes too.
        if is_inside(dir, models_dir) && fs::read_dir(dir).is_ok_and(|mut d| d.next().is_none()) {
            let _ = fs::remove_dir(dir);
        }
    }
    Ok(removed)
}

fn remove_file_if_exists(path: &Path, removed: &mut Vec<PathBuf>) -> anyhow::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => {
            removed.push(path.to_path_buf());
            Ok(())
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(in_use_error(e, path)),
    }
}

// ---------------------------------------------------------------------------------------------
// Visual C++ runtime check

/// DLLs that `exe` and the DLLs next to it import, which are neither next to it nor in the
/// Windows system folder (`api-ms-win-*` API sets are part of Windows and skipped). On a fresh
/// PC that is typically the Visual C++ runtime: `VCRUNTIME140.dll`, `MSVCP140.dll`,
/// `VCOMP140.DLL`. Empty when everything is there or outside Windows.
pub fn missing_system_dlls(exe: &Path) -> Vec<String> {
    let Some(dir) = exe.parent() else { return Vec::new() };
    let system = system_dir();
    let mut files: Vec<PathBuf> = vec![exe.to_path_buf()];
    if let Ok(entries) = fs::read_dir(dir) {
        files.extend(
            entries.flatten().map(|e| e.path()).filter(|p| file_name(p).to_ascii_lowercase().ends_with(".dll")),
        );
    }
    let local: HashSet<String> = files.iter().map(|p| file_name(p).to_ascii_lowercase()).collect();
    let mut missing: Vec<String> = Vec::new();
    for file in &files {
        for import in pe_imports(file).unwrap_or_default() {
            let lower = import.to_ascii_lowercase();
            if lower.starts_with("api-ms-") || lower.starts_with("ext-ms-") || local.contains(&lower) {
                continue;
            }
            let present = system.as_ref().is_none_or(|s| s.join(&import).is_file());
            if !present && !missing.iter().any(|m| m.eq_ignore_ascii_case(&import)) {
                missing.push(import);
            }
        }
    }
    missing.sort_by_key(|m| m.to_ascii_lowercase());
    missing
}

/// The problem text for [`missing_system_dlls`] (`None` when nothing is missing).
pub fn missing_dlls_message(missing: &[String]) -> Option<String> {
    if missing.is_empty() {
        return None;
    }
    let vc = missing.iter().any(|m| {
        let m = m.to_ascii_lowercase();
        m.starts_with("vcruntime") || m.starts_with("msvcp") || m.starts_with("vcomp") || m.starts_with("concrt")
    });
    let list = missing.join(", ");
    Some(if vc {
        format!(
            "llama-server needs the Microsoft Visual C++ runtime, which is not installed ({list} missing) - install \
             it from {VC_REDIST_URL} and start again"
        )
    } else {
        format!("llama-server needs DLLs that are not on this PC: {list}")
    })
}

#[cfg(windows)]
fn system_dir() -> Option<PathBuf> {
    let root = std::env::var_os("SystemRoot").or_else(|| std::env::var_os("windir"))?;
    Some(PathBuf::from(root).join("System32"))
}

#[cfg(not(windows))]
fn system_dir() -> Option<PathBuf> {
    None
}

/// Names of the DLLs a PE file (exe / dll) imports. Reads only the headers and the import table.
pub fn pe_imports(path: &Path) -> io::Result<Vec<String>> {
    let mut file = File::open(path)?;
    let invalid = || io::Error::new(io::ErrorKind::InvalidData, "not a PE file");
    let mut read_at = |offset: u64, len: usize| -> io::Result<Vec<u8>> {
        file.seek(SeekFrom::Start(offset))?;
        let mut buf = vec![0u8; len];
        let n = file.read(&mut buf)?;
        buf.truncate(n);
        Ok(buf)
    };
    let u16_at = |b: &[u8], o: usize| b.get(o..o + 2).map(|s| u16::from_le_bytes([s[0], s[1]]));
    let u32_at = |b: &[u8], o: usize| b.get(o..o + 4).map(|s| u32::from_le_bytes([s[0], s[1], s[2], s[3]]));

    let head = read_at(0, 4096)?;
    if head.get(0..2) != Some(b"MZ") {
        return Err(invalid());
    }
    let pe = u32_at(&head, 0x3c).ok_or_else(invalid)? as usize;
    let hdr = read_at(pe as u64, 1024)?;
    if hdr.get(0..4) != Some(b"PE\0\0") {
        return Err(invalid());
    }
    let sections = u16_at(&hdr, 6).ok_or_else(invalid)? as usize;
    let opt_size = u16_at(&hdr, 20).ok_or_else(invalid)? as usize;
    let opt = 24;
    let magic = u16_at(&hdr, opt).ok_or_else(invalid)?;
    let data_dirs = opt + if magic == 0x20b { 112 } else { 96 };
    let import_rva = u32_at(&hdr, data_dirs + 8).ok_or_else(invalid)?;
    if import_rva == 0 {
        return Ok(Vec::new());
    }
    let table = read_at((pe + opt + opt_size) as u64, sections * 40)?;
    let mut secs = Vec::new();
    for i in 0..sections {
        let o = i * 40;
        let (Some(vsize), Some(va), Some(rsize), Some(raw)) =
            (u32_at(&table, o + 8), u32_at(&table, o + 12), u32_at(&table, o + 16), u32_at(&table, o + 20))
        else {
            break;
        };
        secs.push((va, vsize.max(rsize), raw));
    }
    let to_offset = |rva: u32| {
        secs.iter().find(|(va, size, _)| rva >= *va && rva < va + size).map(|(va, _, raw)| u64::from(rva - va + raw))
    };
    let mut names = Vec::new();
    let mut desc = to_offset(import_rva).ok_or_else(invalid)?;
    for _ in 0..512 {
        let d = read_at(desc, 20)?;
        let name_rva = u32_at(&d, 12).ok_or_else(invalid)?;
        if name_rva == 0 {
            break;
        }
        if let Some(off) = to_offset(name_rva) {
            let raw = read_at(off, 260)?;
            let end = raw.iter().position(|b| *b == 0).unwrap_or(raw.len());
            names.push(String::from_utf8_lossy(&raw[..end]).into_owned());
        }
        desc += 20;
    }
    Ok(names)
}

// ---------------------------------------------------------------------------------------------
// Downloads

/// Error of a download / task that was cancelled (find it with `err.downcast_ref::<Cancelled>()`).
/// A partial download stays as `<file>.part` and resumes next time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cancelled;

impl std::fmt::Display for Cancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("cancelled - the partial download is kept and continues next time")
    }
}

impl std::error::Error for Cancelled {}

/// One file to download.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Download {
    pub url: String,
    pub dest: PathBuf,
    /// Expected size (checked, and used for the free-space check and progress).
    pub size: Option<u64>,
    /// Expected sha256 (hex). The file is only installed when it matches.
    pub sha256: Option<String>,
    /// Bearer token, sent only to the host of `url`.
    pub token: Option<String>,
}

/// `<dest>.part`.
pub fn part_path(dest: &Path) -> PathBuf {
    let mut name = dest.as_os_str().to_owned();
    name.push(".part");
    PathBuf::from(name)
}

/// HTTP agent for downloads (proxy from the environment, redirects followed by hand).
pub fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(20))
        .timeout_read(Duration::from_secs(60))
        .redirects(0)
        .user_agent(USER_AGENT)
        .try_proxy_from_env(true)
        .build()
}

/// Download `d.url` to `d.dest`, resuming `<dest>.part`. `progress(done, total)` is called after
/// every chunk. The file is renamed into place only when complete and (with `d.sha256`) its
/// checksum matches; a mismatch deletes the `.part`. Cancel returns [`Cancelled`] and keeps it.
pub fn download_file(
    agent: &ureq::Agent,
    d: &Download,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(u64, Option<u64>),
) -> anyhow::Result<()> {
    let part = part_path(&d.dest);
    if let Some(dir) = d.dest.parent() {
        fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let mut have = fs::metadata(&part).map(|m| m.len()).unwrap_or(0);
    if d.size.is_some_and(|size| have > size) {
        fs::remove_file(&part).map_err(|e| io_error(e, &part))?;
        have = 0;
    }
    if let Some(size) = d.size {
        check_free_space(&d.dest, size.saturating_sub(have))?;
    }
    let mut hasher = Sha256::new();
    if have > 0 && d.sha256.is_some() {
        // The bytes already there count for the checksum.
        hash_file_into(&part, &mut hasher, cancel, &mut |n| progress(n, d.size))?;
    }

    let mut total = d.size;
    if d.size != Some(have) || have == 0 {
        let mut attempt = 0;
        let response = loop {
            attempt += 1;
            match get_following_redirects(agent, &d.url, d.token.as_deref(), have) {
                Ok(resp) => break resp,
                // The part is complete or broken: start over once.
                Err(GetError::RangeNotSatisfiable) if have > 0 && attempt == 1 => {
                    fs::remove_file(&part).map_err(|e| io_error(e, &part))?;
                    have = 0;
                    hasher = Sha256::new();
                }
                Err(GetError::RangeNotSatisfiable) => bail!("the server refused the download range for {}", d.url),
                Err(GetError::Other(e)) => return Err(e),
            }
        };
        let resumed = response.status() == 206;
        if have > 0 && !resumed {
            // The server sends the whole file again.
            have = 0;
            hasher = Sha256::new();
        }
        if total.is_none() {
            total = if resumed {
                response.header("Content-Range").and_then(|r| r.rsplit('/').next()).and_then(|t| t.trim().parse().ok())
            } else {
                response.header("Content-Length").and_then(|l| l.trim().parse().ok())
            };
        }
        let mut file = if have > 0 {
            OpenOptions::new().append(true).open(&part).map_err(|e| io_error(e, &part))?
        } else {
            File::create(&part).map_err(|e| io_error(e, &part))?
        };
        let mut reader = response.into_reader();
        let mut buf = vec![0u8; CHUNK];
        loop {
            if cancel.load(Ordering::Relaxed) {
                let _ = file.flush();
                return Err(Cancelled.into());
            }
            let n = match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => {
                    let _ = file.flush();
                    bail!(
                        "the download of {} broke off after {} ({e}) - start it again to continue",
                        file_leaf(&d.url),
                        fmt_bytes(have)
                    );
                }
            };
            file.write_all(&buf[..n]).map_err(|e| io_error(e, &part))?;
            hasher.update(&buf[..n]);
            have += n as u64;
            progress(have, total);
        }
        file.sync_all().map_err(|e| io_error(e, &part))?;
    }
    if let Some(total) = total {
        if have != total {
            bail!(
                "the download of {} ended early ({} of {}) - start it again to continue",
                file_leaf(&d.url),
                fmt_bytes(have),
                fmt_bytes(total)
            );
        }
    }
    if let Some(want) = &d.sha256 {
        let got = hex(&hasher.finalize());
        if !got.eq_ignore_ascii_case(want.trim()) {
            let _ = fs::remove_file(&part);
            bail!(
                "checksum mismatch for {}: expected sha256 {want}, got {got} - the download was damaged and was \
                 deleted; download it again",
                file_name(&d.dest)
            );
        }
    }
    fs::rename(&part, &d.dest).map_err(|e| in_use_error(e, &d.dest))?;
    Ok(())
}

enum GetError {
    RangeNotSatisfiable,
    Other(anyhow::Error),
}

/// GET with `Range: bytes=<from>-` (when `from > 0`), following redirects by hand so the range
/// is kept and the token stays with its host.
fn get_following_redirects(
    agent: &ureq::Agent,
    url: &str,
    token: Option<&str>,
    from: u64,
) -> Result<ureq::Response, GetError> {
    let token_host = host_of(url);
    let mut current = url.to_string();
    for _ in 0..=MAX_REDIRECTS {
        let mut req = agent.get(&current);
        if from > 0 {
            req = req.set("Range", &format!("bytes={from}-"));
        }
        if let Some(token) = token.filter(|_| host_of(&current) == token_host) {
            req = req.set("Authorization", &format!("Bearer {token}"));
        }
        match req.call() {
            Ok(resp) if (300..400).contains(&resp.status()) => {
                let Some(location) = resp.header("Location").map(str::to_string) else {
                    return Err(GetError::Other(anyhow::anyhow!("redirect without a location from {current}")));
                };
                current = join_url(&current, &location);
            }
            Ok(resp) => return Ok(resp),
            Err(ureq::Error::Status(416, _)) => return Err(GetError::RangeNotSatisfiable),
            Err(ureq::Error::Status(code, _)) => {
                return Err(GetError::Other(anyhow::anyhow!(status_message(code, url))))
            }
            Err(ureq::Error::Transport(t)) => {
                return Err(GetError::Other(anyhow::anyhow!(
                    "could not reach {}: {t} - check the internet connection",
                    host_of(&current)
                )))
            }
        }
    }
    Err(GetError::Other(anyhow::anyhow!("too many redirects for {url}")))
}

/// What an HTTP error status means for the user.
pub fn status_message(code: u16, url: &str) -> String {
    match code {
        401 | 403 => format!(
            "access denied ({code}) for {url}: the repo may be gated or private - accept its terms on huggingface.co \
             and set a Hugging Face token (Settings > Bonsai server, `bh3 config set runtime.hf_token <TOKEN>` or \
             HF_TOKEN)"
        ),
        404 => format!("not found (404): {url} - check the repo and the file name"),
        429 => format!("too many requests (429) for {url} - wait a minute and try again"),
        500..=599 => format!("the server had an error ({code}) for {url} - try again later"),
        _ => format!("the download failed with HTTP {code}: {url}"),
    }
}

/// `scheme://host[:port]` of a URL (lowercase).
fn host_of(url: &str) -> String {
    let (scheme, rest) = url.split_once("://").unwrap_or(("http", url));
    let host = rest.split(['/', '?', '#']).next().unwrap_or(rest);
    format!("{scheme}://{host}").to_ascii_lowercase()
}

/// A redirect `Location` against the URL that sent it.
fn join_url(base: &str, location: &str) -> String {
    if location.contains("://") {
        location.to_string()
    } else if location.starts_with("//") {
        let scheme = base.split_once("://").map_or("https", |(s, _)| s);
        format!("{scheme}:{location}")
    } else if location.starts_with('/') {
        format!("{}{location}", host_of_original_case(base))
    } else {
        let dir = base.rsplit_once('/').map_or(base, |(d, _)| d);
        format!("{dir}/{location}")
    }
}

fn host_of_original_case(url: &str) -> String {
    let (scheme, rest) = url.split_once("://").unwrap_or(("http", url));
    format!("{scheme}://{}", rest.split(['/', '?', '#']).next().unwrap_or(rest))
}

/// Error when the drive of `dest` has less than `need` (plus a reserve) free.
pub fn check_free_space(dest: &Path, need: u64) -> anyhow::Result<()> {
    if let Some(free) = free_space(dest) {
        if free < need.saturating_add(DISK_RESERVE) {
            bail!(
                "not enough disk space for {}: {} needed, {} free on that drive - free some space or choose another \
                 models folder (Settings > Bonsai server)",
                file_name(dest),
                fmt_bytes(need),
                fmt_bytes(free)
            );
        }
    }
    Ok(())
}

/// An I/O error with the path, and a clear message for a full disk.
fn io_error(e: io::Error, path: &Path) -> anyhow::Error {
    if is_disk_full(&e) {
        anyhow::anyhow!("the disk is full while writing {} - free some space or choose another folder", path.display())
    } else {
        anyhow::anyhow!("{}: {e}", path.display())
    }
}

/// Like [`io_error`], naming a file Bonsai may hold open.
fn in_use_error(e: io::Error, path: &Path) -> anyhow::Error {
    if e.kind() == io::ErrorKind::PermissionDenied || e.raw_os_error() == Some(32) {
        anyhow::anyhow!("{} is in use (is Bonsai running from it?) - stop Bonsai first: {e}", path.display())
    } else {
        io_error(e, path)
    }
}

fn is_disk_full(e: &io::Error) -> bool {
    // ERROR_HANDLE_DISK_FULL (39), ERROR_DISK_FULL (112), ENOSPC (28).
    e.kind() == io::ErrorKind::StorageFull
        || matches!(e.raw_os_error(), Some(39) | Some(112))
        || (!cfg!(windows) && e.raw_os_error() == Some(28))
}

fn hash_file_into(
    path: &Path,
    hasher: &mut Sha256,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(u64),
) -> anyhow::Result<()> {
    let mut file = File::open(path).map_err(|e| io_error(e, path))?;
    let mut buf = vec![0u8; CHUNK * 4];
    let mut done = 0u64;
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err(Cancelled.into());
        }
        let n = file.read(&mut buf).map_err(|e| io_error(e, path))?;
        if n == 0 {
            return Ok(());
        }
        hasher.update(&buf[..n]);
        done += n as u64;
        progress(done);
    }
}

/// sha256 of a file (hex).
pub fn sha256_file(path: &Path, cancel: &AtomicBool) -> anyhow::Result<String> {
    let mut hasher = Sha256::new();
    hash_file_into(path, &mut hasher, cancel, &mut |_| {})?;
    Ok(hex(&hasher.finalize()))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// A file in a Hugging Face repo or a GitHub release.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteFile {
    pub path: String,
    pub size: Option<u64>,
    pub sha256: Option<String>,
}

/// The files of a Hugging Face tree listing (`/api/models/<repo>/tree/main?recursive=true`);
/// `lfs.oid` is the sha256.
pub fn parse_hf_tree(json: &str) -> anyhow::Result<Vec<RemoteFile>> {
    let value: serde_json::Value = serde_json::from_str(json).context("the Hugging Face file list is not JSON")?;
    let items = value.as_array().context("the Hugging Face file list is not a list")?;
    Ok(items
        .iter()
        .filter(|i| i.get("type").and_then(|t| t.as_str()) == Some("file"))
        .filter_map(|i| {
            Some(RemoteFile {
                path: i.get("path")?.as_str()?.to_string(),
                size: i.get("size").and_then(|s| s.as_u64()),
                sha256: i.pointer("/lfs/oid").and_then(|s| s.as_str()).map(str::to_string),
            })
        })
        .collect())
}

/// The assets of a GitHub release (`assets[].name/size/digest`, digest `sha256:<hex>`).
pub fn parse_release_assets(json: &str) -> anyhow::Result<Vec<RemoteFile>> {
    let value: serde_json::Value = serde_json::from_str(json).context("the GitHub release is not JSON")?;
    let assets = value.get("assets").and_then(|a| a.as_array()).context("the GitHub release lists no assets")?;
    Ok(assets
        .iter()
        .filter_map(|a| {
            Some(RemoteFile {
                path: a.get("name")?.as_str()?.to_string(),
                size: a.get("size").and_then(|s| s.as_u64()),
                sha256: a
                    .get("digest")
                    .and_then(|d| d.as_str())
                    .and_then(|d| d.strip_prefix("sha256:"))
                    .map(str::to_string),
            })
        })
        .collect())
}

/// GET a small JSON document, `tries` times (the Hugging Face API sometimes times out).
fn get_json_text(url: &str, token: Option<&str>, tries: usize) -> anyhow::Result<String> {
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(15))
        .timeout(Duration::from_secs(30))
        .user_agent(USER_AGENT)
        .try_proxy_from_env(true)
        .build();
    let mut last = None;
    for attempt in 0..tries.max(1) {
        if attempt > 0 {
            std::thread::sleep(Duration::from_secs(2 * attempt as u64));
        }
        let mut req = agent.get(url).set("Accept", "application/json");
        if let Some(token) = token {
            req = req.set("Authorization", &format!("Bearer {token}"));
        }
        match req.call() {
            Ok(resp) => return resp.into_string().with_context(|| format!("reading {url}")),
            Err(ureq::Error::Status(code, _)) if code != 429 && code < 500 => bail!(status_message(code, url)),
            Err(e) => last = Some(e.to_string()),
        }
    }
    bail!("could not load {url}: {}", last.unwrap_or_default())
}

// ---------------------------------------------------------------------------------------------
// Tasks

/// Copy or move files out of a Bonsai-demo folder.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum ImportMode {
    #[default]
    Copy,
    Move,
}

/// A download or import job.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum RuntimeTask {
    /// A model (and its vision projector) from Hugging Face into the models folder.
    DownloadModel(ModelSource),
    /// The llama-server build for a backend from GitHub into `<runtime>\bin`.
    DownloadServer(Backend),
    /// The server build, model and projector of a Bonsai-demo folder into the app's storage.
    Import { dir: PathBuf, mode: ImportMode },
}

impl RuntimeTask {
    /// Short description (`Download Ternary-Bonsai-2-27B-PTQ1_0.gguf`).
    pub fn title(&self) -> String {
        match self {
            RuntimeTask::DownloadModel(src) => format!("Download {}", file_leaf(&src.file)),
            RuntimeTask::DownloadServer(backend) => format!("Download the {} server build", backend.label()),
            RuntimeTask::Import { dir, mode: ImportMode::Copy } => format!("Copy from {}", dir.display()),
            RuntimeTask::Import { dir, mode: ImportMode::Move } => format!("Move from {}", dir.display()),
        }
    }

    /// Files and folders the task writes (or, when moving, takes away): Bonsai must not start
    /// from them while it runs.
    pub fn targets(&self, cfg: &AppConfig) -> Vec<PathBuf> {
        let models = models_dir(cfg);
        match self {
            RuntimeTask::DownloadModel(src) => {
                std::iter::once(src.model_path(&models)).chain(src.mmproj_path(&models)).collect()
            }
            RuntimeTask::DownloadServer(backend) => vec![build_dir(cfg, RELEASE_TAG, *backend)],
            RuntimeTask::Import { dir, mode } => match plan_import(dir, cfg) {
                Ok(plan) => plan
                    .items
                    .iter()
                    .flat_map(|item| {
                        let src = (*mode == ImportMode::Move).then(|| item.src.clone());
                        std::iter::once(item.dest.clone()).chain(src)
                    })
                    .collect(),
                Err(_) => Vec::new(),
            },
        }
    }
}

/// True when Bonsai's files (`files`: server exe, model, projector) are among `targets` or in a
/// target folder.
pub fn touches(targets: &[PathBuf], files: &[&Path]) -> bool {
    files.iter().any(|f| targets.iter().any(|t| same_file(f, t) || is_inside(f, t)))
}

/// Progress of a task.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct TaskProgress {
    /// What happens now (`Downloading`, `Checking`, `Extracting`, `Copying`, `Moving`).
    pub phase: String,
    /// The file being worked on.
    pub file: String,
    /// 1-based file number and the number of files.
    pub file_index: usize,
    pub files: usize,
    /// Bytes done of all files / all bytes (0 = unknown).
    pub done: u64,
    pub total: u64,
    /// Average speed over the last seconds (bytes/s).
    pub bytes_per_sec: f64,
    pub eta_secs: Option<u64>,
}

impl TaskProgress {
    /// 0..=1 (0 while the total is unknown).
    pub fn fraction(&self) -> f32 {
        if self.total == 0 {
            0.0
        } else {
            (self.done as f64 / self.total as f64).clamp(0.0, 1.0) as f32
        }
    }

    /// `2.4 GB of 5.5 GB · 38 MB/s · 1m 20s left`.
    pub fn line(&self) -> String {
        let mut parts = vec![if self.total > 0 {
            format!("{} of {}", fmt_bytes(self.done), fmt_bytes(self.total))
        } else {
            fmt_bytes(self.done)
        }];
        if self.bytes_per_sec > 0.0 {
            parts.push(format!("{:.1} MB/s", self.bytes_per_sec / (1024.0 * 1024.0)));
        }
        if let Some(eta) = self.eta_secs {
            parts.push(format!("{} left", fmt_duration(eta)));
        }
        parts.join(" · ")
    }
}

/// `52s`, `4m 05s`, `1h 02m`.
pub fn fmt_duration(secs: u64) -> String {
    if secs >= 3600 {
        format!("{}h {:02}m", secs / 3600, (secs % 3600) / 60)
    } else if secs >= 60 {
        format!("{}m {:02}s", secs / 60, secs % 60)
    } else {
        format!("{secs}s")
    }
}

/// What a finished task produced.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct TaskOutcome {
    pub server_exe: Option<PathBuf>,
    pub model: Option<PathBuf>,
    pub mmproj: Option<PathBuf>,
    /// Notes for the user (e.g. "checksum not verified").
    pub notes: Vec<String>,
    /// One line for a toast / the CLI.
    pub message: String,
}

/// Speed over a sliding window.
#[derive(Debug, Default)]
pub struct RateMeter {
    samples: std::collections::VecDeque<(Instant, u64)>,
}

impl RateMeter {
    /// Add a sample (`done` bytes at `now`); returns bytes/s over the window.
    pub fn sample(&mut self, now: Instant, done: u64) -> f64 {
        self.samples.push_back((now, done));
        while self.samples.len() > 2 && self.samples.front().is_some_and(|(t, _)| now.duration_since(*t) > RATE_WINDOW)
        {
            self.samples.pop_front();
        }
        let Some(&(t0, d0)) = self.samples.front() else { return 0.0 };
        let secs = now.duration_since(t0).as_secs_f64();
        if secs < 0.5 || done < d0 {
            return 0.0;
        }
        (done - d0) as f64 / secs
    }

    /// Forget the samples (a new phase: hashing is not download speed).
    pub fn reset(&mut self) {
        self.samples.clear();
    }
}

/// Where downloads go and come from (the real services by default; tests use a local server).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoints {
    pub hf: String,
    pub release_download: String,
    pub release_api: String,
}

impl Default for Endpoints {
    fn default() -> Self {
        Self {
            hf: HF_BASE_URL.to_string(),
            release_download: RELEASE_DOWNLOAD_URL.to_string(),
            release_api: RELEASE_API_URL.to_string(),
        }
    }
}

/// Reports progress of a task (throttled) with speed and ETA.
struct Reporter<'a> {
    callback: &'a mut dyn FnMut(&TaskProgress),
    state: TaskProgress,
    /// Bytes of the files before the current one.
    base: u64,
    meter: RateMeter,
    last: Option<Instant>,
}

impl<'a> Reporter<'a> {
    fn new(callback: &'a mut dyn FnMut(&TaskProgress), files: usize, total: u64) -> Self {
        let state = TaskProgress { files, total, ..TaskProgress::default() };
        Self { callback, state, base: 0, meter: RateMeter::default(), last: None }
    }

    /// A new file (or phase); always reported.
    fn begin(&mut self, phase: &str, file: &str, index: usize) {
        self.state.phase = phase.to_string();
        self.state.file = file.to_string();
        self.state.file_index = index;
        self.base = self.state.done;
        self.meter.reset();
        self.last = None;
        self.update(0);
    }

    /// `done` bytes of the current file.
    fn update(&mut self, done: u64) {
        let now = Instant::now();
        self.state.done = self.base + done;
        if self.state.done > self.state.total && self.state.total > 0 {
            self.state.total = self.state.done;
        }
        let rate = self.meter.sample(now, self.state.done);
        if self.last.is_some_and(|t| now.duration_since(t) < PROGRESS_EVERY) {
            return;
        }
        self.last = Some(now);
        self.state.bytes_per_sec = rate;
        self.state.eta_secs = (rate > 1.0 && self.state.total > 0)
            .then(|| (self.state.total.saturating_sub(self.state.done) as f64 / rate).ceil() as u64);
        (self.callback)(&self.state);
    }

    /// The current file is done: count it whole (a resumed part was counted from its size).
    fn end(&mut self, bytes: u64) {
        self.base += bytes;
        self.state.done = self.base;
        self.last = None;
        self.update(0);
    }
}

/// Run a task with the real endpoints. See [`run_task_with`].
pub fn run_task(
    task: &RuntimeTask,
    cfg: &AppConfig,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(&TaskProgress),
) -> anyhow::Result<TaskOutcome> {
    run_task_with(task, cfg, &Endpoints::default(), cancel, progress)
}

/// Run a task: downloads resume their `.part` files; a cancel returns [`Cancelled`].
pub fn run_task_with(
    task: &RuntimeTask,
    cfg: &AppConfig,
    endpoints: &Endpoints,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(&TaskProgress),
) -> anyhow::Result<TaskOutcome> {
    match task {
        RuntimeTask::DownloadModel(src) => download_model(src, cfg, endpoints, cancel, progress),
        RuntimeTask::DownloadServer(backend) => download_server(*backend, cfg, endpoints, cancel, progress),
        RuntimeTask::Import { dir, mode } => import(dir, *mode, cfg, cancel, progress),
    }
}

fn download_model(
    src: &ModelSource,
    cfg: &AppConfig,
    endpoints: &Endpoints,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(&TaskProgress),
) -> anyhow::Result<TaskOutcome> {
    let models = models_dir(cfg);
    let token = hf_token(cfg);
    let mut wanted: Vec<(&str, PathBuf)> = vec![(src.file.as_str(), src.model_path(&models))];
    if let (Some(mmproj), Some(dest)) = (&src.mmproj, src.mmproj_path(&models)) {
        wanted.push((mmproj.as_str(), dest));
    }
    let mut notes = Vec::new();
    // Sizes and checksums: the catalog, else the repo's file list.
    let known: Vec<Option<CatalogFile>> = wanted.iter().map(|(file, _)| catalog_file(&src.repo, file)).collect();
    let mut listed: Vec<RemoteFile> = Vec::new();
    if known.iter().any(Option::is_none) {
        let url = format!("{}/api/models/{}/tree/main?recursive=true", endpoints.hf, src.repo);
        match get_json_text(&url, token.as_deref(), TREE_TRIES).and_then(|t| parse_hf_tree(&t)) {
            Ok(files) => listed = files,
            Err(e) => {
                notes.push(format!("The file list of {} did not load ({e:#}): checksums are not verified", src.repo))
            }
        }
        for (i, (file, _)) in wanted.iter().enumerate() {
            if known[i].is_none() && !listed.is_empty() && !listed.iter().any(|f| f.path.eq_ignore_ascii_case(file)) {
                bail!(
                    "{} has no file {file} - check the name (`bh3 models download` takes hf:<owner>/<repo>/<file>)",
                    src.repo
                );
            }
        }
    }
    let specs: Vec<Download> = wanted
        .iter()
        .enumerate()
        .map(|(i, (file, dest))| {
            let listed = listed.iter().find(|f| f.path.eq_ignore_ascii_case(file));
            Download {
                url: format!("{}/{}/resolve/main/{}", endpoints.hf, src.repo, file),
                dest: dest.clone(),
                size: known[i].map(|k| k.bytes).or_else(|| listed.and_then(|f| f.size)),
                sha256: known[i].map(|k| k.sha256.to_string()).or_else(|| listed.and_then(|f| f.sha256.clone())),
                token: token.clone(),
            }
        })
        .collect();
    let total: u64 = specs.iter().filter(|s| !s.dest.is_file()).filter_map(|s| s.size).sum();
    check_free_space(&models.join("x"), total.saturating_sub(specs.iter().map(|s| part_len(&s.dest)).sum()))?;
    let agent = agent();
    let mut reporter = Reporter::new(progress, specs.len(), total);
    for (i, spec) in specs.iter().enumerate() {
        let name = file_name(&spec.dest);
        if spec.dest.is_file() && spec.size.is_none_or(|s| fs::metadata(&spec.dest).is_ok_and(|m| m.len() == s)) {
            notes.push(format!("{name} was already downloaded"));
            continue;
        }
        if spec.sha256.is_none() && !notes.iter().any(|n| n.contains("not verified")) {
            notes.push(format!("{name}: no checksum known, the file is not verified"));
        }
        reporter.begin("Downloading", &name, i + 1);
        download_file(&agent, spec, cancel, &mut |done, _| reporter.update(done))?;
        reporter.end(spec.size.unwrap_or_else(|| fs::metadata(&spec.dest).map(|m| m.len()).unwrap_or(0)));
    }
    let model = src.model_path(&models);
    let mmproj = src.mmproj_path(&models);
    Ok(TaskOutcome {
        message: format!(
            "{} is ready ({})",
            file_name(&model),
            model.parent().map(|p| p.display().to_string()).unwrap_or_default()
        ),
        server_exe: None,
        model: Some(model),
        mmproj,
        notes,
    })
}

fn part_len(dest: &Path) -> u64 {
    fs::metadata(part_path(dest)).map(|m| m.len()).unwrap_or(0)
}

fn download_server(
    backend: Backend,
    cfg: &AppConfig,
    endpoints: &Endpoints,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(&TaskProgress),
) -> anyhow::Result<TaskOutcome> {
    let tag = RELEASE_TAG;
    let assets = backend.assets(tag);
    let mut notes = Vec::new();
    let mut infos: Vec<Option<CatalogFile>> = assets.iter().map(|a| known_asset(a)).collect();
    let mut listed = Vec::new();
    if infos.iter().any(Option::is_none) {
        match get_json_text(&format!("{}/{tag}", endpoints.release_api), None, 2).and_then(|t| parse_release_assets(&t))
        {
            Ok(files) => listed = files,
            Err(e) => notes.push(format!("The release's asset list did not load ({e:#}): checksums are not verified")),
        }
        for (i, info) in infos.iter_mut().enumerate() {
            if info.is_none() && !listed.is_empty() && !listed.iter().any(|f| f.path == assets[i]) {
                bail!("the release {tag} has no {}", assets[i]);
            }
        }
    }
    let downloads = downloads_dir(cfg);
    let specs: Vec<Download> = assets
        .iter()
        .enumerate()
        .map(|(i, asset)| {
            let listed = listed.iter().find(|f| f.path == *asset);
            Download {
                url: format!("{}/{tag}/{asset}", endpoints.release_download),
                dest: downloads.join(asset),
                size: infos[i].map(|k| k.bytes).or_else(|| listed.and_then(|f| f.size)),
                sha256: infos[i].map(|k| k.sha256.to_string()).or_else(|| listed.and_then(|f| f.sha256.clone())),
                token: None,
            }
        })
        .collect();
    let total: u64 = specs.iter().filter_map(|s| s.size).sum();
    // The zips and the extracted files (about as big) at once.
    check_free_space(&downloads.join("x"), (total * 2).saturating_sub(specs.iter().map(|s| part_len(&s.dest)).sum()))?;
    let agent = agent();
    let mut reporter = Reporter::new(progress, specs.len(), total);
    for (i, spec) in specs.iter().enumerate() {
        reporter.begin("Downloading", &file_name(&spec.dest), i + 1);
        let complete =
            spec.dest.is_file() && spec.size.is_some_and(|s| fs::metadata(&spec.dest).is_ok_and(|m| m.len() == s));
        if !complete {
            download_file(&agent, spec, cancel, &mut |done, _| reporter.update(done))?;
        }
        reporter.end(spec.size.unwrap_or_else(|| fs::metadata(&spec.dest).map(|m| m.len()).unwrap_or(0)));
    }
    if cancel.load(Ordering::Relaxed) {
        return Err(Cancelled.into());
    }
    let final_dir = build_dir(cfg, tag, backend);
    let staging = bin_dir(cfg).join(format!(".{tag}-{}.new", backend.id()));
    let _ = fs::remove_dir_all(&staging);
    for (i, spec) in specs.iter().enumerate() {
        reporter.begin("Extracting", &file_name(&spec.dest), i + 1);
        extract_server_zip(&spec.dest, &staging)?;
    }
    if !staging.join(SERVER_EXE).is_file() {
        let _ = fs::remove_dir_all(&staging);
        bail!("the downloaded build has no {SERVER_EXE}");
    }
    let info = BuildInfo { tag: tag.to_string(), backend: backend.id().to_string(), source: assets.clone() };
    write_build_info(&staging, &info)?;
    replace_dir(&staging, &final_dir)?;
    for spec in &specs {
        let _ = fs::remove_file(&spec.dest);
    }
    let exe = final_dir.join(SERVER_EXE);
    if let Some(message) = missing_dlls_message(&missing_system_dlls(&exe)) {
        notes.push(message);
    }
    Ok(TaskOutcome {
        message: format!("The {} server build is ready ({})", backend.label(), final_dir.display()),
        server_exe: Some(exe),
        model: None,
        mmproj: None,
        notes,
    })
}

fn write_build_info(dir: &Path, info: &BuildInfo) -> anyhow::Result<()> {
    let path = dir.join(BUILD_INFO_FILE);
    fs::write(&path, serde_json::to_string_pretty(info)?).map_err(|e| io_error(e, &path))
}

/// Put `staging` where `dest` is (an old `dest` is removed first; fails while Bonsai runs from it).
fn replace_dir(staging: &Path, dest: &Path) -> anyhow::Result<()> {
    if dest.exists() {
        let old = dest.with_extension("old");
        let _ = fs::remove_dir_all(&old);
        fs::rename(dest, &old).map_err(|e| in_use_error(e, dest))?;
        let _ = fs::remove_dir_all(&old);
    }
    fs::rename(staging, dest).map_err(|e| io_error(e, dest))
}

/// What an import copies or moves.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ImportPlan {
    pub items: Vec<ImportItem>,
    /// The server build's folder in the app (`bin\<tag>-<backend>`), when the folder has one.
    pub server_dir: Option<PathBuf>,
    pub model: Option<PathBuf>,
    pub mmproj: Option<PathBuf>,
    pub bytes: u64,
}

/// One file of an import.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ImportItem {
    pub src: PathBuf,
    pub dest: PathBuf,
    pub bytes: u64,
}

/// What [`RuntimeTask::Import`] would do with the Bonsai-demo folder `dir` (read-only): its
/// llama-server build (`bin\<backend>`, release from `.llama_release`), the model the launcher
/// would pick and its projector, and `scripts\webui-config.json`.
pub fn plan_import(dir: &Path, cfg: &AppConfig) -> anyhow::Result<ImportPlan> {
    let install = crate::server::BonsaiInstall::detect(dir)?;
    let mut items = Vec::new();
    let mut server_dir = None;
    if let Some(exe) = &install.server_exe {
        let src_dir = exe.parent().context("llama-server.exe has no folder")?;
        let stamp = fs::read_to_string(src_dir.join(".llama_release")).unwrap_or_default();
        let mut words = stamp.split_whitespace();
        let tag = words.next().unwrap_or("imported").to_string();
        let kind = build_kind(exe).unwrap_or("local");
        let backend = words.next().map(str::to_string).unwrap_or_else(|| kind.to_string());
        let dest_dir = bin_dir(cfg).join(format!("{tag}-{backend}").replace(['\\', '/', ':'], "_"));
        let mut files: Vec<PathBuf> = fs::read_dir(src_dir)
            .with_context(|| format!("reading {}", src_dir.display()))?
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_file() && keep_server_file(&file_name(p)))
            .collect();
        files.sort();
        for src in files {
            let dest = dest_dir.join(file_name(&src));
            items.push(ImportItem { bytes: file_len(&src), src, dest });
        }
        server_dir = Some(dest_dir);
    }
    let models = models_dir(cfg);
    let place = |file: &Path| -> PathBuf {
        let name = file_name(file);
        let sub = catalog_by_file(&name)
            .map(|m| repo_dir_name(m.repo))
            .or_else(|| {
                CATALOG
                    .iter()
                    .find(|m| m.mmproj.is_some_and(|p| p.name.eq_ignore_ascii_case(&name)))
                    .filter(|_| install.primary_model().is_some_and(|pm| catalog_by_file(&file_name(pm)).is_some()))
                    .map(|m| repo_dir_name(m.repo))
            })
            .unwrap_or_else(|| LOCAL_MODELS_DIR.to_string());
        models.join(sub).join(name)
    };
    let model = install.primary_model().cloned();
    let mut model_dest = None;
    let mut mmproj_dest = None;
    if let Some(src) = &model {
        let dest = place(src);
        items.push(ImportItem { bytes: file_len(src), src: src.clone(), dest: dest.clone() });
        model_dest = Some(dest);
    }
    if let Some(src) = &install.mmproj {
        let dest = model_dest.as_ref().and_then(|m| m.parent()).map_or_else(|| place(src), |d| d.join(file_name(src)));
        items.push(ImportItem { bytes: file_len(src), src: src.clone(), dest: dest.clone() });
        mmproj_dest = Some(dest);
    }
    let webui = install.root.join("scripts").join(WEBUI_CONFIG_FILE);
    if webui.is_file() {
        items.push(ImportItem { bytes: file_len(&webui), src: webui, dest: runtime_dir(cfg).join(WEBUI_CONFIG_FILE) });
    }
    if model.is_none() && server_dir.is_none() {
        bail!("{} has neither a llama-server build nor a model to import", install.root.display());
    }
    let bytes = items.iter().map(|i| i.bytes).sum();
    Ok(ImportPlan { items, server_dir, model: model_dest, mmproj: mmproj_dest, bytes })
}

fn import(
    dir: &Path,
    mode: ImportMode,
    cfg: &AppConfig,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(&TaskProgress),
) -> anyhow::Result<TaskOutcome> {
    let plan = plan_import(dir, cfg)?;
    let todo: Vec<&ImportItem> =
        plan.items.iter().filter(|i| !(i.dest.is_file() && file_len(&i.dest) == i.bytes)).collect();
    let copy_bytes: u64 = todo.iter().map(|i| i.bytes).sum();
    let mut reporter = Reporter::new(progress, todo.len(), copy_bytes);
    // Moved by rename (undone on failure) / copied (sources deleted at the end when moving).
    let mut renamed: Vec<&ImportItem> = Vec::new();
    let mut copied: Vec<&ImportItem> = Vec::new();
    let result = (|| -> anyhow::Result<()> {
        for (i, item) in todo.iter().enumerate() {
            if let Some(parent) = item.dest.parent() {
                fs::create_dir_all(parent).map_err(|e| io_error(e, parent))?;
            }
            let name = file_name(&item.src);
            if mode == ImportMode::Move && fs::rename(&item.src, &item.dest).is_ok() {
                reporter.begin("Moving", &name, i + 1);
                renamed.push(item);
                reporter.end(item.bytes);
                continue;
            }
            check_free_space(&item.dest, item.bytes)?;
            reporter.begin(if mode == ImportMode::Move { "Moving" } else { "Copying" }, &name, i + 1);
            copy_file(&item.src, &item.dest, cancel, &mut |done| reporter.update(done))?;
            copied.push(item);
            reporter.end(item.bytes);
        }
        Ok(())
    })();
    if let Err(e) = result {
        for item in renamed.iter().rev() {
            let _ = fs::rename(&item.dest, &item.src);
        }
        return Err(e);
    }
    let mut notes = Vec::new();
    if let Some(server_dir) = &plan.server_dir {
        if !server_dir.join(BUILD_INFO_FILE).is_file() {
            let name = file_name(server_dir);
            let backend = Backend::ALL.into_iter().find(|b| name.ends_with(b.id())).map_or("?", Backend::id);
            let tag = name.trim_end_matches(backend).trim_end_matches('-').to_string();
            let info = BuildInfo {
                tag,
                backend: backend.to_string(),
                source: vec![format!("imported from {}", dir.display())],
            };
            write_build_info(server_dir, &info)?;
        }
    }
    if mode == ImportMode::Move {
        for item in &copied {
            if let Err(e) = fs::remove_file(&item.src) {
                notes.push(format!("copied, but {} could not be removed: {e}", item.src.display()));
            }
        }
    }
    let skipped = plan.items.len() - todo.len();
    if skipped > 0 {
        notes.push(format!("{skipped} file(s) were already in the app and were left as they are"));
    }
    let verb = if mode == ImportMode::Move { "Moved" } else { "Copied" };
    Ok(TaskOutcome {
        message: format!(
            "{verb} {} from {} into the app ({})",
            fmt_bytes(copy_bytes),
            dir.display(),
            runtime_dir(cfg).display()
        ),
        server_exe: plan.server_dir.map(|d| d.join(SERVER_EXE)),
        model: plan.model,
        mmproj: plan.mmproj,
        notes,
    })
}

/// Copy `src` to `dest` through `<dest>.part` (cancel deletes the part: a copy restarts quickly).
fn copy_file(src: &Path, dest: &Path, cancel: &AtomicBool, progress: &mut dyn FnMut(u64)) -> anyhow::Result<()> {
    let part = part_path(dest);
    let result = (|| -> anyhow::Result<()> {
        let mut input = File::open(src).map_err(|e| io_error(e, src))?;
        let mut output = File::create(&part).map_err(|e| io_error(e, &part))?;
        let mut buf = vec![0u8; CHUNK * 4];
        let mut done = 0u64;
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(Cancelled.into());
            }
            let n = input.read(&mut buf).map_err(|e| io_error(e, src))?;
            if n == 0 {
                break;
            }
            output.write_all(&buf[..n]).map_err(|e| io_error(e, &part))?;
            done += n as u64;
            progress(done);
        }
        output.sync_all().map_err(|e| io_error(e, &part))?;
        Ok(())
    })();
    if let Err(e) = result {
        let _ = fs::remove_file(&part);
        return Err(e);
    }
    fs::rename(&part, dest).map_err(|e| in_use_error(e, dest))
}

// ---------------------------------------------------------------------------------------------
// Launch flags

/// Sampling flags `start_llama_server.ps1` passes for a Bonsai 2 model (server defaults; the
/// app sends its own per request): the 27B gets the model card's values, the small models
/// (2B / 4B / 9B) the small-model profile with thinking on.
pub fn launcher_sampling(model: &Path) -> Vec<&'static str> {
    let name = file_name(model).to_ascii_lowercase();
    let small = ["-2b-", "-4b-", "-9b-"].iter().any(|s| name.contains(s));
    if small {
        vec![
            "--temp",
            "1.0",
            "--top-p",
            "0.95",
            "--top-k",
            "20",
            "--min-p",
            "0",
            "--presence-penalty",
            "1.0",
            "--repeat-penalty",
            "1.0",
            "--repeat-last-n",
            "64",
        ]
    } else {
        vec!["--temp", "1.0", "--top-p", "0.95", "--top-k", "20", "--min-p", "0.05"]
    }
}

/// Split extra server arguments like a command line: spaces separate, double quotes group.
pub fn split_args(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    let mut any = false;
    for c in text.chars() {
        match c {
            '"' => {
                quoted = !quoted;
                any = true;
            }
            c if c.is_whitespace() && !quoted => {
                if any {
                    out.push(std::mem::take(&mut cur));
                    any = false;
                }
            }
            c => {
                cur.push(c);
                any = true;
            }
        }
    }
    if any {
        out.push(cur);
    }
    out
}

// ---------------------------------------------------------------------------------------------
// Small helpers

fn file_name(path: &Path) -> String {
    path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}

fn file_len(path: &Path) -> u64 {
    fs::metadata(path).map(|m| m.len()).unwrap_or(0)
}

fn collect_files(dir: &Path, depth: usize, max_depth: usize, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with('.') {
            continue;
        }
        if path.is_dir() {
            if depth < max_depth {
                collect_files(&path, depth + 1, max_depth, out);
            }
        } else if path.is_file() {
            out.push(path);
        }
    }
}

/// Comparison key (separators unified, lowercase on Windows).
fn path_key(path: &Path) -> String {
    let s = path.to_string_lossy().replace('/', "\\");
    let s = s.trim_end_matches('\\').trim_start_matches(r"\\?\");
    if cfg!(windows) {
        s.to_lowercase()
    } else {
        s.to_string()
    }
}

/// The same path (any case on Windows).
pub fn same_file(a: &Path, b: &Path) -> bool {
    path_key(a) == path_key(b)
}

/// `path` lies inside the folder `dir`.
pub fn is_inside(path: &Path, dir: &Path) -> bool {
    let (p, d) = (path_key(path), path_key(dir));
    !d.is_empty() && p.len() > d.len() && p.starts_with(&d) && p[d.len()..].starts_with('\\')
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("bh3-runtime-test-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn sample_bytes(len: usize) -> Vec<u8> {
        (0..len).map(|i| ((i * 31 + i / 7) % 251) as u8).collect()
    }

    /// One route of the mock server.
    #[derive(Clone)]
    struct Route {
        status: u16,
        body: Vec<u8>,
        /// Answer `Range` with 206.
        ranges: bool,
        /// Close the connection after this many body bytes (a broken download).
        cut_after: Option<usize>,
        /// 302 to this path.
        redirect: Option<String>,
    }

    impl Route {
        fn file(body: Vec<u8>) -> Self {
            Self { status: 200, body, ranges: true, cut_after: None, redirect: None }
        }
    }

    /// (path, Range header, Authorization header) of one request.
    type Seen = Arc<Mutex<Vec<(String, Option<String>, Option<String>)>>>;

    /// A tiny HTTP/1.1 server on a free loopback port (never the user's 8080).
    struct MockServer {
        port: u16,
        routes: Arc<Mutex<HashMap<String, Route>>>,
        /// (path, Range header, Authorization header) of every request.
        seen: Seen,
    }

    impl MockServer {
        fn start() -> Self {
            let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
            let port = listener.local_addr().unwrap().port();
            let routes: Arc<Mutex<HashMap<String, Route>>> = Arc::default();
            let seen: Seen = Arc::default();
            let (r, s) = (routes.clone(), seen.clone());
            std::thread::spawn(move || {
                for stream in listener.incoming() {
                    let Ok(mut stream) = stream else { continue };
                    let (r, s) = (r.clone(), s.clone());
                    std::thread::spawn(move || {
                        let mut buf = Vec::new();
                        let mut byte = [0u8; 1];
                        while !buf.ends_with(b"\r\n\r\n") {
                            match stream.read(&mut byte) {
                                Ok(1) => buf.push(byte[0]),
                                _ => return,
                            }
                        }
                        let text = String::from_utf8_lossy(&buf).to_string();
                        let path = text.split_whitespace().nth(1).unwrap_or("/").to_string();
                        let header = |name: &str| {
                            text.lines().find_map(|l| {
                                let (k, v) = l.split_once(':')?;
                                k.trim().eq_ignore_ascii_case(name).then(|| v.trim().to_string())
                            })
                        };
                        let range = header("Range");
                        s.lock().unwrap().push((path.clone(), range.clone(), header("Authorization")));
                        let route = r.lock().unwrap().get(&path).cloned();
                        let Some(route) = route else {
                            let _ = stream
                                .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                            return;
                        };
                        if let Some(to) = route.redirect {
                            let reply = format!("HTTP/1.1 302 Found\r\nLocation: {to}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                            let _ = stream.write_all(reply.as_bytes());
                            return;
                        }
                        if route.status != 200 {
                            let reply = format!(
                                "HTTP/1.1 {} X\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                                route.status
                            );
                            let _ = stream.write_all(reply.as_bytes());
                            return;
                        }
                        let from = range.filter(|_| route.ranges).and_then(|r| {
                            r.strip_prefix("bytes=").and_then(|r| r.trim_end_matches('-').parse::<usize>().ok())
                        });
                        let len = route.body.len();
                        let (head, body) = match from {
                            Some(from) if from >= len => {
                                let _ = stream.write_all(b"HTTP/1.1 416 Range Not Satisfiable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                                return;
                            }
                            Some(from) => (
                                format!(
                                    "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {from}-{}/{len}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                                    len - 1,
                                    len - from
                                ),
                                &route.body[from..],
                            ),
                            None => (format!("HTTP/1.1 200 OK\r\nContent-Length: {len}\r\nConnection: close\r\n\r\n"), &route.body[..]),
                        };
                        let _ = stream.write_all(head.as_bytes());
                        let body = match route.cut_after {
                            Some(n) => &body[..n.min(body.len())],
                            None => body,
                        };
                        let _ = stream.write_all(body);
                    });
                }
            });
            Self { port, routes, seen }
        }

        fn url(&self, path: &str) -> String {
            format!("http://127.0.0.1:{}{path}", self.port)
        }

        fn route(&self, path: &str, route: Route) {
            self.routes.lock().unwrap().insert(path.to_string(), route);
        }
    }

    fn sha_of(bytes: &[u8]) -> String {
        hex(&Sha256::digest(bytes))
    }

    #[test]
    fn download_resumes_with_range_and_checks_sha() {
        let server = MockServer::start();
        let body = sample_bytes(3 * CHUNK + 12345);
        let dir = temp_dir("resume");
        let dest = dir.join("model.gguf");
        let d = Download {
            url: server.url("/start"),
            dest: dest.clone(),
            size: Some(body.len() as u64),
            sha256: Some(sha_of(&body)),
            token: Some("secret".into()),
        };
        // A redirect to the file; the first answer breaks off after 1.5 MB.
        server.route("/start", Route { redirect: Some("/file".into()), ..Route::file(Vec::new()) });
        server.route("/file", Route { cut_after: Some(CHUNK + CHUNK / 2), ..Route::file(body.clone()) });
        let cancel = AtomicBool::new(false);
        let err = download_file(&agent(), &d, &cancel, &mut |_, _| {}).unwrap_err().to_string();
        assert!(err.contains("start it again to continue") || err.contains("broke off"), "{err}");
        let part = part_path(&dest);
        assert_eq!(fs::metadata(&part).unwrap().len(), (CHUNK + CHUNK / 2) as u64, "the part is kept");
        assert!(!dest.exists());

        // Resume: the rest comes with 206.
        server.route("/file", Route::file(body.clone()));
        let mut last = (0, None);
        download_file(&agent(), &d, &cancel, &mut |done, total| last = (done, total)).unwrap();
        assert_eq!(fs::read(&dest).unwrap(), body);
        assert!(!part.exists());
        assert_eq!(last, (body.len() as u64, Some(body.len() as u64)));
        let seen = server.seen.lock().unwrap().clone();
        let resumed = seen.iter().rfind(|(p, _, _)| p == "/file").unwrap();
        assert_eq!(resumed.1.as_deref(), Some(format!("bytes={}-", CHUNK + CHUNK / 2).as_str()), "{seen:?}");
        // The redirect kept the host, so the token went along; the range survived the redirect.
        assert!(seen.iter().all(|(_, _, auth)| auth.as_deref() == Some("Bearer secret")), "{seen:?}");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn download_with_wrong_sha_is_not_installed() {
        let server = MockServer::start();
        let body = sample_bytes(200_000);
        server.route("/f", Route::file(body.clone()));
        let dir = temp_dir("sha");
        let dest = dir.join("x.gguf");
        let d = Download {
            url: server.url("/f"),
            dest: dest.clone(),
            size: None,
            sha256: Some("00".repeat(32)),
            token: None,
        };
        let err = download_file(&agent(), &d, &AtomicBool::new(false), &mut |_, _| {}).unwrap_err().to_string();
        assert!(err.contains("checksum mismatch"), "{err}");
        assert!(!dest.exists() && !part_path(&dest).exists(), "neither the file nor the part stays");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn download_errors_are_clear() {
        let server = MockServer::start();
        server.route("/gated", Route { status: 401, ..Route::file(Vec::new()) });
        let dir = temp_dir("errors");
        let get = |path: &str| {
            let d = Download { url: server.url(path), dest: dir.join("x.gguf"), size: None, sha256: None, token: None };
            download_file(&agent(), &d, &AtomicBool::new(false), &mut |_, _| {}).unwrap_err().to_string()
        };
        let missing = get("/nope");
        assert!(missing.contains("not found (404)"), "{missing}");
        let gated = get("/gated");
        assert!(gated.contains("access denied (401)") && gated.contains("token"), "{gated}");
        // Nothing listens on this port.
        let port = TcpListener::bind(("127.0.0.1", 0)).unwrap().local_addr().unwrap().port();
        let d = Download {
            url: format!("http://127.0.0.1:{port}/x"),
            dest: dir.join("y.gguf"),
            size: None,
            sha256: None,
            token: None,
        };
        let down = download_file(&agent(), &d, &AtomicBool::new(false), &mut |_, _| {}).unwrap_err().to_string();
        assert!(down.contains("could not reach"), "{down}");
        // Far more than any disk has free.
        let huge = Download { size: Some(u64::MAX / 4), ..d };
        if free_space(&dir).is_some() {
            let full = download_file(&agent(), &huge, &AtomicBool::new(false), &mut |_, _| {}).unwrap_err().to_string();
            assert!(full.contains("not enough disk space"), "{full}");
        }
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn download_cancel_keeps_the_part_and_a_rerun_finishes() {
        let server = MockServer::start();
        let body = sample_bytes(4 * CHUNK);
        server.route("/f", Route::file(body.clone()));
        let dir = temp_dir("cancel");
        let dest = dir.join("m.gguf");
        let d = Download {
            url: server.url("/f"),
            dest: dest.clone(),
            size: Some(body.len() as u64),
            sha256: Some(sha_of(&body)),
            token: None,
        };
        let cancel = AtomicBool::new(false);
        let err = download_file(&agent(), &d, &cancel, &mut |done, _| {
            if done >= CHUNK as u64 {
                cancel.store(true, Ordering::SeqCst);
            }
        })
        .unwrap_err();
        assert!(err.downcast_ref::<Cancelled>().is_some(), "{err:#}");
        let kept = fs::metadata(part_path(&dest)).unwrap().len();
        assert!(kept > 0 && kept < body.len() as u64, "{kept}");
        assert!(!dest.exists());
        download_file(&agent(), &d, &AtomicBool::new(false), &mut |_, _| {}).unwrap();
        assert_eq!(fs::read(&dest).unwrap(), body);
        // A server without range support sends everything again: still the right file.
        let dest2 = dir.join("n.gguf");
        fs::write(part_path(&dest2), &body[..1000]).unwrap();
        server.route("/g", Route { ranges: false, ..Route::file(body.clone()) });
        let d2 = Download { url: server.url("/g"), dest: dest2.clone(), ..d };
        download_file(&agent(), &d2, &AtomicBool::new(false), &mut |_, _| {}).unwrap();
        assert_eq!(fs::read(&dest2).unwrap(), body);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn model_task_downloads_through_the_tree_api() {
        let server = MockServer::start();
        let model = sample_bytes(300_000);
        let mmproj = sample_bytes(50_000);
        let tree = serde_json::json!([
            {"type": "file", "path": "Custom-27B-PTQ1_0.gguf", "size": model.len(), "lfs": {"oid": sha_of(&model)}},
            {"type": "file", "path": "Custom-27B-mmproj-Q8_0.gguf", "size": mmproj.len(), "lfs": {"oid": sha_of(&mmproj)}},
            {"type": "directory", "path": "assets"},
        ]);
        server.route("/api/models/someone/custom/tree/main?recursive=true", Route::file(tree.to_string().into_bytes()));
        server.route("/someone/custom/resolve/main/Custom-27B-PTQ1_0.gguf", Route::file(model.clone()));
        server.route("/someone/custom/resolve/main/Custom-27B-mmproj-Q8_0.gguf", Route::file(mmproj.clone()));
        let dir = temp_dir("task");
        let cfg = AppConfig {
            runtime: crate::config::RuntimeConfig { dir: Some(dir.clone()), ..Default::default() },
            ..AppConfig::default()
        };
        let src = ModelSource::parse("hf:someone/custom/Custom-27B-PTQ1_0.gguf", Some("Custom-27B-mmproj-Q8_0.gguf"))
            .unwrap();
        let endpoints = Endpoints { hf: server.url(""), ..Endpoints::default() };
        let mut seen = Vec::new();
        let task = RuntimeTask::DownloadModel(src.clone());
        let outcome =
            run_task_with(&task, &cfg, &endpoints, &AtomicBool::new(false), &mut |p| seen.push(p.clone())).unwrap();
        let want = dir.join("models").join("someone--custom").join("Custom-27B-PTQ1_0.gguf");
        assert_eq!(outcome.model.as_deref(), Some(want.as_path()));
        assert_eq!(fs::read(&want).unwrap(), model);
        assert!(outcome.notes.is_empty(), "{:?}", outcome.notes);
        let last = seen.last().unwrap();
        assert_eq!(
            (last.done, last.total, last.files),
            ((model.len() + mmproj.len()) as u64, (model.len() + mmproj.len()) as u64, 2)
        );
        assert!(task.targets(&cfg).contains(&want));
        // Scanned back: one model with its projector, inside the app's folder.
        let found = scan_models(&models_dir(&cfg));
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].repo.as_deref(), Some("someone/custom"));
        assert!(found[0].managed && found[0].mmproj.is_some());
        // A file the repo does not have.
        let wrong = ModelSource::parse("hf:someone/custom/Nope.gguf", Some("none")).unwrap();
        let err =
            run_task_with(&RuntimeTask::DownloadModel(wrong), &cfg, &endpoints, &AtomicBool::new(false), &mut |_| {})
                .unwrap_err()
                .to_string();
        assert!(err.contains("has no file Nope.gguf"), "{err}");
        let _ = fs::remove_dir_all(dir);
    }

    fn write_zip(path: &Path, files: &[(&str, &[u8])]) {
        let file = File::create(path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        let options = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
        for (name, data) in files {
            zip.start_file(*name, options).unwrap();
            zip.write_all(data).unwrap();
        }
        zip.finish().unwrap();
    }

    #[test]
    fn zip_extraction_keeps_only_the_server() {
        let dir = temp_dir("zip");
        let zip_path = dir.join("build.zip");
        write_zip(
            &zip_path,
            &[
                ("llama-server.exe", b"exe"),
                ("llama-server-impl.dll", b"impl"),
                ("ggml-cuda.dll", b"cuda"),
                ("llama-cli.exe", b"cli"),
                ("llama-bench-impl.dll", b"bench"),
                ("LICENSE", b"text"),
                ("sub/../../evil.dll", b"x"),
                ("nested/ggml-base.dll", b"base"),
            ],
        );
        let out = dir.join("out");
        let mut written = extract_server_zip(&zip_path, &out).unwrap();
        written.sort();
        assert_eq!(written, vec!["ggml-base.dll", "ggml-cuda.dll", "llama-server-impl.dll", "llama-server.exe"]);
        assert!(!out.join("llama-cli.exe").exists() && !dir.join("evil.dll").exists());
        assert_eq!(fs::read(out.join("ggml-base.dll")).unwrap(), b"base");
        assert_eq!(build_kind(&out.join("llama-server.exe")), Some("cuda"));
        assert!(keep_server_file("LLAMA-SERVER.EXE") && keep_server_file("cudart64_13.dll"));
        assert!(!keep_server_file("llama-perplexity-impl.dll") && !keep_server_file("ggml-rpc-server.exe"));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn backend_from_nvidia_smi() {
        let banner = |v: &str| {
            format!(
                "+-----------------------------------------------------------------------------+\n\
                 | NVIDIA-SMI 581.57                 Driver Version: 581.57         CUDA Version: {v}     |\n"
            )
        };
        assert_eq!(cuda_version(&banner("13.0")), Some((13, 0)));
        assert_eq!(choose_backend(Some(&banner("13.3")), false).backend, Backend::Cuda133);
        assert_eq!(choose_backend(Some(&banner("13.4")), false).backend, Backend::Cuda133);
        assert_eq!(choose_backend(Some(&banner("14.0")), false).backend, Backend::Cuda133);
        assert_eq!(choose_backend(Some(&banner("13.0")), false).backend, Backend::Cuda124);
        assert_eq!(choose_backend(Some(&banner("12.4")), true).backend, Backend::Cuda124);
        let old = choose_backend(Some(&banner("12.2")), true);
        assert_eq!(old.backend, Backend::Vulkan);
        assert!(old.reason.contains("update the driver"), "{}", old.reason);
        assert_eq!(choose_backend(Some(&banner("11.8")), false).backend, Backend::Cpu);
        let umd = "| NVIDIA-SMI 590.01   Driver Version: 590.01   CUDA UMD Version: 13.3 |";
        assert_eq!(choose_backend(Some(umd), false).backend, Backend::Cuda133);
        let none = choose_backend(None, false);
        assert_eq!(none.backend, Backend::Cpu);
        assert!(none.reason.contains("No NVIDIA driver"), "{}", none.reason);
        assert_eq!(choose_backend(None, true).backend, Backend::Vulkan);
        assert_eq!(choose_backend(Some("garbage"), false).backend, Backend::Cpu);
        assert_eq!(Backend::from_id("CUDA-13.3"), Some(Backend::Cuda133));
        assert_eq!(Backend::from_id("cuda124"), Some(Backend::Cuda124));
        assert_eq!(Backend::from_id("rocm"), None);
        assert_eq!(
            Backend::Cuda133.assets(RELEASE_TAG),
            vec![
                "llama-prism-b10787-73bfe6c-bin-win-cuda-13.3-x64.zip".to_string(),
                "cudart-llama-bin-win-cuda-13.3-x64.zip".to_string()
            ]
        );
        assert_eq!(Backend::Vulkan.assets("t").len(), 2);
        for backend in Backend::ALL {
            assert!(backend.assets(RELEASE_TAG).iter().all(|a| known_asset(a).is_some()), "{backend}");
        }
        assert_eq!(Backend::Cuda133.download_bytes(), 179_514_746 + 390_970_417);
    }

    #[test]
    fn catalog_and_sources() {
        let ids: HashSet<&str> = CATALOG.iter().map(|m| m.id).collect();
        assert_eq!(ids.len(), CATALOG.len(), "unique ids");
        assert_eq!(default_model().id, "27b-ptq1");
        for m in CATALOG {
            for f in std::iter::once(m.model).chain(m.mmproj) {
                assert_eq!(f.sha256.len(), 64, "{}", f.name);
                assert!(f.name.ends_with(".gguf"));
            }
        }
        let src = ModelSource::parse("27B-PTQ1", None).unwrap();
        assert_eq!(src.repo, REPO_27B);
        assert_eq!(src.mmproj.as_deref(), Some(MMPROJ_27B.name));
        assert!(src.catalog().is_some());
        let custom = ModelSource::parse(
            "hf:someone/Ternary-Bonsai-2-27B-Uncensored-Heretic-gguf/Ternary-Bonsai-2-27B-Uncensored-Heretic-PTQ1_0.gguf",
            Some("Ternary-Bonsai-2-27B-mmproj-Q8_0.gguf"),
        )
        .unwrap();
        assert_eq!(custom.repo, "someone/Ternary-Bonsai-2-27B-Uncensored-Heretic-gguf");
        assert!(custom.catalog().is_none());
        let link =
            ModelSource::parse("https://huggingface.co/a/b/resolve/main/sub/M.gguf?download=true", None).unwrap();
        assert_eq!((link.repo.as_str(), link.file.as_str()), ("a/b", "sub/M.gguf"));
        assert_eq!(link.model_path(Path::new("M")), Path::new("M").join("a--b").join("M.gguf"));
        assert!(ModelSource::parse("no-such-id", None).is_err());
        assert!(ModelSource::parse("hf:a/b/readme.md", None).is_err());
        assert_eq!(ModelSource::parse("27b-ptq1", Some("none")).unwrap().mmproj, None);
        assert_eq!(repo_from_dir_name("prism-ml--Ternary-Bonsai-2-27B-gguf").as_deref(), Some(REPO_27B));
        let tree = r#"[{"type":"file","path":"a.gguf","size":5,"lfs":{"oid":"abc"}},{"type":"file","path":"README.md","size":2},{"type":"directory","path":"x"}]"#;
        let files = parse_hf_tree(tree).unwrap();
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].sha256.as_deref(), Some("abc"));
        assert_eq!(files[1].sha256, None);
        let release =
            r#"{"assets":[{"name":"a.zip","size":3,"digest":"sha256:ff"},{"name":"b.zip","size":4,"digest":null}]}"#;
        let assets = parse_release_assets(release).unwrap();
        assert_eq!(assets[0].sha256.as_deref(), Some("ff"));
        assert_eq!(assets[1].sha256, None);
    }

    #[test]
    fn models_scan_pair_and_remove() {
        let dir = temp_dir("scan");
        let models = dir.join("models");
        let repo = models.join("prism-ml--Ternary-Bonsai-2-27B-gguf");
        fs::create_dir_all(&repo).unwrap();
        fs::write(repo.join("Ternary-Bonsai-2-27B-PTQ1_0.gguf"), b"m").unwrap();
        fs::write(repo.join("Ternary-Bonsai-2-27B-PQ2_0.gguf"), b"mm").unwrap();
        fs::write(repo.join("Ternary-Bonsai-2-27B-mmproj-Q8_0.gguf"), b"p").unwrap();
        fs::write(repo.join("Ternary-Bonsai-2-27B-mmproj-BF16.gguf"), b"pp").unwrap();
        fs::write(repo.join("other.gguf.part"), b"x").unwrap();
        let local = models.join(LOCAL_MODELS_DIR);
        fs::create_dir_all(&local).unwrap();
        fs::write(local.join("Ternary-Bonsai-2-27B-Uncensored-Heretic-PTQ1_0.gguf"), b"h").unwrap();
        fs::write(local.join("Ternary-Bonsai-2-27B-mmproj-Q8_0.gguf"), b"p").unwrap();
        let found = scan_models(&models);
        assert_eq!(found.len(), 3, "{found:?}");
        let heretic = find_model(&found, "ternary-bonsai-2-27b-uncensored-heretic-ptq1_0").unwrap();
        assert_eq!(heretic.mmproj.as_deref(), Some(local.join("Ternary-Bonsai-2-27B-mmproj-Q8_0.gguf").as_path()));
        assert_eq!(heretic.repo, None);
        let ptq = find_model(&found, "27b-ptq1").unwrap();
        assert_eq!(ptq.repo.as_deref(), Some(REPO_27B));
        assert!(ptq.mmproj.as_ref().is_some_and(|p| file_name(p).contains("Q8_0")), "Q8_0 preferred");
        assert!(find_model(&found, "Ternary-Bonsai-2-27B-PQ2_0.gguf").is_some());

        // The projector stays while another model of the folder uses it.
        let removed = remove_model(ptq, &models).unwrap();
        assert_eq!(removed.len(), 1, "{removed:?}");
        let pq2 = scan_models(&models).into_iter().find(|m| m.id.ends_with("PQ2_0")).unwrap();
        let removed = remove_model(&pq2, &models).unwrap();
        assert_eq!(removed.len(), 2, "model + projector: {removed:?}");
        // Never a file outside the models folder.
        let outside = describe_model(&local.join("Ternary-Bonsai-2-27B-Uncensored-Heretic-PTQ1_0.gguf"), Some(&repo));
        assert!(!outside.managed);
        assert!(remove_model(&outside, &repo).unwrap_err().to_string().contains("not deleted"));
        assert!(local.join("Ternary-Bonsai-2-27B-Uncensored-Heretic-PTQ1_0.gguf").exists());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn import_copies_from_a_demo_folder() {
        let dir = temp_dir("import");
        let demo = dir.join("Bonsai-demo");
        let bin = demo.join("bin").join("cuda");
        let model_dir = demo.join("models").join("bonsai2-gguf").join("27B");
        fs::create_dir_all(&bin).unwrap();
        fs::create_dir_all(&model_dir).unwrap();
        fs::create_dir_all(demo.join("scripts")).unwrap();
        fs::write(demo.join("scripts").join("start_llama_server.ps1"), b"#").unwrap();
        fs::write(demo.join("scripts").join(WEBUI_CONFIG_FILE), b"{}").unwrap();
        for name in
            ["llama-server.exe", "llama-server-impl.dll", "ggml-cuda.dll", "llama-cli.exe", "llama-cli-impl.dll"]
        {
            fs::write(bin.join(name), name.as_bytes()).unwrap();
        }
        fs::write(bin.join(".llama_release"), b"prism-b10787-73bfe6c cuda-13.3").unwrap();
        fs::write(model_dir.join("Ternary-Bonsai-2-27B-Uncensored-Heretic-PTQ1_0.gguf"), sample_bytes(5000)).unwrap();
        fs::write(model_dir.join("Ternary-Bonsai-2-27B-mmproj-Q8_0.gguf"), sample_bytes(700)).unwrap();
        let cfg = AppConfig {
            runtime: crate::config::RuntimeConfig { dir: Some(dir.join("rt")), ..Default::default() },
            ..AppConfig::default()
        };
        let plan = plan_import(&demo, &cfg).unwrap();
        assert_eq!(plan.items.len(), 3 + 2 + 1, "{:?}", plan.items);
        let task = RuntimeTask::Import { dir: demo.clone(), mode: ImportMode::Copy };
        let outcome = run_task(&task, &cfg, &AtomicBool::new(false), &mut |_| {}).unwrap();
        let exe = outcome.server_exe.clone().unwrap();
        assert!(
            exe.is_file() && exe.ends_with("bin/prism-b10787-73bfe6c-cuda-13.3/llama-server.exe"),
            "{}",
            exe.display()
        );
        assert!(!exe.with_file_name("llama-cli.exe").exists());
        let model = outcome.model.clone().unwrap();
        assert!(model.starts_with(dir.join("rt").join("models").join(LOCAL_MODELS_DIR)) && model.is_file());
        assert!(outcome.mmproj.as_ref().is_some_and(|p| p.is_file()));
        assert!(dir.join("rt").join(WEBUI_CONFIG_FILE).is_file());
        assert!(
            model_dir.join("Ternary-Bonsai-2-27B-Uncensored-Heretic-PTQ1_0.gguf").is_file(),
            "copy keeps the source"
        );
        let builds = installed_builds(&cfg);
        assert_eq!(builds.len(), 1);
        assert_eq!((builds[0].tag.as_str(), builds[0].backend.as_str()), ("prism-b10787-73bfe6c", "cuda-13.3"));
        // Again: nothing to copy.
        let again = run_task(&task, &cfg, &AtomicBool::new(false), &mut |_| {}).unwrap();
        assert!(again.notes.iter().any(|n| n.contains("already in the app")), "{:?}", again.notes);
        // Move: the sources go away.
        let cfg2 = AppConfig {
            runtime: crate::config::RuntimeConfig { dir: Some(dir.join("rt2")), ..Default::default() },
            ..AppConfig::default()
        };
        let moved = RuntimeTask::Import { dir: demo.clone(), mode: ImportMode::Move };
        assert!(touches(&moved.targets(&cfg2), &[model_dir.join("Ternary-Bonsai-2-27B-mmproj-Q8_0.gguf").as_path()]));
        run_task(&moved, &cfg2, &AtomicBool::new(false), &mut |_| {}).unwrap();
        assert!(!model_dir.join("Ternary-Bonsai-2-27B-Uncensored-Heretic-PTQ1_0.gguf").exists());
        assert!(!bin.join("llama-server.exe").exists() && bin.join("llama-cli.exe").exists());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn helpers() {
        assert_eq!(
            split_args(r#"--flash-attn on  -t 8 --alias "my model" """#),
            vec!["--flash-attn", "on", "-t", "8", "--alias", "my model", ""]
        );
        assert!(split_args("   ").is_empty());
        assert_eq!(launcher_sampling(Path::new("X-27B-PTQ1_0.gguf")).last(), Some(&"0.05"));
        assert!(launcher_sampling(Path::new("Ternary-Bonsai-2-4B-PTQ1_0.gguf")).contains(&"--presence-penalty"));
        assert_eq!(join_url("https://a.b/x/y?q", "/z"), "https://a.b/z");
        assert_eq!(join_url("https://a.b/x/y", "z"), "https://a.b/x/z");
        assert_eq!(join_url("https://a.b/x", "https://c.d/e"), "https://c.d/e");
        assert_eq!(host_of("https://HF.co/x"), "https://hf.co");
        assert!(is_inside(Path::new(r"C:\a\b\c.gguf"), Path::new(r"C:\a")) || !cfg!(windows));
        assert!(!is_inside(Path::new(r"C:\ab\c"), Path::new(r"C:\a")));
        assert!(touches(&[PathBuf::from("/x/bin/b1")], &[Path::new("/x/bin/b1/llama-server.exe")]));
        assert!(!touches(&[PathBuf::from("/x/bin/b1")], &[Path::new("/x/bin/b2/llama-server.exe")]));
        assert_eq!(fmt_bytes(5_946_648_928), "5.5 GB");
        assert_eq!(fmt_duration(65), "1m 05s");
        let p = TaskProgress {
            done: 1024 * 1024 * 1024,
            total: 2 * 1024 * 1024 * 1024,
            bytes_per_sec: 10.0 * 1048576.0,
            eta_secs: Some(100),
            ..Default::default()
        };
        assert_eq!(p.line(), "1.0 GB of 2.0 GB · 10.0 MB/s · 1m 40s left");
        assert!((p.fraction() - 0.5).abs() < 1e-6);
        let msg = missing_dlls_message(&["VCRUNTIME140.dll".into(), "MSVCP140.dll".into()]).unwrap();
        assert!(msg.contains("Visual C++") && msg.contains(VC_REDIST_URL), "{msg}");
        assert_eq!(missing_dlls_message(&[]), None);
    }

    /// The test binary itself is a PE file on Windows: its imports include KERNEL32.
    #[cfg(windows)]
    #[test]
    fn reads_pe_imports() {
        let exe = std::env::current_exe().unwrap();
        let imports = pe_imports(&exe).unwrap();
        assert!(imports.iter().any(|i| i.eq_ignore_ascii_case("kernel32.dll")), "{imports:?}");
        assert!(pe_imports(Path::new("Cargo.toml")).is_err());
        // Everything a Rust test binary imports is on Windows.
        let dir = temp_dir("pe");
        let copy = dir.join("t.exe");
        fs::copy(&exe, &copy).unwrap();
        assert!(missing_system_dlls(&copy).is_empty(), "{:?}", missing_system_dlls(&copy));
        let _ = fs::remove_dir_all(dir);
    }

    /// Real network (small files only): the 18 MB CPU build. `cargo test --lib runtime:: -- --ignored`
    #[test]
    #[ignore]
    fn live_cpu_build() {
        let dir = temp_dir("live-build");
        let cfg = AppConfig {
            runtime: crate::config::RuntimeConfig { dir: Some(dir.clone()), ..Default::default() },
            ..AppConfig::default()
        };
        let outcome = run_task(&RuntimeTask::DownloadServer(Backend::Cpu), &cfg, &AtomicBool::new(false), &mut |p| {
            eprintln!("{} {} {}", p.phase, p.file, p.line())
        });
        let _ = fs::remove_dir_all(&dir);
        let outcome = outcome.unwrap();
        eprintln!("{}", outcome.message);
    }

    /// Real network: a small non-LFS file of the 27B repo through resolve/main (redirects).
    #[test]
    #[ignore]
    fn live_hf_readme() {
        let dir = temp_dir("live-hf");
        let readme = Download {
            url: format!("{HF_BASE_URL}/{REPO_27B}/resolve/main/README.md"),
            dest: dir.join("README.md"),
            size: None,
            sha256: None,
            token: None,
        };
        let result = download_file(&agent(), &readme, &AtomicBool::new(false), &mut |_, _| {});
        let len = fs::metadata(dir.join("README.md")).map(|m| m.len()).unwrap_or(0);
        let _ = fs::remove_dir_all(&dir);
        result.unwrap();
        assert!(len > 1000, "{len}");
    }
}
