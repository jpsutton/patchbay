//! patchbayd: Patchbay without a window. It runs chats, Code sessions and the mesh like the
//! desktop app, for phones and other computers in the mesh to drive, and (built with the `web`
//! feature) serves the Patchbay UI to a browser.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};

const USAGE: &str = "\
Usage: patchbayd [options]
       patchbayd token [--rotate] [options]
       patchbayd agents [options]
       patchbayd mesh <status|create|join|approve|reject|leave> [options]

Runs Patchbay headless: chats, Code sessions and the mesh, without a window.

Options:
  --data-dir <dir>   Data folder (default: the desktop app's, e.g. ~/.local/share/dev.patchbay.app)
  --listen <addr>    Web UI address (default: 127.0.0.1:4420)
  --ui-dir <dir>     Built frontend to serve (default: found next to the binary)
  --no-web           Don't serve the web UI
  -V, --version      Show the version
  -h, --help         Show this help

Commands:
  token              Print the web UI's address with its access token
  token --rotate     Replace the token (open tabs must sign in again)
  agents             Show the subagents of Patchbay's agent sessions: state, idle time, what each is doing
  mesh               Manage this node's mesh membership through the running daemon (see below)

Mesh commands (the daemon must be running):
  mesh status                       Mesh membership, members and requests waiting for approval
  mesh create --name <name>         Start a new mesh with this node as its first member
  mesh join <address> --name <name> Ask to join the mesh of the computer at <address>
  mesh approve <node>               Admit a waiting node (its id, an id prefix or its name)
  mesh reject <node>                Turn a waiting node away
  mesh leave                        Leave the mesh
  The passphrase of create and join comes from PATCHBAY_MESH_PASSPHRASE, or from the first line
  of stdin with --passphrase-stdin.

Environment:
  PATCHBAY_LOG       Log filter (default: info)
";

#[derive(Default)]
struct Args {
    data_dir: Option<PathBuf>,
    listen: Option<String>,
    ui_dir: Option<PathBuf>,
    no_web: bool,
    token: bool,
    rotate: bool,
    agents: bool,
    /// Words after `mesh`: the subcommand, then its arguments.
    mesh: Option<Vec<String>>,
    name: Option<String>,
    passphrase_stdin: bool,
}

fn parse_args() -> Result<Args> {
    let mut a = Args::default();
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        let mut value = |name: &str| it.next().with_context(|| format!("{name} needs a value"));
        match arg.as_str() {
            "--data-dir" => a.data_dir = Some(value("--data-dir")?.into()),
            "--listen" => a.listen = Some(value("--listen")?),
            "--ui-dir" => a.ui_dir = Some(value("--ui-dir")?.into()),
            "--no-web" => a.no_web = true,
            "--rotate" => a.rotate = true,
            "token" => a.token = true,
            "agents" => a.agents = true,
            "mesh" if a.mesh.is_none() => a.mesh = Some(Vec::new()),
            "--name" => a.name = Some(value("--name")?),
            "--passphrase-stdin" => a.passphrase_stdin = true,
            word if a.mesh.is_some() && !word.starts_with('-') => a.mesh.as_mut().unwrap().push(word.to_owned()),
            "-h" | "--help" => {
                print!("{USAGE}");
                std::process::exit(0);
            }
            "-V" | "--version" => {
                println!("patchbayd {}", env!("CARGO_PKG_VERSION"));
                std::process::exit(0);
            }
            other => bail!("unknown argument {other}\n\n{USAGE}"),
        }
    }
    if a.rotate && !a.token {
        bail!("--rotate goes with the token command");
    }
    if a.mesh.is_none() && (a.name.is_some() || a.passphrase_stdin) {
        bail!("--name and --passphrase-stdin go with the mesh command");
    }
    Ok(a)
}

/// The web UI's access token, created on first use (readable by this user only).
fn load_token(data_dir: &std::path::Path, rotate: bool) -> Result<String> {
    let path = data_dir.join("web-token");
    if !rotate
        && let Ok(t) = std::fs::read_to_string(&path)
        && !t.trim().is_empty()
    {
        return Ok(t.trim().to_owned());
    }
    std::fs::create_dir_all(data_dir)?;
    let token = hex::encode(rand::random::<[u8; 24]>());
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
    std::io::Write::write_all(&mut opts.open(&path)?, token.as_bytes())?;
    Ok(token)
}

