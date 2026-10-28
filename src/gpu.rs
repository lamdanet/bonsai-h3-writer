//! NVIDIA GPU status via `nvidia-smi` (run hidden, no console window).
//!
//! One snapshot runs two queries (each ~60-300 ms):
//! - `nvidia-smi --query-gpu=name,memory.used,memory.total,temperature.gpu,utilization.gpu,uuid
//!   --format=csv,noheader,nounits` -> `NVIDIA GeForce RTX 4060 Laptop GPU, 7775, 8188, 49, 0, GPU-4c86...`
//! - `nvidia-smi --query-compute-apps=pid,process_name,used_memory,gpu_uuid --format=csv,noheader,nounits`
//!   -> `22240, D:\Bonsai-demo\bin\cuda\llama-server.exe, [N/A], GPU-4c86...`
//!
//! The trailing `uuid` / `gpu_uuid` columns only serve to keep processes of other GPUs out of the
//! snapshot of the first GPU; lines without them parse as well. Values such as `[N/A]` or
//! `[Not Supported]` read as 0. On Windows (WDDM driver model) the per-process memory is always
//! `[N/A]`, so [`vram_used_by_others`] falls back to the total used memory - unless a
//! llama-server is listed, whose own share must never count as other programs'.

use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// A process using the GPU.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct GpuProcess {
    pub pid: u32,
    /// Executable file name, e.g. `python.exe`.
    pub name: String,
    /// VRAM in MB; 0 when the driver reports `[N/A]` (common on Windows WDDM).
    pub mem_mb: u32,
}

/// Snapshot of the first NVIDIA GPU.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct GpuInfo {
    pub name: String,
    pub mem_used_mb: u32,
    pub mem_total_mb: u32,
    pub temp_c: u32,
    pub util_pct: u32,
    /// Compute processes (`nvidia-smi --query-compute-apps=pid,process_name,used_memory`).
    pub processes: Vec<GpuProcess>,
}

impl GpuInfo {
    /// Used / total, 0.0..=1.0.
    pub fn mem_fraction(&self) -> f32 {
        if self.mem_total_mb == 0 {
            0.0
        } else {
            self.mem_used_mb as f32 / self.mem_total_mb as f32
        }
    }
}

/// How long one `nvidia-smi` call may take before it is killed (a wedged driver can hang it).
const NVIDIA_SMI_TIMEOUT: Duration = Duration::from_secs(5);
const GPU_QUERY: &str = "--query-gpu=name,memory.used,memory.total,temperature.gpu,utilization.gpu,uuid";
const APPS_QUERY: &str = "--query-compute-apps=pid,process_name,used_memory,gpu_uuid";
const CSV_FORMAT: &str = "--format=csv,noheader,nounits";

/// Query the GPU; `None` when nvidia-smi is missing or fails. Takes ~100-300 ms.
pub fn query() -> Option<GpuInfo> {
    let (exe, gpu_csv) = nvidia_smi_candidates().into_iter().find_map(|exe| {
        let out = crate::server::run_hidden(&exe, [GPU_QUERY, CSV_FORMAT], NVIDIA_SMI_TIMEOUT).ok()?;
        out.success.then_some((exe, out.stdout))
    })?;
    let gpu = parse_gpu_csv(&gpu_csv).into_iter().next()?;
    // A failing process query still leaves a useful GPU snapshot.
    let apps_csv = crate::server::run_hidden(&exe, [APPS_QUERY, CSV_FORMAT], NVIDIA_SMI_TIMEOUT)
        .ok()
        .filter(|o| o.success)
        .map(|o| o.stdout)
        .unwrap_or_default();
    let processes = parse_compute_apps_csv(&apps_csv)
        .into_iter()
        .filter(|app| match (&gpu.uuid, &app.gpu_uuid) {
            (Some(gpu_uuid), Some(app_uuid)) => gpu_uuid.eq_ignore_ascii_case(app_uuid),
            _ => true,
        })
        .map(|app| app.process)
        .collect();
    Some(GpuInfo {
        name: gpu.name,
        mem_used_mb: gpu.mem_used_mb,
        mem_total_mb: gpu.mem_total_mb,
        temp_c: gpu.temp_c,
        util_pct: gpu.util_pct,
        processes,
    })
}

/// GPU processes other than llama-server (e.g. ComfyUI's python.exe).
pub fn other_processes(info: &GpuInfo) -> Vec<GpuProcess> {
    info.processes.iter().filter(|p| !is_llama_server(&p.name)).cloned().collect()
}

