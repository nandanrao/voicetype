//! Configuration: `$XDG_CONFIG_HOME/voicetype/config.toml`.
//!
//! Every field has a default, so an empty (or missing) file is valid except
//! for the API key, which has to come from somewhere.

use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub stt: Stt,
    pub normalize: Option<Normalize>,
    pub audio: Audio,
    pub typing: Typing,
    pub hooks: Hooks,
    pub log: Log,
    pub notify: Notify,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
#[derive(Default)]
pub enum Provider {
    /// ElevenLabs Scribe: batch REST plus optional realtime WebSocket.
    #[default]
    Elevenlabs,
    /// Any OpenAI-compatible `/audio/transcriptions` endpoint (OpenAI, Groq, …).
    Openai,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Stt {
    pub provider: Provider,
    /// Batch endpoint. For `openai` this is the API base (…/v1); for
    /// `elevenlabs` it is the full speech-to-text URL.
    pub endpoint: String,
    pub model: String,
    pub language: String,
    #[serde(flatten)]
    pub key: KeySource,
    /// ElevenLabs: drop fillers, false starts and non-speech sounds.
    pub no_verbatim: bool,
    /// ElevenLabs: "" (model default), "digits" or "words".
    pub numbers_format: String,
    /// ElevenLabs: spelling hints.
    pub keyterms: Vec<String>,
    /// Seconds allowed for one batch request (plus 1s per 2s of audio).
    pub timeout_secs: u64,
    pub live: Live,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Live {
    /// Stream audio while recording (ElevenLabs only). The batch endpoint is
    /// still used as a fallback whenever the stream fails or comes back empty.
    pub enabled: bool,
    pub endpoint: String,
    pub model: String,
    /// How long to wait for the final transcript after stopping.
    pub finish_timeout_ms: u64,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct KeySource {
    /// Read the key from this file. With `key_var`, the file is an env file
    /// and `key_var=…` is extracted; otherwise the whole file is the key.
    pub key_file: Option<String>,
    pub key_var: Option<String>,
    /// Read the key from this environment variable of the daemon.
    pub key_env: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Normalize {
    /// OpenAI-compatible API base (…/v1); `/chat/completions` is appended.
    pub endpoint: String,
    pub model: String,
    pub prompt: String,
    #[serde(flatten)]
    pub key: KeySource,
    #[serde(default = "default_norm_timeout")]
    pub timeout_secs: u64,
}

fn default_norm_timeout() -> u64 {
    15
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Audio {
    /// PulseAudio/PipeWire source name; empty = default source.
    pub source: String,
    /// Keep capturing this long after stop so the last word isn't clipped.
    pub stop_tail_ms: u64,
    /// Recording stops on its own after this many seconds.
    pub max_seconds: u64,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum TypingMethod {
    /// Virtual-keyboard protocol if the compositor offers it, else wtype,
    /// else ydotool.
    Auto,
    /// zwp_virtual_keyboard_v1, in-process (wlroots compositors, niri, …).
    Wayland,
    Wtype,
    /// uinput via ydotoold; works anywhere but assumes a US layout.
    Ydotool,
    /// Run `typing.command` with the text on stdin.
    Command,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Typing {
    pub method: TypingMethod,
    /// Delay between characters; 0 is fine for almost every app.
    pub key_delay_ms: u64,
    pub command: Vec<String>,
    /// Text typed after every transcript. Defaults to a space so dictations
    /// that follow each other don't run together; "" turns it off.
    pub append: String,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct Hooks {
    /// Run (argv, no shell) when recording starts, e.g. to enter a
    /// compositor mode where Escape cancels.
    pub on_recording: Vec<String>,
    /// Run when recording ends (stopped, cancelled or failed).
    pub on_idle: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Notify {
    /// Longest popup body, in characters. Longer transcripts show "…" and
    /// their end, so the newest words stay visible. Match it to what your
    /// notification daemon's popup size can show.
    pub max_chars: usize,
}

impl Default for Notify {
    fn default() -> Self {
        Notify { max_chars: 300 }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Log {
    /// Per-phase timing rows; empty disables.
    pub timing: String,
    /// Keep audio of recordings that could not be transcribed.
    pub keep_failed_audio: bool,
}

impl Default for Stt {
    fn default() -> Self {
        Stt {
            provider: Provider::Elevenlabs,
            endpoint: String::new(),
            model: String::new(),
            language: "en".into(),
            key: KeySource::default(),
            no_verbatim: true,
            numbers_format: String::new(),
            keyterms: Vec::new(),
            timeout_secs: 20,
            live: Live::default(),
        }
    }
}

impl Default for Live {
    fn default() -> Self {
        Live {
            enabled: true,
            endpoint: "wss://api.elevenlabs.io/v1/speech-to-text/realtime".into(),
            model: "scribe_v3_live".into(),
            finish_timeout_ms: 4000,
        }
    }
}

impl Default for Audio {
    fn default() -> Self {
        Audio {
            source: String::new(),
            stop_tail_ms: 300,
            max_seconds: 600,
        }
    }
}

impl Default for Typing {
    fn default() -> Self {
        Typing {
            method: TypingMethod::Auto,
            key_delay_ms: 0,
            command: Vec::new(),
            append: " ".into(),
        }
    }
}

impl Default for Log {
    fn default() -> Self {
        Log {
            timing: state_dir()
                .join("timing.log")
                .to_string_lossy()
                .into_owned(),
            keep_failed_audio: true,
        }
    }
}

impl Config {
    pub fn path() -> PathBuf {
        config_dir().join("voicetype").join("config.toml")
    }

    pub fn load() -> Result<Config> {
        let path = Self::path();
        let mut cfg: Config = match std::fs::read_to_string(&path) {
            Ok(text) => {
                toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Config::default(),
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        cfg.fill_defaults();
        cfg.validate()?;
        Ok(cfg)
    }

    fn fill_defaults(&mut self) {
        let s = &mut self.stt;
        match s.provider {
            Provider::Elevenlabs => {
                if s.endpoint.is_empty() {
                    s.endpoint = "https://api.elevenlabs.io/v1/speech-to-text".into();
                }
                if s.model.is_empty() {
                    s.model = "scribe_v3".into();
                }
            }
            Provider::Openai => {
                if s.endpoint.is_empty() {
                    s.endpoint = "https://api.openai.com/v1".into();
                }
                if s.model.is_empty() {
                    s.model = "whisper-1".into();
                }
                s.live.enabled = false;
            }
        }
    }

    fn validate(&self) -> Result<()> {
        if self.typing.method == TypingMethod::Command && self.typing.command.is_empty() {
            bail!("typing.method = \"command\" needs typing.command");
        }
        Ok(())
    }

    pub fn timing_log(&self) -> Option<PathBuf> {
        (!self.log.timing.is_empty()).then(|| expand(&self.log.timing))
    }
}

impl KeySource {
    /// Read the key fresh on every use, so rotating it needs no restart.
    pub fn read(&self) -> Result<String> {
        if let Some(var) = &self.key_env {
            if let Ok(v) = std::env::var(var)
                && !v.trim().is_empty()
            {
                return Ok(v.trim().to_string());
            }
            if self.key_file.is_none() {
                bail!("API key variable {var} is not set");
            }
        }
        let file = self
            .key_file
            .as_deref()
            .ok_or_else(|| anyhow!("no API key configured (key_file or key_env)"))?;
        let path = expand(file);
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("reading key file {}", path.display()))?;
        let key = match &self.key_var {
            Some(var) => env_file_value(&text, var)
                .ok_or_else(|| anyhow!("{var} not found in {}", path.display()))?,
            None => text.trim().to_string(),
        };
        if key.is_empty() {
            bail!("empty API key in {}", path.display());
        }
        Ok(key)
    }
}

/// `VAR=value` from an env file, tolerating `export`, quotes and a trailing
/// ` # comment`.
fn env_file_value(text: &str, var: &str) -> Option<String> {
    for line in text.lines() {
        let line = line.trim();
        let line = line.strip_prefix("export ").unwrap_or(line).trim_start();
        let Some(rest) = line.strip_prefix(var) else {
            continue;
        };
        let Some(value) = rest.trim_start().strip_prefix('=') else {
            continue;
        };
        let value = value.trim();
        let value = if let Some(q) = value.chars().next().filter(|c| *c == '"' || *c == '\'') {
            let inner = &value[1..];
            inner.split(q).next().unwrap_or("")
        } else {
            value.split(" #").next().unwrap_or("").trim()
        };
        return Some(value.to_string());
    }
    None
}

/// Expand a leading `~/` and `$HOME`/`${HOME}`.
pub fn expand(p: &str) -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_default();
    let p = p.replace("${HOME}", &home).replace("$HOME", &home);
    match p.strip_prefix("~/") {
        Some(rest) => Path::new(&home).join(rest),
        None => PathBuf::from(p),
    }
}

fn xdg(var: &str, fallback: &str) -> PathBuf {
    match std::env::var_os(var) {
        Some(v) if !v.is_empty() => PathBuf::from(v),
        _ => expand(fallback),
    }
}

pub fn config_dir() -> PathBuf {
    xdg("XDG_CONFIG_HOME", "~/.config")
}

pub fn state_dir() -> PathBuf {
    xdg("XDG_STATE_HOME", "~/.local/state").join("voicetype")
}

pub fn runtime_dir() -> PathBuf {
    match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(v) if !v.is_empty() => PathBuf::from(v),
        _ => std::env::temp_dir().join(format!("voicetype-{}", rustix::process::getuid().as_raw())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_values() {
        let f = "A=1\nexport KEY=\"abc\" # c\nB='x y'\nC=plain # note\n";
        assert_eq!(env_file_value(f, "KEY").as_deref(), Some("abc"));
        assert_eq!(env_file_value(f, "B").as_deref(), Some("x y"));
        assert_eq!(env_file_value(f, "C").as_deref(), Some("plain"));
        assert_eq!(env_file_value(f, "KE"), None);
    }

    #[test]
    fn empty_config_parses() {
        let mut c: Config = toml::from_str("").unwrap();
        c.fill_defaults();
        assert_eq!(c.stt.model, "scribe_v3");
    }
}
