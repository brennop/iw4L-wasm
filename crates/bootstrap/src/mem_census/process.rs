//! Process-level memory numbers. Windows: psapi counters plus a committed-region
//! walk by type. Linux: `/proc/self/status` and `/proc/self/smaps_rollup`
//! (compiled, not run on this box).

use serde_json::{Value, json};

#[cfg(windows)]
pub fn sample() -> Value {
    #[repr(C)]
    #[derive(Default)]
    struct Counters {
        cb: u32,
        page_fault_count: u32,
        peak_working_set: usize,
        working_set: usize,
        quota_peak_paged_pool: usize,
        quota_paged_pool: usize,
        quota_peak_nonpaged_pool: usize,
        quota_nonpaged_pool: usize,
        pagefile_usage: usize,
        peak_pagefile_usage: usize,
        private_usage: usize,
    }
    #[repr(C)]
    struct BasicInfo {
        base: usize,
        alloc_base: usize,
        alloc_protect: u32,
        partition: u16,
        _pad: u16,
        size: usize,
        state: u32,
        protect: u32,
        kind: u32,
    }
    unsafe extern "system" {
        fn GetCurrentProcess() -> isize;
        fn K32GetProcessMemoryInfo(p: isize, c: *mut Counters, cb: u32) -> i32;
        fn VirtualQuery(addr: usize, info: *mut BasicInfo, len: usize) -> usize;
    }
    let mut c = Counters::default();
    c.cb = std::mem::size_of::<Counters>() as u32;
    // SAFETY: c is a correctly sized, writable PROCESS_MEMORY_COUNTERS_EX.
    let ok = unsafe { K32GetProcessMemoryInfo(GetCurrentProcess(), &mut c, c.cb) } != 0;
    if !ok {
        return json!({"error": "GetProcessMemoryInfo failed"});
    }
    // Committed bytes by region type: MEM_IMAGE 0x1000000, MEM_MAPPED 0x40000, MEM_PRIVATE 0x20000.
    let (mut image, mut mapped, mut private) = (0u64, 0u64, 0u64);
    let mut addr = 0usize;
    loop {
        // SAFETY: zeroed BasicInfo is valid for a plain-integer struct.
        let mut info: BasicInfo = unsafe { std::mem::zeroed() };
        let got = unsafe { VirtualQuery(addr, &mut info, std::mem::size_of::<BasicInfo>()) };
        if got == 0 {
            break;
        }
        if info.state == 0x1000 {
            match info.kind {
                0x100_0000 => image += info.size as u64,
                0x4_0000 => mapped += info.size as u64,
                0x2_0000 => private += info.size as u64,
                _ => {}
            }
        }
        match info.base.checked_add(info.size) {
            Some(next) if next > addr => addr = next,
            _ => break,
        }
    }
    json!({
        "working_set": c.working_set,
        "peak_working_set": c.peak_working_set,
        "private_bytes": c.private_usage,
        "pagefile_usage": c.pagefile_usage,
        "committed_image": image,
        "committed_mapped": mapped,
        "committed_private": private,
    })
}

#[cfg(target_os = "linux")]
pub fn sample() -> Value {
    fn kib(text: &str, key: &str) -> Option<u64> {
        text.lines().find_map(|l| {
            let rest = l.strip_prefix(key)?;
            rest.split_whitespace()
                .next()?
                .parse::<u64>()
                .ok()?
                .checked_mul(1024)
        })
    }
    let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    let smaps = std::fs::read_to_string("/proc/self/smaps_rollup").unwrap_or_default();
    json!({
        "working_set": kib(&status, "VmRSS:"),
        "peak_working_set": kib(&status, "VmHWM:"),
        "private_bytes": kib(&smaps, "Private_Dirty:").zip(kib(&smaps, "Private_Clean:")).map(|(a, b)| a + b),
        "rss_anon": kib(&status, "RssAnon:"),
        "rss_file": kib(&status, "RssFile:"),
        "rss_shmem": kib(&status, "RssShmem:"),
        "vm_size": kib(&status, "VmSize:"),
        "smaps_pss": kib(&smaps, "Pss:"),
        "smaps_private_dirty": kib(&smaps, "Private_Dirty:"),
        "smaps_shared_clean": kib(&smaps, "Shared_Clean:"),
        "smaps_anonymous": kib(&smaps, "Anonymous:"),
        "threads": status.lines().find_map(|l| l.strip_prefix("Threads:")?.trim().parse::<u64>().ok()),
    })
}

#[cfg(not(any(windows, target_os = "linux")))]
pub fn sample() -> Value {
    json!({"error": "no process counters on this platform"})
}
