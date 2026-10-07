//! `cargo xtask dedicated [options]`: the master and N dedicated hosts on one
//! machine, one command. The master serves the page, the pack and the game on
//! its ws port (D3a), so no separate web server exists. `--help` has the rest.
//!
//! Everything a run writes lives in one run directory: the master binary copy,
//! the web root, the WebTransport cert, and each process's log and status file.
//! The launcher never restarts anything: a host that exits is logged and the
//! master keeps running. Ctrl-C (or SIGTERM) stops every child by PID.

use std::fs;
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use master_protocol::Channel;

use crate::certs::{Ca, San};
use crate::dotenv::Env;
use crate::shell::{self, Res, Step};

const QUIC_PORT: u16 = 4433;
const WT_PORT: u16 = 4435;
const WS_PORT: u16 = 4436;
const READY_TIMEOUT: Duration = Duration::from_secs(300);

const HELP: &str = "\
usage: cargo xtask dedicated [options]      (make dedicated DEDICATED_ARGS='...')

Builds (cargo no-ops when fresh), then starts the master and N dedicated hosts
and prints the page URLs. Ctrl-C stops everything. Nothing is restarted.

options:
  --hosts N          dedicated hosts to start (default 1)
  --name NAME        host name prefix; rooms are NAME-1, NAME-2 ... (default iw4l-dedicated)
  --map MAP          map each host serves (default mp_rust)
  --bind IP          master listen address (default 0.0.0.0, the LAN; 127.0.0.1 = local only)
  --pack PATH        game pack, linked or copied to <web>/game.pack
                     (default on Windows: E:\\iw4l\\packs\\mp_rust_cap512_merge2.pack)
  --certs DIR        dir holding iw4l-ca.pem, server-cert.pem, server-key.pem
                     (default: E:\\iw4l\\o1-duo\\certs if complete, else minted with openssl
                     under the run base dir)
  --run-dir DIR      logs, status files, web root (default $IW4L_RUN_DIR, else
                     E:\\iw4l\\dedicated\\<timestamp> on Windows, else target/dedicated/<timestamp>)
  --public-url URL   https origin of a tunnel in front of the ws port (or $IW4L_PUBLIC_URL)
  --wt-host HOST     DNS-only name (or IP) that reaches this machine's UDP 4435, for a
                     --public-url tunnel (or $IW4L_WT_HOST): /master.json tells pages to
                     dial https://HOST:4435/ (WebTransport) while the page stays on the
                     tunnel. Binds the WebTransport listener on 0.0.0.0 whatever --bind is
                     and adds HOST to its certificate. The share URL then has no transport=ws.
  --no-build         skip the cargo builds; use what is already built
  --release-web      build the wasm with the slow fat-LTO web profile (small, ~10 min); the
                     default is the quick web-dev profile (bigger, not for release)
  --fast             accepted, does nothing: the quick web-dev wasm is the default
  --no-web           skip only the wasm build; reuse dist/web as it is
  --exit-after SECS  test hook: shut down after SECS seconds, as if by Ctrl-C
  --stop-file PATH   test hook: shut down once PATH exists (cargo xtask mem-census uses it)
  --host-exe PATH    run this iw4l binary as the host (default target/play/iw4l)
  -h, --help         this text
";

struct Args {
    hosts: usize,
    name: String,
    map: String,
    bind: Ipv4Addr,
    pack: Option<PathBuf>,
    certs: Option<PathBuf>,
    run_dir: Option<PathBuf>,
    public_url: Option<String>,
    wt_host: Option<String>,
    build: bool,
    web: bool,
    release_web: bool,
    exit_after: Option<Duration>,
    stop_file: Option<PathBuf>,
    host_exe: Option<PathBuf>,
}

