//! The daemon owns all state. Clients send one command per connection; the
//! main loop handles them strictly in order, so two quick key presses can
//! never race. Each dictation is a session task that owns its own audio,
//! stream and popup, so a new recording can start while the previous one is
//! still being transcribed or typed.

use crate::audio::{self, AudioMsg};
use crate::config::{self, Config, Provider};
use crate::notify::Popup;
use crate::stt;
use crate::typer::{self, Job};
use crate::util::log_timing;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::os::fd::AsFd;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::time::{Instant, sleep_until, timeout};

pub type Env = HashMap<String, String>;

#[derive(Serialize, Deserialize)]
pub struct Request {
    pub cmd: String,
    #[serde(default)]
    pub arg: Option<String>,
    /// The client's environment: compositor sockets can change when the
    /// session restarts, and the daemon always uses the latest.
    #[serde(default)]
    pub env: Env,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct Response {
    pub ok: bool,
    pub state: String,
    pub message: String,
}

pub fn socket_path() -> PathBuf {
    config::runtime_dir().join("voicetype.sock")
}

fn lock_path() -> PathBuf {
    config::runtime_dir().join("voicetype.lock")
}

enum Ctl {
    Stop,
    Cancel,
}

struct Current {
    seq: u64,
    ctl: mpsc::UnboundedSender<Ctl>,
}

struct Shared {
    http: reqwest::Client,
    typer: std::sync::mpsc::Sender<Job>,
    hooks: mpsc::UnboundedSender<(Vec<String>, Env)>,
    events: mpsc::UnboundedSender<u64>,
    in_flight: AtomicUsize,
}

pub async fn run() -> Result<()> {
    let dir = config::runtime_dir();
    std::fs::create_dir_all(&dir).ok();
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(lock_path())
        .context("opening lock file")?;
    if rustix::fs::flock(
        lock.as_fd(),
        rustix::fs::FlockOperation::NonBlockingLockExclusive,
    )
    .is_err()
    {
        bail!("another voicetype daemon is already running");
    }
    let sock = socket_path();
    let _ = std::fs::remove_file(&sock);
    let listener =
        UnixListener::bind(&sock).with_context(|| format!("binding {}", sock.display()))?;
    std::fs::set_permissions(&sock, std::fs::Permissions::from_mode(0o600)).ok();

    crate::log!("daemon started (pid {})", std::process::id());
    // A broken config must not keep the daemon from starting: it is
    // reloaded (and reported) at the start of every recording anyway.
    let mut cfg = Config::load().unwrap_or_else(|e| {
        crate::log!("config error: {e:#}");
        Config::default()
    });

    let (events_tx, mut events_rx) = mpsc::unbounded_channel::<u64>();
    let shared = Arc::new(Shared {
        http: stt::http_client(),
        typer: typer::spawn(),
        hooks: spawn_hook_runner(),
        events: events_tx,
        in_flight: AtomicUsize::new(0),
    });
    let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<(Request, oneshot::Sender<Response>)>();
    tokio::spawn(accept_loop(listener, cmd_tx));

    let mut env: Env = std::env::vars().collect();
    let mut current: Option<Current> = None;
    let mut next_seq: u64 = 1;
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;

    loop {
        tokio::select! {
            Some((req, reply)) = cmd_rx.recv() => {
                if !req.env.is_empty() {
                    env = req.env.clone();
                }
                let recording = current.is_some();
                let (ok, message) = match (req.cmd.as_str(), recording) {
                    ("toggle" | "start", false) => {
                        // Pick up config edits without a restart.
                        match Config::load() {
                            Ok(c) => cfg = c,
                            Err(e) => {
                                crate::log!("config error, keeping previous: {e:#}");
                                Popup::new().show("voicetype · config error", format!("{e:#}"), 8000);
                            }
                        }
                        let seq = next_seq;
                        next_seq += 1;
                        let (ctl_tx, ctl_rx) = mpsc::unbounded_channel();
                        current = Some(Current { seq, ctl: ctl_tx });
                        shared.in_flight.fetch_add(1, Ordering::SeqCst);
                        tokio::spawn(session(seq, cfg.clone(), env.clone(), ctl_rx, shared.clone()));
                        hook(&shared, &cfg.hooks.on_recording, &env);
                        (true, "recording".to_string())
                    }
                    ("start", true) => (true, "already recording".into()),
                    ("toggle" | "stop", true) => {
                        let cur = current.take().unwrap();
                        let _ = cur.ctl.send(Ctl::Stop);
                        hook(&shared, &cfg.hooks.on_idle, &env);
                        (true, "stopped".into())
                    }
                    ("cancel", true) => {
                        let cur = current.take().unwrap();
                        let _ = cur.ctl.send(Ctl::Cancel);
                        hook(&shared, &cfg.hooks.on_idle, &env);
                        (true, "cancelled".into())
                    }
                    ("stop" | "cancel", false) => {
                        // Nothing to stop, but make sure the compositor isn't
                        // left in a recording mode.
                        hook(&shared, &cfg.hooks.on_idle, &env);
                        (true, "not recording".into())
                    }
                    ("status", _) => (true, format!(
                        "{} dictation(s) in progress",
                        shared.in_flight.load(Ordering::SeqCst)
                    )),
                    ("type", _) => {
                        let text = req.arg.clone().unwrap_or_default();
                        let seq = next_seq;
                        next_seq += 1;
                        let _ = shared.typer.send(Job {
                            seq, text: Some(text), popup: None, env: env.clone(),
                            cfg: cfg.typing.clone(), done: None,
                        });
                        (true, "queued".into())
                    }
                    ("quit", _) => {
                        let _ = reply.send(Response { ok: true, state: state(&current), message: "bye".into() });
                        if let Some(cur) = current.take() {
                            let _ = cur.ctl.send(Ctl::Cancel);
                            hook(&shared, &cfg.hooks.on_idle, &env);
                        }
                        tokio::time::sleep(Duration::from_millis(300)).await;
                        break;
                    }
                    (other, _) => (false, format!("unknown command: {other}")),
                };
                let _ = reply.send(Response { ok, state: state(&current), message });
            }
            Some(seq) = events_rx.recv() => {
                // A session stopped recording by itself (time limit, mic failure).
                if current.as_ref().is_some_and(|c| c.seq == seq) {
                    current = None;
                    hook(&shared, &cfg.hooks.on_idle, &env);
                }
            }
            _ = term.recv() => {
                if let Some(cur) = current.take() {
                    let _ = cur.ctl.send(Ctl::Cancel);
                    hook(&shared, &cfg.hooks.on_idle, &env);
                    tokio::time::sleep(Duration::from_millis(300)).await;
                }
                break;
            }
        }
    }
    let _ = std::fs::remove_file(&sock);
    crate::log!("daemon exiting");
    Ok(())
}

fn state(current: &Option<Current>) -> String {
    if current.is_some() {
        "recording"
    } else {
        "idle"
    }
    .into()
}

async fn accept_loop(
    listener: UnixListener,
    cmd_tx: mpsc::UnboundedSender<(Request, oneshot::Sender<Response>)>,
) {
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            continue;
        };
        let cmd_tx = cmd_tx.clone();
        tokio::spawn(async move {
            let (rd, mut wr) = stream.into_split();
            let mut line = String::new();
            let read = timeout(
                Duration::from_secs(2),
                BufReader::new(rd).read_line(&mut line),
            )
            .await;
            let resp = match read
                .ok()
                .and_then(|r| r.ok())
                .and_then(|_| serde_json::from_str::<Request>(&line).ok())
            {
                Some(req) => {
                    let (tx, rx) = oneshot::channel();
                    let _ = cmd_tx.send((req, tx));
                    match rx.await {
                        Ok(r) => r,
                        Err(_) => return,
                    }
                }
                None => Response {
                    ok: false,
                    state: String::new(),
                    message: "bad request".into(),
                },
            };
            let mut out = serde_json::to_string(&resp).unwrap_or_default();
            out.push('\n');
            let _ = timeout(Duration::from_secs(2), wr.write_all(out.as_bytes())).await;
        });
    }
}