/// VRAM (MB) used by everything except llama-server. Uses per-process numbers when available,
/// otherwise `mem_used_mb` minus llama-server's share (assume ~0 when it is not running).
///
/// Details:
/// - llama-server not running: all used VRAM counts (desktop, games and other graphics apps do not
///   appear in the compute process list but still take memory).
/// - llama-server running with known per-process memory: `mem_used_mb` minus its numbers.
/// - llama-server running with `[N/A]` memory (Windows WDDM): the other compute processes'
///   numbers when they are all known, otherwise 0 - the split is unknown, and most of the used
///   VRAM is llama-server's own (it must not be blamed on e.g. an idle ComfyUI).
pub fn vram_used_by_others(info: &GpuInfo) -> u32 {
    let llama: Vec<&GpuProcess> = info.processes.iter().filter(|p| is_llama_server(&p.name)).collect();
    if llama.is_empty() {
        return info.mem_used_mb;
    }
    if llama.iter().all(|p| p.mem_mb > 0) {
        return info.mem_used_mb.saturating_sub(llama.iter().map(|p| p.mem_mb).sum());
    }
    let others = other_processes(info);
    if !others.is_empty() && others.iter().all(|p| p.mem_mb > 0) {
        return others.iter().map(|p| p.mem_mb).sum();
    }
    0
}

/// True for llama-server's executable (`llama-server.exe`, `llama-server`), any case.
pub fn is_llama_server(name: &str) -> bool {
    exe_file_name(name).to_ascii_lowercase().starts_with("llama-server")
}

/// What a GPU process probably is, for warnings (`python.exe` -> `ComfyUI?`).
pub fn process_hint(name: &str) -> Option<&'static str> {
    let lower = exe_file_name(name).to_ascii_lowercase();
    let stem = lower.strip_suffix(".exe").unwrap_or(&lower);
    if stem.starts_with("python") {
        Some("ComfyUI?")
    } else if stem.contains("comfyui") {
        Some("ComfyUI")
    } else if stem.starts_with("ollama") {
        Some("Ollama")
    } else if stem.contains("lm studio") || stem.starts_with("lm-studio") || stem == "lms" {
        Some("LM Studio")
    } else if stem == "blender" {
        Some("Blender")
    } else if stem.starts_with("llama-") {
        Some("llama.cpp")
    } else {
        None
    }
}

/// `python.exe (ComfyUI?) 6.1 GB` - name, hint and memory when known.
pub fn describe_process(p: &GpuProcess) -> String {
    let mut s = p.name.clone();
    if let Some(hint) = process_hint(&p.name) {
        s.push_str(&format!(" ({hint})"));
    }
    if p.mem_mb > 0 {
        s.push_str(&format!(" {}", fmt_mb(p.mem_mb)));
    }
    s
}

/// `6.1 GB` / `830 MB` (nvidia-smi reports MiB).
pub fn fmt_mb(mb: u32) -> String {
    if mb >= 1024 {
        format!("{:.1} GB", mb as f64 / 1024.0)
    } else {
        format!("{mb} MB")
    }
}

/// Why a non-forced Bonsai start should wait, or `None` when other programs use at most
/// `threshold_mb` of VRAM (`threshold_mb == 0` never refuses) or while a llama-server is running
/// (then the used memory is mostly its own, and a start finds Bonsai already running).
///
/// Example: `python.exe (ComfyUI?) uses 6.1 GB of the 8.0 GB GPU memory. ...`
pub fn vram_busy_message(info: &GpuInfo, threshold_mb: u32) -> Option<String> {
    if info.processes.iter().any(|p| is_llama_server(&p.name)) {
        return None;
    }
    let used = vram_used_by_others(info);
    if threshold_mb == 0 || used <= threshold_mb {
        return None;
    }
    let others = other_processes(info);
    let of_total = if info.mem_total_mb > 0 {
        format!(" of the {} GPU memory", fmt_mb(info.mem_total_mb))
    } else {
        " of GPU memory".to_string()
    };
    let who = match others.as_slice() {
        [] => format!(
            "Other programs use {}{of_total} (graphics apps and games are not listed by nvidia-smi)",
            fmt_mb(used)
        ),
        [one] => {
            let mut name = one.name.clone();
            if let Some(hint) = process_hint(&one.name) {
                name.push_str(&format!(" ({hint})"));
            }
            format!("{name} (PID {}) uses {}{of_total}", one.pid, fmt_mb(used))
        }
        many => {
            let names: Vec<String> = many.iter().map(describe_process).collect();
            format!("{} use {}{of_total}", names.join(", "), fmt_mb(used))
        }
    };
    Some(format!(
        "{who}. Bonsai 27B needs about 7.7 GB, so it would likely fail with \"out of memory\". \
         Close {} first (e.g. ComfyUI), or force the start.",
        if others.len() == 1 { "it" } else { "them" }
    ))
}

