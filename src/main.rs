//! voicetype: dictation for Wayland desktops.
//!
//! `voicetype toggle` (bound to a key) talks to a small daemon that records,
//! transcribes and types into the focused window. The daemon starts on demand.

mod audio;
mod config;
mod daemon;
mod notify;
mod stt;
mod typer;
mod util;

use anyhow::{Context, Result, anyhow, bail};
use daemon::{Request, Response};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const USAGE: &str = "\
voicetype — dictation for Wayland

usage: voicetype <command>

  toggle            start recording, or stop and type the transcript
  start | stop      for push-to-talk (bind stop to the key release)
  cancel            discard the current recording
  status            show what the daemon is doing
  type TEXT         type TEXT through the daemon (tests the typing backend)
  transcribe FILE   transcribe an audio file with the batch API and print it
  test              check config, API key, microphone, typing and notifications
  timings           show the timing log
  daemon            run the daemon in the foreground (normally auto-started)
  quit              stop the daemon

config: ~/.config/voicetype/config.toml";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = args.first().map(String::as_str).unwrap_or("toggle");
    let result = match cmd {
        "toggle" | "start" | "stop" | "cancel" | "status" | "quit" => client(cmd, None),
        "type" => match args.get(1) {
            Some(t) => client("type", Some(t.clone())),
            None => Err(anyhow!("usage: voicetype type TEXT")),
        },
        "daemon" => run_daemon(),
        "transcribe" => match args.get(1) {
            Some(f) => transcribe(f),
            None => Err(anyhow!("usage: voicetype transcribe FILE")),
        },
        "test" => self_test(),
        "timings" => timings(),
        "-h" | "--help" | "help" => {
            println!("{USAGE}");
            Ok(())
        }
        "-V" | "--version" => {
            println!("voicetype {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        other => Err(anyhow!("unknown command {other:?}\n\n{USAGE}")),
    };
    if let Err(e) = result {
        eprintln!("voicetype: {e:#}");
        std::process::exit(1);
    }
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("tokio runtime")
}

fn run_daemon() -> Result<()> {
    runtime().block_on(daemon::run())
}

fn request(req: &Request) -> Result<Response> {
    let mut s = UnixStream::connect(daemon::socket_path())?;
    s.set_read_timeout(Some(Duration::from_secs(5)))?;
    s.set_write_timeout(Some(Duration::from_secs(2)))?;
    let mut line = serde_json::to_string(req)?;
    line.push('\n');
    s.write_all(line.as_bytes())?;
    let mut resp = String::new();
    BufReader::new(s).read_line(&mut resp)?;
    serde_json::from_str(&resp).context("bad response from daemon")
}

/// Send one command, starting the daemon first if it isn't running.
fn client(cmd: &str, arg: Option<String>) -> Result<()> {
    let req = Request {
        cmd: cmd.into(),
        arg,
        env: std::env::vars().collect(),
    };
    let resp = match request(&req) {
        Ok(r) => r,
        Err(_) if cmd == "quit" || cmd == "status" => {
            println!("daemon not running");
            return Ok(());
        }
        Err(_) => {
            spawn_daemon()?;
            let deadline = Instant::now() + Duration::from_secs(3);
            loop {
                match request(&req) {
                    Ok(r) => break r,
                    Err(e) if Instant::now() > deadline => {
                        fail_notify(&format!("daemon did not start: {e}"));
                        bail!("daemon did not start: {e} (see {})", daemon_log().display());
                    }
                    Err(_) => std::thread::sleep(Duration::from_millis(50)),
                }
            }
        }
    };
    if !resp.ok {
        bail!("{}", resp.message);
    }
    if cmd == "status" {
        println!("{} — {}", resp.state, resp.message);
    }
    Ok(())
}

fn daemon_log() -> std::path::PathBuf {
    config::state_dir().join("daemon.log")
}

fn spawn_daemon() -> Result<()> {
    let log = daemon_log();
    std::fs::create_dir_all(log.parent().unwrap()).ok();
    let err = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log)?;
    Command::new(std::env::current_exe()?)
        .arg("daemon")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(err)
        .process_group(0)
        .spawn()
        .context("starting daemon")?;
    Ok(())
}

/// Last resort when even the daemon can't start: tell the user anyway.
fn fail_notify(msg: &str) {
    let _ = Command::new("notify-send")
        .args(["-t", "8000", "voicetype", msg])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
}