#[cfg_attr(not(feature = "web"), allow(dead_code))]
fn find_ui_dir(given: Option<PathBuf>) -> Option<PathBuf> {
    let has_index = |p: &PathBuf| p.join("index.html").is_file();
    if let Some(p) = given.or_else(|| std::env::var_os("PATCHBAY_WEB_DIR").map(PathBuf::from)) {
        return Some(p);
    }
    let exe_dir = std::env::current_exe().ok().and_then(|e| e.parent().map(PathBuf::from));
    let mut candidates = Vec::new();
    if let Some(d) = &exe_dir {
        candidates.push(d.join("web"));
        candidates.push(d.join("../share/patchbay/web"));
        candidates.push(d.join("../lib/patchbay/web"));
    }
    candidates.push(PathBuf::from("/usr/share/patchbay/web"));
    // A dev build, run from the source tree.
    candidates.push(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../dist"));
    candidates.into_iter().find(has_index)
}

/// `45s`, `3m 05s`, `1h 02m`.
fn span(secs: u64) -> String {
    match secs {
        0..=59 => format!("{secs}s"),
        60..=3599 => format!("{}m {:02}s", secs / 60, secs % 60),
        _ => format!("{}h {:02}m", secs / 3600, secs % 3600 / 60),
    }
}

/// What the agent wrote to `agent/subagents/<session>/status.json`: one block per session. It
/// needs no running daemon, so it also works when the app is stuck or gone; the file's age says
/// which. Idle time is counted on from when the file was written.
fn print_agents(data_dir: &std::path::Path) -> Result<()> {
    let root = data_dir.join("agent").join("subagents");
    let mut sessions: Vec<_> = std::fs::read_dir(&root).map(|d| d.flatten().map(|e| e.path()).collect()).unwrap_or_default();
    sessions.sort();
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_millis() as u64;
    let mut any = false;
    for dir in sessions {
        let Ok(text) = std::fs::read_to_string(dir.join("status.json")) else { continue };
        let Ok(doc) = serde_json::from_str::<serde_json::Value>(&text) else { continue };
        let Some(agents) = doc["agents"].as_array().filter(|a| !a.is_empty()) else { continue };
        any = true;
        let age = now.saturating_sub(doc["updated_at_ms"].as_u64().unwrap_or(0)) / 1000;
        let running = agents.iter().any(|a| a["status"] == "running");
        let id = dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let note = match (running, age > 60) {
            (true, true) => ", not updated since: the app may have exited or hung",
            _ => "",
        };
        println!("session {id} (state written {} ago{note})", span(age));
        for a in agents {
            let s = |k: &str| a[k].as_str().unwrap_or("").to_owned();
            let n = |k: &str| a[k].as_u64().unwrap_or(0);
            let name = a["name"].as_str().map(|n| format!(" \"{n}\"")).unwrap_or_default();
            if a["status"] == "running" {
                println!(
                    "  {}{name} running {}, idle {}, {} steps: {}  [{}]",
                    s("id"),
                    span(n("elapsed_secs") + age),
                    span(n("idle_secs") + age),
                    n("steps"),
                    s("current"),
                    s("description")
                );
            } else {
                let err = a["error"].as_str().map(|e| format!(": {}", e.lines().next().unwrap_or(""))).unwrap_or_default();
                println!("  {}{name} {}{err}  [{}]", s("id"), s("status"), s("description"));
            }
        }
    }
    if !any {
        println!("No subagent state under {}", root.display());
    }
    Ok(())
}

/// `patchbayd mesh ...`: the mesh commands of Settings → Mesh, run on the daemon that owns the
/// data folder (through the socket the desktop app attaches by).
async fn mesh_cli(words: &[String], args: &Args, data_dir: &std::path::Path) -> Result<()> {
    use patchbay_lib::api::ChanFactory;
    use patchbay_lib::ipc::client::{AttachError, Conn};
    use serde_json::{Value, json};

    let conn = Conn::open(data_dir, std::sync::Arc::new(|_, _| {}), || {}).await.map_err(|e| match e {
        AttachError::Unreachable(_) => anyhow::anyhow!(
            "patchbayd isn't running on {}; start it first (for example `systemctl --user start patchbayd`)",
            data_dir.display()
        ),
        e => anyhow::anyhow!("{e}"),
    })?;
    let no_chans: ChanFactory = std::sync::Arc::new(|_| None);
    let call = |cmd: &'static str, a: Value| {
        let (conn, no_chans) = (conn.clone(), no_chans.clone());
        async move { conn.call(cmd, a, &no_chans).await.map_err(|e| anyhow::anyhow!("{e}")) }
    };
    let name = || args.name.clone().context("--name <this node's name> is required");
    let passphrase = || -> Result<String> {
        let p = if args.passphrase_stdin {
            let mut line = String::new();
            std::io::stdin().read_line(&mut line)?;
            line.trim_end_matches(['\r', '\n']).to_owned()
        } else {
            std::env::var("PATCHBAY_MESH_PASSPHRASE").unwrap_or_default()
        };
        if p.is_empty() {
            bail!("no passphrase: set PATCHBAY_MESH_PASSPHRASE, or pass --passphrase-stdin and write it to stdin");
        }
        Ok(p)
    };
    // The node a waiting request names: its id, a prefix of it, or its name.
    let waiting = |status: &Value, who: &str| -> Result<String> {
        let found: Vec<&Value> = status["pending"]
            .as_array()
            .map(|a| a.iter().filter(|p| p["nodeId"].as_str().is_some_and(|id| id.starts_with(who)) || p["name"] == who).collect())
            .unwrap_or_default();
        match found.as_slice() {
            [one] => Ok(one["nodeId"].as_str().unwrap_or_default().to_owned()),
            [] => bail!("no waiting request matches {who}; see `patchbayd mesh status`"),
            _ => bail!("{who} matches more than one waiting request; use more of the node id"),
        }
    };

    let sub = words.first().map(String::as_str).unwrap_or("status");
    let rest = &words[words.len().min(1)..];
    match (sub, rest) {
        ("status", []) => print_mesh(&call("mesh_status", json!({})).await?),
        ("create", []) => {
            call("mesh_create", json!({ "passphrase": passphrase()?, "name": name()? })).await?;
            println!("Created a mesh. Other computers join with this node's address and the passphrase.");
            print_mesh(&call("mesh_status", json!({})).await?);
        }
        ("join", [address]) => {
            call("mesh_join", json!({ "url": address, "passphrase": passphrase()?, "name": name()? })).await?;
            // The daemon keeps joining after this returns; wait for the code a member must check.
            for _ in 0..60 {
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                let s = call("mesh_status", json!({})).await?;
                let j = &s["joining"];
                let err = j["error"].as_str().unwrap_or("");
                match j["status"].as_str() {
                    Some("waiting") => {
                        println!(
                            "Waiting for approval. On a computer in the mesh, check that this code matches, then approve:\n  code: {}\n  node: {}",
                            j["code"].as_str().unwrap_or("?"),
                            s["nodeId"].as_str().unwrap_or("?")
                        );
                        println!("Approve in Settings → Mesh, or with `patchbayd mesh approve {}` there.", s["name"].as_str().unwrap_or("<name>"));
                        return Ok(());
                    }
                    Some("rejected") => bail!("the request was declined{}", if err.is_empty() { String::new() } else { format!(": {err}") }),
                    Some("failed") => bail!("{}", if err.is_empty() { "couldn't join" } else { err }),
                    _ if s["member"] == true => {
                        println!("Joined the mesh.");
                        return Ok(());
                    }
                    _ => {}
                }
            }
            bail!("still connecting to {address}; check `patchbayd mesh status`, and that the address is reachable");
        }
        ("approve" | "reject", [who]) => {
            let s = call("mesh_status", json!({})).await?;
            let id = waiting(&s, who.as_str())?;
            call(if sub == "approve" { "mesh_approve" } else { "mesh_reject" }, json!({ "nodeId": id })).await?;
            println!("{} {id}", if sub == "approve" { "Approved" } else { "Declined" });
        }
        ("leave", []) => {
            call("mesh_leave", json!({})).await?;
            println!("Left the mesh.");
        }
        _ => bail!("unknown mesh command\n\n{USAGE}"),
    }
    Ok(())
}