/// One row of the GPU query (public for tests).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct GpuRow {
    pub name: String,
    pub mem_used_mb: u32,
    pub mem_total_mb: u32,
    pub temp_c: u32,
    pub util_pct: u32,
    /// `GPU-...` when the query asked for it.
    pub uuid: Option<String>,
}

/// One row of the compute-apps query (public for tests).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AppRow {
    pub process: GpuProcess,
    /// `GPU-...` when the query asked for it.
    pub gpu_uuid: Option<String>,
}

/// Parse `--query-gpu=name,memory.used,memory.total,temperature.gpu,utilization.gpu[,uuid]` CSV
/// (no header, no units). Lines that do not look like a GPU row are skipped.
pub fn parse_gpu_csv(text: &str) -> Vec<GpuRow> {
    text.lines().filter_map(parse_gpu_line).collect()
}

fn parse_gpu_line(line: &str) -> Option<GpuRow> {
    let mut fields: Vec<&str> = line.split(',').map(str::trim).collect();
    let uuid = take_uuid(&mut fields);
    if fields.len() < 5 {
        return None;
    }
    let n = fields.len();
    let name = fields[..n - 4].join(",").trim().to_string();
    let used = parse_num(fields[n - 4]);
    let total = parse_num(fields[n - 3]);
    if name.is_empty() || (used.is_none() && total.is_none()) {
        return None;
    }
    Some(GpuRow {
        name,
        mem_used_mb: used.unwrap_or(0),
        mem_total_mb: total.unwrap_or(0),
        temp_c: parse_num(fields[n - 2]).unwrap_or(0),
        util_pct: parse_num(fields[n - 1]).unwrap_or(0),
        uuid,
    })
}

/// Parse `--query-compute-apps=pid,process_name,used_memory[,gpu_uuid]` CSV (no header, no
/// units). Full paths become file names; `[N/A]` memory reads as 0.
pub fn parse_compute_apps_csv(text: &str) -> Vec<AppRow> {
    text.lines().filter_map(parse_app_line).collect()
}

fn parse_app_line(line: &str) -> Option<AppRow> {
    let mut fields: Vec<&str> = line.split(',').map(str::trim).collect();
    let gpu_uuid = take_uuid(&mut fields);
    let pid: u32 = fields.first()?.parse().ok()?;
    let (name_fields, mem) = match fields.len() {
        0 | 1 => return None,
        2 => (&fields[1..], 0),
        n => (&fields[1..n - 1], parse_num(fields[n - 1]).unwrap_or(0)),
    };
    let raw_name = name_fields.join(",");
    Some(AppRow { process: GpuProcess { pid, name: exe_file_name(&raw_name), mem_mb: mem }, gpu_uuid })
}

/// Remove and return a trailing `GPU-...` / `MIG-...` uuid field.
fn take_uuid(fields: &mut Vec<&str>) -> Option<String> {
    let last = *fields.last()?;
    let upper = last.to_ascii_uppercase();
    if fields.len() > 1 && (upper.starts_with("GPU-") || upper.starts_with("MIG-")) {
        fields.pop();
        Some(last.to_string())
    } else {
        None
    }
}

/// A numeric CSV value: `7775`, ` 49 `, `12.5`, `8188 MiB`; `[N/A]`, `[Not Supported]`, `N/A` -> None.
pub fn parse_num(field: &str) -> Option<u32> {
    let f = field.trim();
    let end = f.find(|c: char| !(c.is_ascii_digit() || c == '.')).unwrap_or(f.len());
    let value: f64 = f[..end].parse().ok()?;
    if !value.is_finite() || value < 0.0 {
        return None;
    }
    Some(value.round().min(f64::from(u32::MAX)) as u32)
}

/// `D:\Bonsai-demo\bin\cuda\llama-server.exe` -> `llama-server.exe`; `[Insufficient Permissions]`
/// -> `unknown`.
fn exe_file_name(path: &str) -> String {
    let p = path.trim();
    if p.is_empty() || p.starts_with('[') {
        return "unknown".to_string();
    }
    p.rsplit(['\\', '/']).next().map(str::trim).filter(|s| !s.is_empty()).unwrap_or(p).to_string()
}