/// Hooks run one at a time, in order, each with a time limit, so a slow
/// hook can't reorder "recording" and "idle".
fn spawn_hook_runner() -> mpsc::UnboundedSender<(Vec<String>, Env)> {
    let (tx, mut rx) = mpsc::unbounded_channel::<(Vec<String>, Env)>();
    tokio::spawn(async move {
        while let Some((argv, env)) = rx.recv().await {
            let Some((prog, args)) = argv.split_first() else {
                continue;
            };
            let child = tokio::process::Command::new(prog)
                .args(args)
                .env_clear()
                .envs(&env)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .kill_on_drop(true)
                .spawn();
            match child {
                Ok(mut c) => {
                    if timeout(Duration::from_secs(3), c.wait()).await.is_err() {
                        crate::log!("hook timed out: {argv:?}");
                    }
                }
                Err(e) => crate::log!("hook failed to start {argv:?}: {e}"),
            }
        }
    });
    tx
}

fn hook(shared: &Shared, argv: &[String], env: &Env) {
    if !argv.is_empty() {
        let _ = shared.hooks.send((argv.to_vec(), env.clone()));
    }
}

/// Guarantees the typer hears about every dictation exactly once, even when
/// the session bails out early, so later dictations are never held back.
struct Slot {
    seq: u64,
    shared: Arc<Shared>,
    env: Env,
    cfg: config::Typing,
    done: bool,
}

