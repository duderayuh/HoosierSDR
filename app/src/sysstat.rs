//! The strip at the bottom: what the machine is doing while it listens.

use serde::Serialize;
use std::sync::{Mutex, OnceLock};
use sysinfo::{Disks, Pid, ProcessRefreshKind, ProcessesToUpdate, System};
use tauri::State;

use crate::AppState;

#[derive(Serialize)]
pub struct SysStatus {
    /// This process, percent of one core.
    pub cpu_app: f32,
    /// Whole machine, percent of all cores.
    pub cpu_total: f32,
    pub cores: usize,
    pub mem_app_mb: f64,
    pub mem_used_mb: f64,
    pub mem_total_mb: f64,
    /// Free space on the volume holding the call library.
    pub disk_free_gb: f64,
    pub disk_total_gb: f64,
    pub uptime_secs: u64,
    pub library_calls: i64,
    pub library_minutes: f64,
}

static SYS: OnceLock<Mutex<System>> = OnceLock::new();
static STARTED: OnceLock<std::time::Instant> = OnceLock::new();

#[tauri::command]
pub fn sys_status(state: State<AppState>) -> SysStatus {
    let started = STARTED.get_or_init(std::time::Instant::now);
    let mut sys = SYS.get_or_init(|| Mutex::new(System::new())).lock().unwrap();
    sys.refresh_cpu_usage();
    sys.refresh_memory();
    let pid = Pid::from_u32(std::process::id());
    sys.refresh_processes_specifics(ProcessesToUpdate::Some(&[pid]), true, ProcessRefreshKind::nothing().with_cpu().with_memory());
    let (cpu_app, mem_app) = sys
        .process(pid)
        .map(|p| (p.cpu_usage(), p.memory() as f64 / 1e6))
        .unwrap_or((0.0, 0.0));
    let lib_dir = state.library_dir.lock().unwrap().clone();
    let disks = Disks::new_with_refreshed_list();
    // The disk whose mount point is the longest prefix of the library path.
    let (free, total) = lib_dir
        .as_deref()
        .and_then(|p| {
            disks
                .list()
                .iter()
                .filter(|d| p.starts_with(d.mount_point()))
                .max_by_key(|d| d.mount_point().as_os_str().len())
                .map(|d| (d.available_space(), d.total_space()))
        })
        .or_else(|| disks.list().first().map(|d| (d.available_space(), d.total_space())))
        .unwrap_or((0, 0));
    let (calls, minutes) = {
        let db = state.db.lock().unwrap().clone();
        db.and_then(|db| {
            let c = db.lock().unwrap();
            crate::library::stats(&c).ok().map(|(n, secs, _)| (n, secs / 60.0))
        })
        .unwrap_or((0, 0.0))
    };
    SysStatus {
        cpu_app,
        cpu_total: sys.global_cpu_usage(),
        cores: sys.cpus().len().max(1),
        mem_app_mb: mem_app,
        mem_used_mb: sys.used_memory() as f64 / 1e6,
        mem_total_mb: sys.total_memory() as f64 / 1e6,
        disk_free_gb: free as f64 / 1e9,
        disk_total_gb: total as f64 / 1e9,
        uptime_secs: started.elapsed().as_secs(),
        library_calls: calls,
        library_minutes: minutes,
    }
}