fn parse(env: &Env, args: &[String]) -> Res<Option<Args>> {
    let mut out = Args {
        hosts: 1,
        name: "iw4l-dedicated".into(),
        map: "mp_rust".into(),
        bind: Ipv4Addr::UNSPECIFIED,
        pack: None,
        certs: None,
        run_dir: None,
        public_url: env.get("IW4L_PUBLIC_URL"),
        wt_host: env.get("IW4L_WT_HOST"),
        build: true,
        web: true,
        release_web: false,
        exit_after: None,
        stop_file: None,
        host_exe: None,
    };
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        let mut value = |what: &str| -> Res<String> {
            iter.next()
                .cloned()
                .ok_or_else(|| format!("{arg} needs {what}"))
        };
        match arg.as_str() {
            "-h" | "--help" => return Ok(None),
            "--hosts" => {
                out.hosts = value("a count")?
                    .parse()
                    .map_err(|_| "--hosts needs a number".to_string())?;
            }
            "--name" => out.name = value("a name")?,
            "--map" => out.map = value("a map")?,
            "--bind" => {
                out.bind = value("an IPv4 address")?
                    .parse()
                    .map_err(|_| "--bind needs an IPv4 address".to_string())?;
            }
            "--pack" => out.pack = Some(PathBuf::from(value("a path")?)),
            "--certs" => out.certs = Some(PathBuf::from(value("a directory")?)),
            "--run-dir" => out.run_dir = Some(PathBuf::from(value("a directory")?)),
            "--public-url" => out.public_url = Some(value("an https URL")?),
            "--wt-host" => out.wt_host = Some(value("a host name")?),
            "--no-build" => out.build = false,
            "--no-web" => out.web = false,
            "--release-web" => out.release_web = true,
            "--fast" => {}
            "--exit-after" => {
                let secs: u64 = value("seconds")?
                    .parse()
                    .map_err(|_| "--exit-after needs seconds".to_string())?;
                out.exit_after = Some(Duration::from_secs(secs));
            }
            "--stop-file" => out.stop_file = Some(PathBuf::from(value("a path")?)),
            "--host-exe" => out.host_exe = Some(PathBuf::from(value("a path")?)),
            other => return Err(format!("unknown option {other}; see --help")),
        }
    }
    if out.hosts == 0 {
        return Err("--hosts must be at least 1".into());
    }
    if out.name.is_empty() || out.name.contains(char::is_whitespace) {
        return Err("--name must be non-empty and contain no spaces".into());
    }
    if let Some(url) = &mut out.public_url {
        let trimmed = url.trim_end_matches('/').to_string();
        if !(trimmed.starts_with("https://") || trimmed.starts_with("http://")) {
            return Err(format!("--public-url must start with https:// (got {url})"));
        }
        *url = trimmed;
    }
    if let Some(host) = &out.wt_host {
        let host = host.trim();
        if host.is_empty() || host.contains(['/', ':', ' ', '"']) {
            return Err(format!(
                "--wt-host must be a bare host name or IP, no scheme or port (got {host})"
            ));
        }
        out.wt_host = Some(host.to_string());
    }
    Ok(Some(out))
}

static STOP: AtomicBool = AtomicBool::new(false);

#[cfg(windows)]
fn install_ctrl_c() {
    unsafe extern "system" {
        fn SetConsoleCtrlHandler(
            handler: Option<unsafe extern "system" fn(u32) -> i32>,
            add: i32,
        ) -> i32;
    }
    unsafe extern "system" fn handler(_event: u32) -> i32 {
        STOP.store(true, Ordering::SeqCst);
        1
    }
    // SAFETY: kernel32 is always linked; the handler only stores an atomic.
    unsafe {
        SetConsoleCtrlHandler(Some(handler), 1);
    }
}

#[cfg(unix)]
fn install_ctrl_c() {
    unsafe extern "C" {
        fn signal(signum: i32, handler: extern "C" fn(i32)) -> usize;
    }
    extern "C" fn handler(_signum: i32) {
        STOP.store(true, Ordering::SeqCst);
    }
    // SAFETY: libc is always linked; the handler only stores an atomic.
    unsafe {
        signal(2, handler);
        signal(15, handler);
    }
}

/// Every spawned process. Dropping it kills them by PID, so an error halfway
/// through startup cannot leave an orphan either.
struct Fleet {
    children: Vec<Proc>,
}

struct Proc {
    label: String,
    child: Child,
    exited: bool,
}