fn print_mesh(s: &serde_json::Value) {
    let text = |v: &serde_json::Value| v.as_str().unwrap_or("-").to_owned();
    println!("node {} ({})", text(&s["name"]), text(&s["nodeId"]));
    if let Some(p) = s["problem"].as_object() {
        println!("problem: {}", p.get("message").and_then(|m| m.as_str()).unwrap_or("unknown"));
    }
    if s["member"] != true {
        match s["joining"]["status"].as_str() {
            Some(st) => println!("not in a mesh yet: joining {} ({st}, code {})", text(&s["joining"]["url"]), text(&s["joining"]["code"])),
            None => println!("not in a mesh"),
        }
    } else {
        println!("mesh {}, listening on {}", text(&s["meshId"]), text(&s["listen"]));
        for m in s["members"].as_array().into_iter().flatten() {
            println!("  {}  {}  {}  {}", text(&m["nodeId"]), text(&m["name"]), text(&m["state"]), text(&m["role"]));
        }
    }
    for p in s["pending"].as_array().into_iter().flatten() {
        println!(
            "waiting to join: {} ({}), code {}, connected to {}",
            text(&p["name"]),
            text(&p["nodeId"]),
            text(&p["code"]),
            text(&p["hostName"])
        );
    }
}

fn url_for(listen: &str, token: &str) -> String {
    let shown = listen.replace("0.0.0.0", "127.0.0.1").replace("[::]", "[::1]");
    format!("http://{shown}/#token={token}")
}