/// `nvidia-smi` on PATH first, then the usual install locations.
pub(crate) fn nvidia_smi_candidates() -> Vec<PathBuf> {
    let mut out = vec![PathBuf::from("nvidia-smi")];
    #[cfg(windows)]
    {
        if let Some(root) = std::env::var_os("SystemRoot") {
            out.push(PathBuf::from(root).join("System32").join("nvidia-smi.exe"));
        }
        let program_files =
            std::env::var_os("ProgramFiles").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(r"C:\Program Files"));
        out.push(program_files.join("NVIDIA Corporation").join("NVSMI").join("nvidia-smi.exe"));
    }
    #[cfg(not(windows))]
    {
        out.push(PathBuf::from("/usr/bin/nvidia-smi"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn proc(pid: u32, name: &str, mem_mb: u32) -> GpuProcess {
        GpuProcess { pid, name: name.into(), mem_mb }
    }

    fn info(used: u32, processes: Vec<GpuProcess>) -> GpuInfo {
        GpuInfo {
            name: "NVIDIA GeForce RTX 4060 Laptop GPU".into(),
            mem_used_mb: used,
            mem_total_mb: 8188,
            temp_c: 49,
            util_pct: 0,
            processes,
        }
    }

    #[test]
    fn parses_real_gpu_line() {
        let rows = parse_gpu_csv("NVIDIA GeForce RTX 4060 Laptop GPU, 7775, 8188, 49, 0\r\n");
        assert_eq!(
            rows,
            vec![GpuRow {
                name: "NVIDIA GeForce RTX 4060 Laptop GPU".into(),
                mem_used_mb: 7775,
                mem_total_mb: 8188,
                temp_c: 49,
                util_pct: 0,
                uuid: None,
            }]
        );
        let rows = parse_gpu_csv(
            "NVIDIA GeForce RTX 4060 Laptop GPU, 7775, 8188, 45, 3, GPU-4c86054f-d9d7-9281-d65f-7a0c3fb97c33\n",
        );
        assert_eq!(rows[0].util_pct, 3);
        assert_eq!(rows[0].uuid.as_deref(), Some("GPU-4c86054f-d9d7-9281-d65f-7a0c3fb97c33"));
    }

    #[test]
    fn parses_na_values_names_with_commas_and_skips_garbage() {
        let text = "Tesla T4, [N/A], 15360, [Not Supported], [N/A]\n\
                    Odd, Name GPU, 100, 200.4, 30, 4\n\
                    NVIDIA-SMI has failed because it couldn't communicate with the NVIDIA driver.\n\
                    \n\
                    a, b, c, d, e\n";
        let rows = parse_gpu_csv(text);
        assert_eq!(rows.len(), 2, "{rows:?}");
        assert_eq!(rows[0].name, "Tesla T4");
        assert_eq!((rows[0].mem_used_mb, rows[0].mem_total_mb, rows[0].temp_c), (0, 15360, 0));
        assert_eq!(rows[1].name, "Odd,Name GPU");
        assert_eq!((rows[1].mem_used_mb, rows[1].mem_total_mb), (100, 200));
    }

    #[test]
    fn parses_compute_apps() {
        let text = "22240, D:\\Bonsai-demo\\bin\\cuda\\llama-server.exe, [N/A]\r\n\
                    1234, C:\\Users\\me\\ComfyUI\\python_embeded\\python.exe, 6123\r\n\
                    77, /usr/bin/python3, 512, GPU-abc\n\
                    88, [Insufficient Permissions], [N/A]\n\
                    No running processes found\n\
                    99, C:\\Program Files\\Odd, Name\\x.exe, 10\n";
        let apps = parse_compute_apps_csv(text);
        let procs: Vec<GpuProcess> = apps.iter().map(|a| a.process.clone()).collect();
        assert_eq!(
            procs,
            vec![
                proc(22240, "llama-server.exe", 0),
                proc(1234, "python.exe", 6123),
                proc(77, "python3", 512),
                proc(88, "unknown", 0),
                proc(99, "x.exe", 10),
            ]
        );
        assert_eq!(apps[2].gpu_uuid.as_deref(), Some("GPU-abc"));
        assert_eq!(apps[0].gpu_uuid, None);
        assert!(parse_compute_apps_csv("").is_empty());
    }

    #[test]
    fn parse_num_variants() {
        assert_eq!(parse_num(" 7775 "), Some(7775));
        assert_eq!(parse_num("12.5"), Some(13));
        assert_eq!(parse_num("8188 MiB"), Some(8188));
        assert_eq!(parse_num("[N/A]"), None);
        assert_eq!(parse_num("[Not Supported]"), None);
        assert_eq!(parse_num("N/A"), None);
        assert_eq!(parse_num(""), None);
    }

    #[test]
    fn others_exclude_llama_server() {
        let i = info(7775, vec![proc(1, "llama-server.exe", 0), proc(2, "python.exe", 0)]);
        assert_eq!(other_processes(&i), vec![proc(2, "python.exe", 0)]);
        assert!(is_llama_server("LLAMA-SERVER.EXE"));
        assert!(is_llama_server(r"D:\Bonsai-demo\bin\cuda\llama-server.exe"));
        assert!(!is_llama_server("python.exe"));
    }

    #[test]
    fn vram_used_by_others_cases() {
        // Nothing listed: everything used counts (desktop, games).
        assert_eq!(vram_used_by_others(&info(900, vec![])), 900);
        // Only others, numbers unknown (WDDM): total.
        assert_eq!(vram_used_by_others(&info(6200, vec![proc(2, "python.exe", 0)])), 6200);
        // llama-server alone with [N/A]: all attributed to it.
        assert_eq!(vram_used_by_others(&info(7775, vec![proc(1, "llama-server.exe", 0)])), 0);
        // Known per-process numbers (Linux/TCC): total minus llama-server.
        assert_eq!(vram_used_by_others(&info(7900, vec![proc(1, "llama-server", 7400), proc(2, "python3", 300)])), 500);
        // llama-server [N/A] but the other process is known.
        assert_eq!(
            vram_used_by_others(&info(7900, vec![proc(1, "llama-server.exe", 0), proc(2, "python.exe", 300)])),
            300
        );
        // Both unknown (WDDM, the usual Windows case): Bonsai's share is not counted.
        assert_eq!(vram_used_by_others(&info(7900, vec![proc(1, "llama-server.exe", 0), proc(2, "python.exe", 0)])), 0);
    }

    #[test]
    fn busy_message_names_processes() {
        let i = info(6246, vec![proc(4321, "python.exe", 0)]);
        let msg = vram_busy_message(&i, 1500).unwrap_or_default();
        assert!(msg.starts_with("python.exe (ComfyUI?) (PID 4321) uses 6.1 GB of the 8.0 GB GPU memory"), "{msg}");
        assert!(msg.contains("force"), "{msg}");
        assert_eq!(vram_busy_message(&i, 0), None);
        assert_eq!(vram_busy_message(&i, 7000), None);
        let two = info(5000, vec![proc(1, "python.exe", 3000), proc(2, "blender.exe", 2000)]);
        let msg = vram_busy_message(&two, 1500).unwrap_or_default();
        assert!(msg.contains("python.exe (ComfyUI?) 2.9 GB, blender.exe (Blender) 2.0 GB use 4.9 GB"), "{msg}");
        let none = vram_busy_message(&info(3000, vec![]), 1500).unwrap_or_default();
        assert!(none.starts_with("Other programs use 2.9 GB"), "{none}");
        // Bonsai itself running alone never blocks.
        assert_eq!(vram_busy_message(&info(7775, vec![proc(1, "llama-server.exe", 0)]), 1500), None);
        // Bonsai next to an idle ComfyUI, both [N/A] (this PC): not "python.exe uses 7.3 GB".
        let both = info(7475, vec![proc(22240, "llama-server.exe", 0), proc(19948, "python.exe", 0)]);
        assert_eq!(vram_busy_message(&both, 1500), None);
        // While a llama-server runs, even known numbers do not block (the start finds it running).
        let known = info(7900, vec![proc(1, "llama-server", 5000), proc(2, "python3", 2900)]);
        assert_eq!(vram_used_by_others(&known), 2900);
        assert_eq!(vram_busy_message(&known, 1500), None);
    }

    #[test]
    fn fmt_and_hints() {
        assert_eq!(fmt_mb(830), "830 MB");
        assert_eq!(fmt_mb(6246), "6.1 GB");
        assert_eq!(process_hint("pythonw.exe"), Some("ComfyUI?"));
        assert_eq!(process_hint("ollama_llama_server.exe"), Some("Ollama"));
        assert_eq!(process_hint("explorer.exe"), None);
        assert_eq!(describe_process(&proc(1, "python.exe", 0)), "python.exe (ComfyUI?)");
    }

    /// Live: prints the real nvidia-smi snapshot. `cargo test --lib gpu:: -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn live_query() {
        match query() {
            Some(i) => {
                println!("{i:#?}");
                println!("others: {:?}, used by others: {} MB", other_processes(&i), vram_used_by_others(&i));
                assert!(i.mem_total_mb > 0);
            }
            None => println!("nvidia-smi not available"),
        }
    }
}