impl Fleet {
    fn spawn(&mut self, label: &str, cmd: &mut Command, log: &Path) -> Res<()> {
        let out = fs::File::create(log.with_extension("out"))
            .map_err(|e| format!("create {}: {e}", log.display()))?;
        let err = fs::File::create(log.with_extension("err"))
            .map_err(|e| format!("create {}: {e}", log.display()))?;
        let child = cmd
            .stdin(Stdio::null())
            .stdout(out)
            .stderr(err)
            .spawn()
            .map_err(|e| format!("start {label}: {e}"))?;
        println!("{label}: pid {}", child.id());
        self.children.push(Proc {
            label: label.to_string(),
            child,
            exited: false,
        });
        Ok(())
    }

    /// Log each process that has exited since the last call.
    fn reap(&mut self) {
        for proc in &mut self.children {
            if proc.exited {
                continue;
            }
            if let Ok(Some(status)) = proc.child.try_wait() {
                proc.exited = true;
                println!("{}: exited ({status})", proc.label);
            }
        }
    }

    fn alive(&self, index: usize) -> bool {
        !self.children[index].exited
    }

    fn stop_all(&mut self) {
        for proc in self.children.iter_mut().rev() {
            if proc.exited || proc.child.try_wait().ok().flatten().is_some() {
                continue;
            }
            let pid = proc.child.id();
            let _ = proc.child.kill();
            let _ = proc.child.wait();
            println!("{}: stopped pid {pid}", proc.label);
        }
        self.children.clear();
    }
}

impl Drop for Fleet {
    fn drop(&mut self) {
        self.stop_all();
    }
}

fn is_windows() -> bool {
    cfg!(windows)
}

fn base_dir(root: &Path) -> PathBuf {
    if is_windows() {
        PathBuf::from(r"E:\iw4l\dedicated")
    } else {
        root.join("target/dedicated")
    }
}

/// `YYYYMMDD-HHMMSS` in UTC, from the system clock (no date crate here).
pub fn timestamp() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let (days, rem) = (secs / 86_400, secs % 86_400);
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}{month:02}{day:02}-{:02}{:02}{:02}",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

fn certs_complete(ca: &Ca) -> bool {
    ca.ca_cert().is_file() && ca.server_cert().is_file() && ca.server_key().is_file()
}

fn resolve_certs(base: &Path, explicit: Option<&Path>) -> Res<Ca> {
    if let Some(dir) = explicit {
        let ca = Ca::new(dir.to_path_buf());
        if certs_complete(&ca) {
            return Ok(ca);
        }
        return Err(format!(
            "--certs {} must hold iw4l-ca.pem, server-cert.pem and server-key.pem",
            dir.display()
        ));
    }
    if is_windows() {
        let ca = Ca::new(PathBuf::from(r"E:\iw4l\o1-duo\certs"));
        if certs_complete(&ca) {
            return Ok(ca);
        }
    }
    // Nothing to reuse: mint a CA and a label-only server cert (needs openssl).
    let ca = Ca::new(base.join("certs"));
    ca.ensure(&San::Labels)?;
    Ok(ca)
}

fn resolve_pack(explicit: Option<PathBuf>) -> Res<PathBuf> {
    let pack = match explicit {
        Some(path) => path,
        None if is_windows() => PathBuf::from(r"E:\iw4l\packs\mp_rust_cap512_merge2.pack"),
        None => {
            return Err("pass --pack PATH (a game.pack built by `cargo xtask web-pack`)".into());
        }
    };
    if pack.is_file() {
        Ok(pack)
    } else {
        Err(format!("pack not found: {} (see --pack)", pack.display()))
    }
}

fn copy_tree(from: &Path, to: &Path) -> Res<()> {
    fs::create_dir_all(to).map_err(|e| format!("create {}: {e}", to.display()))?;
    for entry in fs::read_dir(from).map_err(|e| format!("read {}: {e}", from.display()))? {
        let entry = entry.map_err(|e| e.to_string())?;
        let dest = to.join(entry.file_name());
        if entry.path().is_dir() {
            copy_tree(&entry.path(), &dest)?;
        } else {
            fs::copy(entry.path(), &dest)
                .map_err(|e| format!("copy {}: {e}", entry.path().display()))?;
        }
    }
    Ok(())
}

