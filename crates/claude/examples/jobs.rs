//! What Giverny sees: `cargo run -p giverny-claude --example jobs`
fn main() {
    let dirs: Vec<std::path::PathBuf> = giverny_claude::profiles::discover(&[])
        .into_iter()
        .map(|p| p.config_dir)
        .collect();
    for job in giverny_claude::jobs::scan(dirs) {
        println!(
            "{:<10} {:?}{} tasks={} live={} pinned={}\n   {}\n   cwd={:?}\n   resume={:?}",
            job.id,
            job.state,
            if job.state.needs_you() {
                "  <- NEEDS YOU"
            } else {
                ""
            },
            job.tasks,
            job.live,
            job.pinned,
            job.detail.as_deref().unwrap_or("-"),
            job.cwd,
            job.resume_target(),
        );
    }
}
