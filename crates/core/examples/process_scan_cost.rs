//! Compare per-tab process scans with one batched sweep; starts no children.
//! Run: cargo run --release -p giverny-core --example process_scan_cost
//! Distinct nonexistent shell PIDs model idle tabs (no cmdline reads).
//! Results depend on the host process count; this is not a UI benchmark.
use std::{hint::black_box, time::Instant};

fn main() {
    #[cfg(target_os = "linux")]
    println!(
        "numeric /proc entries: {}",
        std::fs::read_dir("/proc")
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().parse::<u32>().is_ok())
            .count()
    );
    for tabs in [1, 4, 16, 32] {
        let pids: Vec<u32> = (0..tabs).map(|i| u32::MAX - i).collect();
        let mut per_tab = Vec::new();
        let mut batched = Vec::new();
        for _ in 0..11 {
            let start = Instant::now();
            for &pid in &pids {
                black_box(giverny_core::procs::foreground_command(pid));
            }
            per_tab.push(start.elapsed().as_secs_f64() * 1000.0);
            let start = Instant::now();
            black_box(giverny_core::procs::foreground_commands(&pids));
            batched.push(start.elapsed().as_secs_f64() * 1000.0);
        }
        per_tab.sort_by(f64::total_cmp);
        batched.sort_by(f64::total_cmp);
        println!(
            "{tabs:2} tabs: median per-tab {:.3} ms, batched {:.3} ms ({:.1}x)",
            per_tab[5],
            batched[5],
            per_tab[5] / batched[5]
        );
    }
}