fn field<'a>(status: &'a str, key: &str) -> Option<&'a str> {
    status
        .lines()
        .filter_map(|line| line.split_once('='))
        .find_map(|(k, v)| (k.trim() == key).then_some(v.trim()))
}

/// The machine's non-loopback IPv4 addresses, from the OS's own tool.
fn lan_addresses() -> Vec<Ipv4Addr> {
    let output = if cfg!(windows) {
        Command::new("ipconfig").output()
    } else if cfg!(target_os = "macos") {
        Command::new("ifconfig").output()
    } else {
        Command::new("hostname").arg("-I").output()
    };
    let Ok(output) = output else {
        return Vec::new();
    };
    let text = String::from_utf8_lossy(&output.stdout);
    let mut found = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        let candidates: Vec<&str> = if cfg!(windows) {
            if !line.contains("IPv4") {
                continue;
            }
            line.rsplit(':').take(1).collect()
        } else if cfg!(target_os = "macos") {
            match line.strip_prefix("inet ") {
                Some(rest) => rest.split_whitespace().take(1).collect(),
                None => continue,
            }
        } else {
            line.split_whitespace().collect()
        };
        for candidate in candidates {
            if let Ok(ip) = candidate.trim().parse::<Ipv4Addr>()
                && !ip.is_loopback()
                && !ip.is_link_local()
                && !found.contains(&ip)
            {
                found.push(ip);
            }
        }
    }
    found
}

fn build(root: &Path, args: &Args) -> Res<()> {
    shell::require_tools(&["cargo"])?;
    let step = Step::start("dedicated.build", "iw4l-master (debug)");
    shell::run(
        Command::new("cargo")
            .current_dir(root)
            .args(["build", "-p", "iw4l-master"]),
    )?;
    step.done("");
    let step = Step::start("dedicated.build", "launcher (play profile)");
    shell::run(Command::new("cargo").current_dir(root).args([
        "build",
        "--profile",
        "play",
        "-p",
        "launcher",
    ]))?;
    step.done("");
    if !args.web {
        println!("web build: skipped (--no-web), reusing dist/web");
        return Ok(());
    }
    let mut web_args = vec!["--no-opt".to_string()];
    if args.release_web {
        web_args.push("--release".to_string());
    }
    crate::web::run(root, &web_args)
}

fn exe(dir: &Path, name: &str) -> PathBuf {
    dir.join(format!("{name}{}", std::env::consts::EXE_SUFFIX))
}