impl Slot {
    fn fill(mut self, text: String, popup: (Popup, String), done: typer::DoneFn) {
        self.done = true;
        let _ = self.shared.typer.send(Job {
            seq: self.seq,
            text: Some(text),
            popup: Some(popup),
            env: std::mem::take(&mut self.env),
            cfg: self.cfg.clone(),
            done: Some(done),
        });
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        if !self.done {
            let _ = self.shared.typer.send(Job {
                seq: self.seq,
                text: None,
                popup: None,
                env: Env::new(),
                cfg: self.cfg.clone(),
                done: None,
            });
        }
        self.shared.in_flight.fetch_sub(1, Ordering::SeqCst);
    }
}

fn provider_label(cfg: &Config, live: bool) -> String {
    match cfg.stt.provider {
        Provider::Elevenlabs if live => format!("ElevenLabs {}", cfg.stt.live.model),
        Provider::Elevenlabs => format!("ElevenLabs {}", cfg.stt.model),
        Provider::Openai => cfg.stt.model.clone(),
    }
}

enum End {
    Stop,
    Cancel,
    Limit,
    MicFailed(String),
}

async fn session(
    seq: u64,
    cfg: Config,
    env: Env,
    mut ctl: mpsc::UnboundedReceiver<Ctl>,
    shared: Arc<Shared>,
) {
    let slot = Slot {
        seq,
        shared: shared.clone(),
        env: env.clone(),
        cfg: cfg.typing.clone(),
        done: false,
    };
    let timing = cfg.timing_log();
    let live_on = cfg.stt.provider == Provider::Elevenlabs && cfg.stt.live.enabled;
    let label = provider_label(&cfg, live_on);
    let title = if live_on {
        format!("voicetype · {label} · live")
    } else {
        format!("voicetype · {label}")
    };
    let popup = Popup::new();
    popup.show(&title, "Recording…", 0);

    let stop = Arc::new(AtomicBool::new(false));
    let (audio_tx, mut audio_rx) = mpsc::unbounded_channel();
    let source = Some(cfg.audio.source.clone()).filter(|s| !s.is_empty());
    audio::spawn(source, stop.clone(), audio_tx);

    let (partial_tx, mut partial_rx) = watch::channel(String::new());
    let mut live =
        live_on.then(|| stt::live_start(cfg.stt.clone(), shared.http.clone(), partial_tx));
    let watcher = {
        let popup = popup.clone();
        let title = title.clone();
        tokio::spawn(async move {
            while partial_rx.changed().await.is_ok() {
                let text = partial_rx.borrow_and_update().clone();
                if !text.is_empty() {
                    popup.show(&title, text, 0);
                }
                tokio::time::sleep(Duration::from_millis(150)).await;
            }
        })
    };

    let t_start = Instant::now();
    let limit = t_start + Duration::from_secs(cfg.audio.max_seconds.max(1));
    let mic_check = t_start + Duration::from_secs(3);
    let mut checked = false;
    let mut pcm: Vec<u8> = Vec::new();
    let feed = |pcm: &mut Vec<u8>, chunk: Vec<u8>, live: &Option<stt::LiveSession>| {
        pcm.extend_from_slice(&chunk);
        if let Some(l) = live {
            let _ = l.tx.send(stt::LiveIn::Audio(chunk));
        }
    };

    let end = loop {
        tokio::select! {
            c = ctl.recv() => break match c {
                Some(Ctl::Cancel) => End::Cancel,
                _ => End::Stop,
            },
            m = audio_rx.recv() => match m {
                Some(AudioMsg::Started) => {}
                Some(AudioMsg::Chunk(c)) => feed(&mut pcm, c, &live),
                Some(AudioMsg::Failed(e)) => break End::MicFailed(e),
                None => break End::MicFailed("capture stopped".into()),
            },
            _ = sleep_until(limit) => break End::Limit,
            _ = sleep_until(mic_check), if !checked => {
                checked = true;
                if pcm.is_empty() {
                    popup.show(&title, "No audio from the microphone yet…", 0);
                } else if audio::peak(&pcm) == 0 {
                    popup.show(&title, "Microphone is sending pure silence (muted, or stuck after resume?)", 0);
                }
            }
        }
    };

    match &end {
        End::Cancel => {
            stop.store(true, Ordering::Relaxed);
            if let Some(l) = live.take() {
                l.task.abort();
            }
            watcher.abort();
            popup.show(&title, "Cancelled", 1500);
            return;
        }
        End::Limit | End::MicFailed(_) => {
            // The daemon still thinks we're recording; tell it we're not.
            let _ = shared.events.send(seq);
        }
        End::Stop => {}
    }
    if let End::MicFailed(e) = &end {
        crate::log!("dictation {seq}: microphone: {e}");
        if pcm.is_empty() {
            stop.store(true, Ordering::Relaxed);
            if let Some(l) = live.take() {
                l.task.abort();
            }
            watcher.abort();
            popup.show("voicetype · microphone error", e.clone(), 8000);
            return;
        }
    }

    // Keep capturing briefly so the last word isn't lost to key latency.
    if matches!(end, End::Stop) {
        let tail = Instant::now() + Duration::from_millis(cfg.audio.stop_tail_ms);
        loop {
            tokio::select! {
                m = audio_rx.recv() => match m {
                    Some(AudioMsg::Chunk(c)) => feed(&mut pcm, c, &live),
                    Some(_) => {}
                    None => break,
                },
                _ = sleep_until(tail) => break,
            }
        }
    }
    stop.store(true, Ordering::Relaxed);
    let drain_until = Instant::now() + Duration::from_millis(500);
    loop {
        tokio::select! {
            m = audio_rx.recv() => match m {
                Some(AudioMsg::Chunk(c)) => feed(&mut pcm, c, &live),
                Some(_) => {}
                None => break,
            },
            _ = sleep_until(drain_until) => break,
        }
    }
    let t_stop = Instant::now();
    let secs = audio::seconds(&pcm);
    log_timing(
        timing.as_deref(),
        "record",
        (t_stop - t_start).as_millis(),
        &format!("audio={secs:.1}s"),
    );
    popup.show(&title, "Finishing…", 0);

    // --- speech to text: live result first, batch as the fallback ---
    let mut text = String::new();
    let mut note;
    let mut error: Option<String> = None;
    if let Some(mut l) = live.take() {
        let _ = l.tx.send(stt::LiveIn::Finish);
        let wait = Duration::from_millis(cfg.stt.live.finish_timeout_ms);
        note = match timeout(wait, &mut l.task).await {
            Ok(Ok(Ok(t))) if !t.trim().is_empty() => {
                text = t.trim().to_string();
                format!("stream {}", cfg.stt.live.model)
            }
            Ok(Ok(Ok(_))) => "stream empty".into(),
            Ok(Ok(Err(e))) => format!("stream failed: {e:#}"),
            Ok(Err(_)) => "stream task died".into(),
            Err(_) => "stream timed out".into(),
        };
        l.task.abort();
        if text.is_empty() {
            crate::log!("dictation {seq}: {note}; falling back to batch");
        }
    } else {
        note = String::new();
    }
    watcher.abort();

    if text.is_empty() && secs >= 0.3 {
        let wav = audio::wav(&pcm);
        match stt::batch(&shared.http, &cfg.stt, &wav, secs).await {
            Ok(t) => {
                text = t;
                note = format!(
                    "{}batch {}",
                    if note.is_empty() {
                        String::new()
                    } else {
                        format!("{note}; ")
                    },
                    cfg.stt.model
                );
            }
            Err(e) => {
                crate::log!("dictation {seq}: batch failed: {e:#}");
                error = Some(format!("{e:#}"));
                note = format!("{note}; batch failed");
            }
        }
    }
    let t_stt = Instant::now();
    log_timing(
        timing.as_deref(),
        "stt",
        (t_stt - t_stop).as_millis(),
        &format!("words={} {note}", text.split_whitespace().count()),
    );

    if text.is_empty() {
        match error {
            Some(e) => {
                let saved = if cfg.log.keep_failed_audio {
                    save_failed(&pcm)
                } else {
                    None
                };
                let body = match saved {
                    Some(p) => format!(
                        "{}\n\nAudio kept: {}\nRetry: voicetype transcribe {}",
                        stt::truncate(&e, 200),
                        p.display(),
                        p.display()
                    ),
                    None => stt::truncate(&e, 200),
                };
                popup.show("voicetype · transcription failed", body, 20000);
            }
            None => popup.show(&title, "No speech detected", 2500),
        }
        return;
    }

    if let Some(n) = &cfg.normalize {
        popup.show(&title, format!("Cleaning up…\n{text}"), 0);
        let (out, how) = stt::normalize(&shared.http, n, &text).await;
        log_timing(
            timing.as_deref(),
            "normalize",
            t_stt.elapsed().as_millis(),
            &how,
        );
        text = out;
    }

    popup.show(&title, text.clone(), 0);
    let t_ready = Instant::now();
    let total_note = format!("words={} {note}", text.split_whitespace().count());
    let done = Box::new(move |r: Result<&'static str, String>| {
        let now = Instant::now();
        match r {
            Ok(how) => log_timing(timing.as_deref(), "type", (now - t_ready).as_millis(), how),
            Err(e) => log_timing(
                timing.as_deref(),
                "type",
                (now - t_ready).as_millis(),
                &format!("failed: {e}"),
            ),
        }
        log_timing(
            timing.as_deref(),
            "total",
            (now - t_stop).as_millis(),
            &total_note,
        );
    });
    slot.fill(text, (popup, label), done);
}

fn save_failed(pcm: &[u8]) -> Option<PathBuf> {
    let dir = config::state_dir().join("failed");
    std::fs::create_dir_all(&dir).ok()?;
    let path = dir.join(format!("{}.wav", crate::util::timestamp().replace(':', "")));
    std::fs::write(&path, audio::wav(pcm)).ok()?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).ok();
    Some(path)
}