fn main() -> Result<()> {
    if std::env::args().nth(1).as_deref() == Some("--agent-messaging-mcp") {
        return patchbay_lib::agent_messaging::run_stdio();
    }
    let args = parse_args()?;
    let data_dir = match args.data_dir.clone() {
        Some(d) => d,
        None => patchbay_lib::default_data_dir()?,
    };
    let listen = args.listen.clone().unwrap_or_else(|| "127.0.0.1:4420".into());

    if args.agents {
        return print_agents(&data_dir);
    }
    if args.token {
        let token = load_token(&data_dir, args.rotate)?;
        println!("{}", url_for(&listen, &token));
        return Ok(());
    }

    if let Some(words) = &args.mesh {
        return tokio::runtime::Runtime::new()?.block_on(mesh_cli(words, &args, &data_dir));
    }

    patchbay_lib::init_tracing();
    let rt = tokio::runtime::Runtime::new()?;
    let _ = patchbay_lib::api::RUNTIME.set(rt.handle().clone());
    rt.block_on(run(args, data_dir, listen))
}

async fn run(args: Args, data_dir: PathBuf, listen: String) -> Result<()> {
    // App events for browser tabs and an attached desktop app.
    let events: patchbay_lib::ipc::server::Events = tokio::sync::broadcast::channel(4096).0;
    let emit = patchbay_lib::ipc::server::ui_sink(events.clone());

    // Servers rarely run a keyring, so API keys may be stored in the database: a folder the
    // daemon creates is readable by this user only.
    if !data_dir.exists() {
        std::fs::create_dir_all(&data_dir)?;
        #[cfg(unix)]
        std::fs::set_permissions(&data_dir, std::os::unix::fs::PermissionsExt::from_mode(0o700))?;
    }
    let state = patchbay_lib::open_state(data_dir.clone(), emit)?;
    tracing::info!("patchbayd running on {}", data_dir.display());
    // A desktop app started on the same folder attaches here and uses this daemon as its backend.
    if let Err(e) = patchbay_lib::ipc::server::start(state.clone(), events.clone(), &data_dir) {
        tracing::warn!("the desktop app can't attach to this daemon: {e:#}");
    }

    #[cfg(feature = "web")]
    let server = if args.no_web {
        // The web UI (and its /share route) is off, but shares still need a server: start the
        // standalone loopback one from the same registry (shares.rs).
        patchbay_lib::shares::spawn(&state);
        None
    } else {
        let addr: std::net::SocketAddr = listen.parse().with_context(|| format!("bad --listen address {listen}"))?;
        let ui_dir = find_ui_dir(args.ui_dir.clone())
            .context("built web UI not found; pass --ui-dir <dist> or --no-web")?;
        if !addr.ip().is_loopback() {
            tracing::warn!(
                "the web UI listens on {addr}, beyond this computer, over plain HTTP. Anyone who gets the token can run \
                 commands as this user; prefer the default loopback address and an SSH tunnel."
            );
        }
        let token = load_token(&data_dir, false)?;
        let cfg = patchbay_lib::web::Config { listen: addr, ui_dir, token: token.clone() };
        let state = state.clone();
        let task = tokio::spawn(patchbay_lib::web::serve(state, events.clone(), cfg));
        println!("Patchbay web UI: {}", url_for(&listen, &token));
        Some(task)
    };
    #[cfg(not(feature = "web"))]
    {
        let _ = (&listen, args.no_web, &args.ui_dir);
        tracing::info!("built without the web UI; drive this node from the mesh");
    }

    #[cfg(feature = "web")]
    let result = match server {
        Some(task) => tokio::select! {
            r = task => r.map_err(anyhow::Error::from).and_then(|r| r),
            _ = shutdown_signal() => Ok(()),
        },
        None => {
            shutdown_signal().await;
            Ok(())
        }
    };
    #[cfg(not(feature = "web"))]
    let result = {
        shutdown_signal().await;
        Ok(())
    };

    tracing::info!("shutting down");
    patchbay_lib::ipc::server::cleanup(&data_dir);
    state.shutdown();
    result
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut term = signal(SignalKind::terminate()).expect("SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    let _ = tokio::signal::ctrl_c().await;
}