pub fn run(root: &Path, env: &Env, args: &[String]) -> Res<()> {
    let Some(args) = parse(env, args)? else {
        print!("{HELP}");
        return Ok(());
    };
    let games = env.require("IW4L_GAMES")?;
    let server_name = env
        .get("IW4L_MASTER_SERVER_NAME")
        .unwrap_or_else(|| Channel::Prod.server_name().to_string());
    if args.build {
        build(root, &args)?;
    }
    let master_src = exe(&root.join("target/debug"), "iw4l-master");
    let host_dir = root.join("target/play");
    let host_exe = args
        .host_exe
        .clone()
        .unwrap_or_else(|| exe(&host_dir, "iw4l"));
    let web_src = root.join("dist/web");
    for (what, path) in [
        ("master binary", &master_src),
        ("host binary", &host_exe),
        ("web bundle", &web_src.join("index.html")),
    ] {
        if !path.is_file() {
            return Err(format!(
                "{what} missing: {} (run without --no-build)",
                path.display()
            ));
        }
    }
    let base = base_dir(root);
    let pack = resolve_pack(args.pack.clone())?;
    let ca = resolve_certs(&base, args.certs.as_deref())?;

    let run_dir = args
        .run_dir
        .clone()
        .or_else(|| env.get("IW4L_RUN_DIR").map(PathBuf::from))
        .unwrap_or_else(|| base.join(timestamp()));
    fs::create_dir_all(&run_dir).map_err(|e| format!("create {}: {e}", run_dir.display()))?;
    let run_dir = std::path::absolute(&run_dir).map_err(|e| e.to_string())?;
    println!("dedicated: run dir {}", run_dir.display());

    // The bundle is copied, not served from dist/web: `cargo xtask web` wipes
    // that directory, and the pack link lives in the copy. The master binary is
    // copied for the same reason (a running .exe cannot be rebuilt on Windows).
    let web = run_dir.join("web");
    copy_tree(&web_src, &web)?;
    let game_pack = web.join("game.pack");
    match fs::hard_link(&pack, &game_pack) {
        Ok(()) => println!("pack: hardlink {}", pack.display()),
        Err(_) => {
            fs::copy(&pack, &game_pack).map_err(|e| format!("copy pack: {e}"))?;
            println!("pack: copied {}", pack.display());
        }
    }
    let master_exe = exe(&run_dir, "iw4l-master");
    fs::copy(&master_src, &master_exe).map_err(|e| format!("copy master: {e}"))?;
    let wt_dir = run_dir.join("wt");
    fs::create_dir_all(&wt_dir).map_err(|e| e.to_string())?;

    install_ctrl_c();
    let mut fleet = Fleet {
        children: Vec::new(),
    };
    let bind = args.bind;
    // `--wt-host` means browsers reach the WebTransport port from the internet,
    // so that one listener is wide open even when `--bind` keeps the rest local.
    let wt_bind = if args.wt_host.is_some() {
        Ipv4Addr::UNSPECIFIED
    } else {
        bind
    };
    let mut master = Command::new(&master_exe);
    master
        .current_dir(&run_dir)
        .arg("serve")
        .args(["--bind", &format!("{bind}:{QUIC_PORT}")])
        .arg("--cert")
        .arg(ca.server_cert())
        .arg("--key")
        .arg(ca.server_key())
        .args(["--webtransport-bind", &format!("{wt_bind}:{WT_PORT}")])
        .arg("--webtransport-dir")
        .arg(&wt_dir)
        .args(["--ws-bind", &format!("{bind}:{WS_PORT}")])
        .arg("--web-root")
        .arg(&web);
    if let Some(host) = &args.wt_host {
        master
            .args(["--webtransport-public-host", host])
            .args(["--webtransport-san", host]);
    }
    println!("master command: {}", describe(&master));
    fleet.spawn("master", &mut master, &run_dir.join("master"))?;

    let ca_cert = std::path::absolute(ca.ca_cert()).map_err(|e| e.to_string())?;
    let mut statuses = Vec::new();
    for i in 1..=args.hosts {
        let host_name = format!("{}-{i}", args.name);
        let status = run_dir.join(format!("host-{i}.status"));
        let _ = fs::remove_file(&status);
        fleet.spawn(
            &format!("host {host_name}"),
            Command::new(&host_exe)
                .current_dir(&host_dir)
                .args(["--no-cheats", "serve", &args.map])
                .env("IW4L_GAMES", &games)
                .env("IW4L_SCRIPT_DVARS", env.get("IW4L_SCRIPT_DVARS").unwrap_or_default())
                .env("IW4L_MASTER_ADDR", format!("127.0.0.1:{QUIC_PORT}"))
                .env("IW4L_MASTER_SERVER_NAME", &server_name)
                .env("IW4L_MASTER_CA_CERT", &ca_cert)
                .env("IW4L_MASTER_HOST_NAME", &host_name)
                .env("IW4L_MASTER_STATUS_FILE", &status)
                .env_remove("IW4L_PRED_LOG")
                .env_remove("IW4L_MASTER_JOIN")
                .env_remove("IW4L_CMDS"),
            &run_dir.join(format!("host-{i}")),
        )?;
        statuses.push((host_name, status));
    }

    let started = Instant::now();
    let deadline = |limit: Duration| started.elapsed() > limit;
    let mut rooms: Vec<Option<String>> = vec![None; statuses.len()];
    println!("dedicated: waiting for the hosts to reach state=hosting");
    while rooms
        .iter()
        .enumerate()
        .any(|(i, room)| room.is_none() && fleet.alive(i + 1))
    {
        if STOP.load(Ordering::SeqCst) || stop_requested(&args) || deadline(READY_TIMEOUT) {
            break;
        }
        if args.exit_after.is_some_and(&deadline) {
            break;
        }
        fleet.reap();
        if !fleet.alive(0) {
            return Err(format!(
                "master exited; see {}",
                run_dir.join("master.err").display()
            ));
        }
        for (i, (_, status)) in statuses.iter().enumerate() {
            if rooms[i].is_some() {
                continue;
            }
            if let Ok(text) = fs::read_to_string(status)
                && field(&text, "state") == Some("hosting")
                && let Some(room) = field(&text, "room")
            {
                rooms[i] = Some(room.to_string());
            }
        }
        std::thread::sleep(Duration::from_millis(250));
    }

    if !STOP.load(Ordering::SeqCst) {
        print_ready(&args, &statuses, &rooms);
    }
    println!(
        "dedicated: running; Ctrl-C stops everything. Logs: {}",
        run_dir.display()
    );
    while !STOP.load(Ordering::SeqCst)
        && !stop_requested(&args)
        && !args.exit_after.is_some_and(&deadline)
    {
        fleet.reap();
        if !fleet.alive(0) {
            println!("dedicated: master exited; stopping");
            break;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    println!("dedicated: shutting down");
    fleet.stop_all();
    Ok(())
}

/// The program and arguments of `cmd`, for the log.
fn describe(cmd: &Command) -> String {
    std::iter::once(cmd.get_program())
        .chain(cmd.get_args())
        .map(|part| part.to_string_lossy())
        .collect::<Vec<_>>()
        .join(" ")
}

fn stop_requested(args: &Args) -> bool {
    args.stop_file.as_deref().is_some_and(Path::exists)
}

fn print_ready(args: &Args, statuses: &[(String, PathBuf)], rooms: &[Option<String>]) {
    println!();
    for ((name, status), room) in statuses.iter().zip(rooms) {
        match room {
            Some(id) => println!("room: name={name} id={id}"),
            None => println!(
                "room: name={name} NOT hosting (see {})",
                status.with_extension("err").display()
            ),
        }
    }
    // Explicit ws: the browser default is now WebTransport in a worker (O19),
    // which needs a secure context and the master's UDP port; ws works anywhere.
    let query = format!("/?map={}&transport=ws", args.map);
    println!();
    if let Some(public) = &args.public_url {
        println!("share this URL:");
        if let Some(wt_host) = &args.wt_host {
            // Bare link: the page follows /master.json to wtw at wt_host.
            println!("  {public}/?map={}", args.map);
            println!(
                "  (an https origin: WebGPU works with no Chrome flag; the page loads through the tunnel, WebTransport goes direct to {wt_host}:{WT_PORT}/udp)"
            );
        } else {
            println!("  {public}{query}");
            println!(
                "  (an https origin: WebGPU works with no Chrome flag; the page's ws origin becomes wss://; transport=ws is explicit as a tunnel cannot reach WebTransport)"
            );
        }
        println!();
    }
    if let Some(wt_host) = &args.wt_host {
        println!(
            "owner steps: router forwards UDP {WT_PORT} to this machine; Windows Firewall inbound UDP {WT_PORT} rule for iw4l-master.exe; DNS-only (not proxied) record {wt_host} -> home IP"
        );
        println!();
    }
    let ips: Vec<Ipv4Addr> = if args.bind.is_unspecified() {
        let mut all = lan_addresses();
        all.push(Ipv4Addr::LOCALHOST);
        all
    } else {
        vec![args.bind]
    };
    println!("page URLs:");
    for ip in &ips {
        println!("  http://{ip}:{WS_PORT}{query}");
    }
    if args.bind.is_unspecified() {
        println!();
        println!("A browser on ANOTHER machine opening http://<lan-ip>:{WS_PORT}/ is not a secure");
        println!(
            "context, and Chrome exposes WebGPU only in secure contexts. Start that Chrome with"
        );
        println!("  --unsafely-treat-insecure-origin-as-secure=http://<lan-ip>:{WS_PORT}");
        println!("or put HTTPS in front (--public-url). Windows Firewall may ask to allow");
        println!("iw4l-master.exe the first time; allow it for private networks.");
    }
    println!();
}
