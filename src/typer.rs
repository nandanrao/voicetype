//! Typing transcripts into the focused window.
//!
//! One thread owns typing, so two dictations can never type interleaved,
//! and transcripts are typed in the order their recordings started even
//! when a later one finishes transcribing first.

use crate::config::{Typing, TypingMethod};
use crate::notify::Popup;
use anyhow::{Context, Result, anyhow, bail};
use std::collections::{BTreeMap, HashMap};
use std::io::Write as _;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// Called with how typing went, for the timing log.
pub type DoneFn = Box<dyn FnOnce(Result<&'static str, String>) + Send>;

pub struct Job {
    pub seq: u64,
    /// None: this dictation produced nothing to type (cancelled, failed, silent).
    pub text: Option<String>,
    pub popup: Option<(Popup, String)>,
    pub env: HashMap<String, String>,
    pub cfg: Typing,
    /// Called with how typing went, for the timing log.
    pub done: Option<DoneFn>,
}

/// If an earlier dictation never reports (it shouldn't: every stage has a
/// timeout), stop holding later ones back after this long.
const STALL_LIMIT: Duration = Duration::from_secs(120);

pub fn spawn() -> mpsc::Sender<Job> {
    let (tx, rx) = mpsc::channel::<Job>();
    std::thread::Builder::new()
        .name("typer".into())
        .spawn(move || run(rx))
        .expect("spawning typer thread");
    tx
}

fn run(rx: mpsc::Receiver<Job>) {
    let mut next: u64 = 1;
    let mut waiting: BTreeMap<u64, Job> = BTreeMap::new();
    let mut stalled_since: Option<Instant> = None;
    loop {
        match rx.recv_timeout(Duration::from_secs(5)) {
            Ok(job) => {
                if job.seq < next {
                    // Arrived after we gave up waiting for it; still type it.
                    type_job(job);
                } else {
                    waiting.insert(job.seq, job);
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
        }
        while let Some(job) = waiting.remove(&next) {
            next += 1;
            stalled_since = None;
            type_job(job);
        }
        if let Some((&first, _)) = waiting.first_key_value() {
            let since = *stalled_since.get_or_insert_with(Instant::now);
            if since.elapsed() > STALL_LIMIT {
                crate::log!("typer: dictation {next} never finished; skipping to {first}");
                next = first;
                stalled_since = None;
            }
        }
    }
}

fn type_job(job: Job) {
    let Some(text) = job.text else { return };
    let mut text = text;
    text.push_str(&job.cfg.append);
    let result = type_text(&text, &job.cfg, &job.env);
    match &result {
        Ok(_) => {
            if let Some((p, label)) = &job.popup {
                p.show(format!("voicetype · typed with {label}"), text.trim(), 5000);
            }
        }
        Err(e) => {
            crate::log!("typing failed: {e:#}");
            if let Some((p, _)) = &job.popup {
                p.show(
                    "voicetype · typing failed",
                    format!("{e:#}\n\n{}", text.trim()),
                    15000,
                );
            }
        }
    }
    if let Some(done) = job.done {
        done(result.map_err(|e| format!("{e:#}")));
    }
}

fn type_text(text: &str, cfg: &Typing, env: &HashMap<String, String>) -> Result<&'static str> {
    match cfg.method {
        TypingMethod::Wayland => wayland::type_text(text, cfg.key_delay_ms, env).map(|_| "wayland"),
        TypingMethod::Wtype => run_tool(
            &["wtype", "-d", &cfg.key_delay_ms.to_string(), "--", text],
            None,
            env,
        )
        .map(|_| "wtype"),
        TypingMethod::Ydotool => {
            run_tool(&["ydotool", "type", "--", text], None, env).map(|_| "ydotool")
        }
        TypingMethod::Command => {
            let argv: Vec<&str> = cfg.command.iter().map(String::as_str).collect();
            run_tool(&argv, Some(text), env).map(|_| "command")
        }
        TypingMethod::Auto => match wayland::type_text(text, cfg.key_delay_ms, env) {
            Ok(()) => Ok("wayland"),
            Err(e) if e.downcast_ref::<wayland::Unsupported>().is_some() => {
                crate::log!("virtual keyboard unavailable ({e}); using ydotool");
                run_tool(&["ydotool", "type", "--", text], None, env).map(|_| "ydotool")
            }
            Err(e) => Err(e),
        },
    }
}

/// Run a helper with the client's environment and a time limit that scales
/// with the text length.
fn run_tool(argv: &[&str], stdin: Option<&str>, env: &HashMap<String, String>) -> Result<()> {
    let (prog, args) = argv
        .split_first()
        .ok_or_else(|| anyhow!("empty typing command"))?;
    let len = stdin
        .map(str::len)
        .unwrap_or_else(|| argv.iter().map(|a| a.len()).sum());
    let limit = Duration::from_secs(10) + Duration::from_millis(20 * len as u64);
    let mut child = Command::new(prog)
        .args(args)
        .env_clear()
        .envs(env)
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("running {prog}"))?;
    if let (Some(text), Some(mut pipe)) = (stdin, child.stdin.take()) {
        pipe.write_all(text.as_bytes()).ok();
    }
    let start = Instant::now();
    loop {
        if let Some(status) = child.try_wait()? {
            if status.success() {
                return Ok(());
            }
            let mut err = String::new();
            if let Some(mut e) = child.stderr.take() {
                use std::io::Read;
                e.read_to_string(&mut err).ok();
            }
            bail!("{prog} failed ({status}): {}", err.trim());
        }
        if start.elapsed() > limit {
            let _ = child.kill();
            let _ = child.wait();
            bail!("{prog} timed out");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Path of the compositor socket named by the client's environment.
fn wayland_socket(env: &HashMap<String, String>) -> Result<PathBuf> {
    let display = env
        .get("WAYLAND_DISPLAY")
        .filter(|d| !d.is_empty())
        .ok_or_else(|| anyhow!("WAYLAND_DISPLAY is not set"))?;
    let path = PathBuf::from(display);
    if path.is_absolute() {
        return Ok(path);
    }
    let runtime = env
        .get("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(crate::config::runtime_dir);
    Ok(runtime.join(path))
}

mod wayland {
    //! zwp_virtual_keyboard_v1: upload a keymap containing exactly the
    //! characters we need, then press each one. Independent of the user's
    //! layout, so any Unicode text types correctly.

    use super::*;
    use std::os::fd::AsFd;
    use wayland_client::globals::{GlobalListContents, registry_queue_init};
    use wayland_client::protocol::{wl_registry, wl_seat::WlSeat};
    use wayland_client::{Connection, Dispatch, QueueHandle, delegate_noop};
    use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::{
        zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1,
        zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1,
    };

    #[derive(Debug)]
    pub struct Unsupported(pub String);
    impl std::fmt::Display for Unsupported {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str(&self.0)
        }
    }
    impl std::error::Error for Unsupported {}

    struct State;
    impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
        fn event(
            _: &mut Self,
            _: &wl_registry::WlRegistry,
            _: wl_registry::Event,
            _: &GlobalListContents,
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
        }
    }
    delegate_noop!(State: ignore WlSeat);
    delegate_noop!(State: ZwpVirtualKeyboardManagerV1);
    delegate_noop!(State: ZwpVirtualKeyboardV1);

    /// XWayland only sees keycodes up to 255, so a keymap holds at most this
    /// many distinct characters; longer texts switch keymaps between parts.
    const MAX_KEYS: usize = 200;
    /// Let the compositor catch up every so many characters.
    const SYNC_EVERY: usize = 32;

    pub fn type_text(text: &str, delay_ms: u64, env: &HashMap<String, String>) -> Result<()> {
        let keysyms: Vec<u32> = text.chars().filter_map(keysym).collect();
        if keysyms.is_empty() {
            return Ok(());
        }
        let socket = wayland_socket(env)?;
        let stream = UnixStream::connect(&socket)
            .with_context(|| format!("connecting to {}", socket.display()))?;
        let conn = Connection::from_socket(stream).context("wayland connection")?;
        let (globals, mut queue) =
            registry_queue_init::<State>(&conn).context("wayland registry")?;
        let qh = queue.handle();
        let seat: WlSeat = globals
            .bind(&qh, 1..=1, ())
            .map_err(|_| Unsupported("compositor has no seat".into()))?;
        let mgr: ZwpVirtualKeyboardManagerV1 = globals
            .bind(&qh, 1..=1, ())
            .map_err(|_| Unsupported("compositor lacks zwp_virtual_keyboard_v1".into()))?;
        let kb = mgr.create_virtual_keyboard(&seat, &qh, ());
        let mut st = State;
        let start = Instant::now();
        let now = || start.elapsed().as_millis() as u32;

        let mut rest: &[u32] = &keysyms;
        while !rest.is_empty() {
            // Take the longest prefix with at most MAX_KEYS distinct keysyms.
            let mut codes: HashMap<u32, u32> = HashMap::new();
            let mut order: Vec<u32> = Vec::new();
            let mut n = 0;
            for &ks in rest {
                if let std::collections::hash_map::Entry::Vacant(e) = codes.entry(ks) {
                    if order.len() == MAX_KEYS {
                        break;
                    }
                    order.push(ks);
                    e.insert(order.len() as u32);
                }
                n += 1;
            }
            let (part, tail) = rest.split_at(n);
            rest = tail;

            let keymap = keymap_text(&order);
            let fd = rustix::fs::memfd_create("voicetype-keymap", rustix::fs::MemfdFlags::CLOEXEC)?;
            let mut file = std::fs::File::from(fd);
            file.write_all(keymap.as_bytes())?;
            file.write_all(&[0])?;
            kb.keymap(1 /* xkb_v1 */, file.as_fd(), keymap.len() as u32 + 1);
            kb.modifiers(0, 0, 0, 0);
            queue.roundtrip(&mut st).context("uploading keymap")?;

            for (i, ks) in part.iter().enumerate() {
                let code = codes[ks];
                kb.key(now(), code, 1);
                kb.key(now(), code, 0);
                if delay_ms > 0 {
                    conn.flush().ok();
                    std::thread::sleep(Duration::from_millis(delay_ms));
                }
                if (i + 1) % SYNC_EVERY == 0 {
                    queue.roundtrip(&mut st).context("typing")?;
                }
            }
            queue.roundtrip(&mut st).context("typing")?;
        }
        kb.destroy();
        queue.roundtrip(&mut st).ok();
        Ok(())
    }

    fn keysym(c: char) -> Option<u32> {
        match c {
            '\n' => Some(0xff0d), // Return
            '\t' => Some(0xff09), // Tab
            '\r' => None,
            c if c.is_control() => None,
            c => Some(xkeysym::Keysym::from_char(c).raw()).filter(|&k| k != 0),
        }
    }

    /// Keycode i (1-based, evdev numbering) is xkb keycode i+8.
    fn keymap_text(keysyms: &[u32]) -> String {
        let mut s = String::from("xkb_keymap {\nxkb_keycodes \"voicetype\" {\nminimum = 8;\n");
        s += &format!("maximum = {};\n", keysyms.len() + 9);
        for i in 1..=keysyms.len() {
            s += &format!("<K{i}> = {};\n", i + 8);
        }
        s += "};\nxkb_types \"voicetype\" { include \"complete\" };\n";
        s += "xkb_compatibility \"voicetype\" { include \"complete\" };\n";
        s += "xkb_symbols \"voicetype\" {\n";
        for (i, ks) in keysyms.iter().enumerate() {
            s += &format!("key <K{}> {{[ 0x{ks:08x} ]}};\n", i + 1);
        }
        s += "};\n};\n";
        s
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        #[test]
        fn keysyms() {
            assert_eq!(keysym('a'), Some(0x61));
            assert_eq!(keysym('é'), Some(0xe9));
            assert_eq!(keysym('\n'), Some(0xff0d));
            assert_eq!(keysym('\u{7}'), None);
            assert!(keysym('😀').unwrap() >= 0x0100_0000);
        }
    }
}
