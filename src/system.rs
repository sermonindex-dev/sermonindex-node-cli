//! Cross-platform host metrics (CPU, memory, disk, network throughput) via sysinfo,
//! plus a Linux CPU temperature read. Works on Linux (incl. the Pi), macOS, Windows.

use std::path::Path;
use sysinfo::{Disks, Networks, System};

pub struct Meter {
    sys: System,
    nets: Networks,
    disks: Disks,
    last: std::time::Instant,
}

#[derive(Clone, Default)]
pub struct Snapshot {
    pub cpu_pct: f32,
    pub temp_c: Option<f32>,
    pub mem_used: u64,
    pub mem_total: u64,
    pub disk_used: u64,
    pub disk_total: u64,
    pub up_bps: f64,
    pub down_bps: f64,
    pub nic: String,
}

impl Meter {
    pub fn new() -> Self {
        let mut sys = System::new();
        sys.refresh_cpu_usage();
        sys.refresh_memory();
        Meter {
            sys,
            nets: Networks::new_with_refreshed_list(),
            disks: Disks::new_with_refreshed_list(),
            last: std::time::Instant::now(),
        }
    }

    /// Sample now. Call on a steady cadence (e.g. every 2 s) so CPU% and the
    /// network deltas are meaningful.
    pub fn sample(&mut self, storage_path: &Path) -> Snapshot {
        self.sys.refresh_cpu_usage();
        self.sys.refresh_memory();
        self.nets.refresh();

        let now = std::time::Instant::now();
        let dt = now.duration_since(self.last).as_secs_f64().max(0.001);
        self.last = now;

        // Sum throughput across non-loopback interfaces; report the busiest name.
        let mut up = 0u64;
        let mut down = 0u64;
        let mut nic = String::new();
        let mut best = 0u64;
        for (name, data) in self.nets.iter() {
            if name == "lo" || name.starts_with("lo") {
                continue;
            }
            up += data.transmitted();
            down += data.received();
            let busy = data.transmitted() + data.received();
            if busy >= best {
                best = busy;
                nic = name.clone();
            }
        }

        // Refresh disk usage in place (cheap) instead of re-enumerating every call.
        self.disks.refresh();
        let (disk_used, disk_total) = disk_for(&self.disks, storage_path);

        Snapshot {
            cpu_pct: self.sys.global_cpu_usage(),
            temp_c: cpu_temp_c(),
            mem_used: self.sys.used_memory(),
            mem_total: self.sys.total_memory(),
            disk_used,
            disk_total,
            up_bps: up as f64 / dt,
            down_bps: down as f64 / dt,
            nic,
        }
    }
}

/// Disk usage for the volume holding `path` (best match by mount-point prefix).
fn disk_for(disks: &Disks, path: &Path) -> (u64, u64) {
    let mut best: Option<(usize, u64, u64)> = None;
    for d in disks.iter() {
        let mp = d.mount_point();
        if path.starts_with(mp) {
            let len = mp.as_os_str().len();
            let used = d.total_space().saturating_sub(d.available_space());
            if best.map(|(l, _, _)| len > l).unwrap_or(true) {
                best = Some((len, used, d.total_space()));
            }
        }
    }
    if let Some((_, used, total)) = best {
        return (used, total);
    }
    // Fallback: the largest disk.
    disks
        .iter()
        .max_by_key(|d| d.total_space())
        .map(|d| {
            (
                d.total_space().saturating_sub(d.available_space()),
                d.total_space(),
            )
        })
        .unwrap_or((0, 0))
}

/// Linux CPU temperature (°C). None on platforms without the thermal zone.
fn cpu_temp_c() -> Option<f32> {
    let raw = std::fs::read_to_string("/sys/class/thermal/thermal_zone0/temp").ok()?;
    let milli: f32 = raw.trim().parse().ok()?;
    Some((milli / 1000.0 * 10.0).round() / 10.0)
}

/// Free bytes on the filesystem holding `path`, or 0 when it cannot be read.
///
/// `sysinfo` reports per-DISK totals, which is the wrong granularity here — the
/// library often lives on a mount that is not the disk sysinfo names first, and
/// a node writing to /mnt/library does not care how much room / has. statvfs is
/// the question actually being asked: how much can I write HERE.
pub fn free_bytes(path: &std::path::Path) -> u64 {
    disk_space(path).0
}

/// (free, total) bytes on the filesystem holding `path` — free as this user
/// may write it, which is what "will it fit" means.
pub fn disk_space(path: &std::path::Path) -> (u64, u64) {
    #[cfg(unix)]
    {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;
        // Walk up to the nearest existing ancestor: the library folder may not
        // have been created yet when this is first asked.
        let mut p = path.to_path_buf();
        while !p.exists() {
            match p.parent() {
                Some(par) => p = par.to_path_buf(),
                None => return (0, 0),
            }
        }
        let Ok(c) = CString::new(p.as_os_str().as_bytes()) else { return (0, 0) };
        unsafe {
            let mut st: libc::statvfs = std::mem::zeroed();
            if libc::statvfs(c.as_ptr(), &mut st) == 0 {
                let fr = st.f_frsize as u64;
                return ((st.f_bavail as u64).saturating_mul(fr), (st.f_blocks as u64).saturating_mul(fr));
            }
        }
        (0, 0)
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        (0, 0)
    }
}

/// Memory charged to the node's systemd service, split the way that matters.
///
/// `systemctl status` prints one "Memory:" figure, and that figure includes
/// the kernel's disk cache for every file the node has written or read — on a
/// node that has just downloaded 400 GB it sits at the service's ceiling and
/// looks like a leak. It isn't: cache is handed back the moment anything else
/// needs it. `anon` is what the program itself is using; `file` is cache.
pub struct ServiceMemory {
    pub anon: u64,
    pub file: u64,
    pub current: u64,
    pub high: Option<u64>,
    pub max: Option<u64>,
}

pub fn service_memory() -> Option<ServiceMemory> {
    let base = std::path::Path::new("/sys/fs/cgroup/system.slice/sermonindex-node.service");
    let stat = std::fs::read_to_string(base.join("memory.stat")).ok()?;
    let (mut anon, mut file) = (0u64, 0u64);
    for l in stat.lines() {
        let mut it = l.split_whitespace();
        match (it.next(), it.next()) {
            (Some("anon"), Some(v)) => anon = v.parse().unwrap_or(0),
            (Some("file"), Some(v)) => file = v.parse().unwrap_or(0),
            _ => {}
        }
    }
    // "max" in these files means unlimited, which parses to None.
    let rd = |n: &str| {
        std::fs::read_to_string(base.join(n))
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok())
    };
    Some(ServiceMemory {
        anon,
        file,
        current: rd("memory.current").unwrap_or(anon + file),
        high: rd("memory.high"),
        max: rd("memory.max"),
    })
}
