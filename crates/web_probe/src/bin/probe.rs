//! `probe <games_root> <zone>`: the native reference for the browser run.

#[global_allocator]
static ALLOC: diag::ProcessCountingAllocator = diag::ProcessCountingAllocator;

fn main() {
    if let Err(error) = gamefs::install_from_env() {
        eprintln!("{error}");
        std::process::exit(2);
    }
    if std::env::var_os("PROBE_SAMPLE").is_some() {
        std::thread::spawn(|| {
            let start = std::time::Instant::now();
            loop {
                if let Some(live) = diag::process_live_heap_bytes() {
                    eprintln!(
                        "sample +{}ms live={} MiB",
                        start.elapsed().as_millis(),
                        live >> 20
                    );
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
        });
    }
    let mut args = std::env::args().skip(1);
    let (Some(root), Some(zone)) = (args.next(), args.next()) else {
        eprintln!("usage: probe <games_root> <zone>");
        std::process::exit(2);
    };
    match bevy::tasks::futures_lite::future::block_on(web_probe::probe(&root, &zone)) {
        Ok(report) => {
            if let Some(path) = std::env::var_os("PROBE_LINES") {
                let _ = std::fs::write(path, report.lines.join("\n"));
            }
            println!("{}", report.render());
            if let Some(secs) = std::env::var("PROBE_HOLD")
                .ok()
                .and_then(|s| s.parse::<u64>().ok())
            {
                std::thread::sleep(std::time::Duration::from_secs(secs));
            }
        }
        Err(error) => {
            eprintln!("probe: {error}");
            std::process::exit(1);
        }
    }
}