fn transcribe(path: &str) -> Result<()> {
    let cfg = config::Config::load()?;
    let data = std::fs::read(path).with_context(|| format!("reading {path}"))?;
    // Rough duration for the timeout budget; exact for our own WAVs.
    let secs = data.len().saturating_sub(44) as f64 / (audio::SAMPLE_RATE as f64 * 2.0);
    let text = runtime().block_on(stt::batch(&stt::http_client(), &cfg.stt, &data, secs))?;
    println!("{text}");
    Ok(())
}

fn timings() -> Result<()> {
    let cfg = config::Config::load()?;
    let path = cfg
        .timing_log()
        .ok_or_else(|| anyhow!("timing log disabled"))?;
    match std::fs::read_to_string(&path) {
        Ok(t) => print!("{t}"),
        Err(_) => println!("no timings yet: {}", path.display()),
    }
    Ok(())
}

fn self_test() -> Result<()> {
    let mark = |ok: bool| if ok { "ok  " } else { "FAIL" };
    println!("config   {}", config::Config::path().display());
    let cfg = match config::Config::load() {
        Ok(c) => {
            println!("  {}  parsed", mark(true));
            c
        }
        Err(e) => {
            println!("  {}  {e:#}", mark(false));
            return Ok(());
        }
    };
    println!(
        "stt      {:?} {} ({})",
        cfg.stt.provider, cfg.stt.model, cfg.stt.endpoint
    );
    let key = cfg.stt.key.read();
    println!(
        "  {}  API key {}",
        mark(key.is_ok()),
        key.as_ref()
            .err()
            .map(|e| format!("{e:#}"))
            .unwrap_or_else(|| "readable".into())
    );
    if cfg.stt.live.enabled && cfg.stt.provider == config::Provider::Elevenlabs {
        println!("  live   {}", cfg.stt.live.model);
    }

    println!(
        "audio    {}",
        if cfg.audio.source.is_empty() {
            "default source"
        } else {
            &cfg.audio.source
        }
    );
    let rt = runtime();
    let mic = rt.block_on(async {
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        audio::spawn(
            Some(cfg.audio.source.clone()).filter(|s| !s.is_empty()),
            stop.clone(),
            tx,
        );
        let mut pcm = Vec::new();
        let deadline = tokio::time::Instant::now() + Duration::from_millis(700);
        let res = loop {
            tokio::select! {
                m = rx.recv() => match m {
                    Some(audio::AudioMsg::Chunk(c)) => pcm.extend(c),
                    Some(audio::AudioMsg::Failed(e)) => break Err(e),
                    Some(_) => {}
                    None => break Err("capture ended".into()),
                },
                _ = tokio::time::sleep_until(deadline) => break Ok(()),
            }
        };
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        res.map(|_| (audio::seconds(&pcm), audio::peak(&pcm)))
    });
    match mic {
        Ok((s, peak)) => println!(
            "  {}  captured {s:.1}s, peak level {peak}{}",
            mark(s > 0.0 && peak > 0),
            if peak == 0 {
                " (silence: muted or broken mic?)"
            } else {
                ""
            }
        ),
        Err(e) => println!("  {}  {e}", mark(false)),
    }

    println!("typing   {:?}", cfg.typing.method);
    let wl = std::env::var("WAYLAND_DISPLAY").unwrap_or_default();
    println!(
        "  {}  WAYLAND_DISPLAY={}",
        mark(!wl.is_empty()),
        if wl.is_empty() { "unset" } else { &wl }
    );
    for tool in ["wtype", "ydotool"] {
        let found = Command::new("sh")
            .args(["-c", &format!("command -v {tool}")])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        println!(
            "  {}  {tool}{}",
            if found { "ok  " } else { "--  " },
            if found {
                ""
            } else {
                " not installed (optional)"
            }
        );
    }
    println!("daemon   {}", daemon::socket_path().display());
    let req = Request {
        cmd: "status".into(),
        arg: None,
        env: Default::default(),
    };
    match request(&req) {
        Ok(r) => println!("  ok    {} — {}", r.state, r.message),
        Err(_) => println!("  --    not running (starts on first use)"),
    }
    println!("log      {}", daemon_log().display());
    Ok(())
}
