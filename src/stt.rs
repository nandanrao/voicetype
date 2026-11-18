//! Speech-to-text: batch requests and the ElevenLabs realtime stream.
//! Every network operation here is bounded by a timeout.

use crate::config::{Normalize, Provider, Stt};
use anyhow::{Context, Result, anyhow, bail};
use base64::Engine as _;
use futures_util::{SinkExt, StreamExt};
use reqwest::multipart::{Form, Part};
use serde_json::{Value, json};
use std::time::Duration;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tokio::time::{Instant, sleep_until, timeout};
use tokio_tungstenite::tungstenite::{self, Message, client::IntoClientRequest};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

pub fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .user_agent(concat!("voicetype/", env!("CARGO_PKG_VERSION")))
        .build()
        .expect("building HTTP client")
}

/// Transcribe a WAV. Retries once on network errors, 429 and 5xx.
pub async fn batch(
    http: &reqwest::Client,
    cfg: &Stt,
    wav: &[u8],
    audio_secs: f64,
) -> Result<String> {
    let budget = Duration::from_secs(cfg.timeout_secs) + Duration::from_secs_f64(audio_secs / 2.0);
    let mut last_err = anyhow!("no attempt made");
    for attempt in 0..2 {
        if attempt > 0 {
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        match batch_once(http, cfg, wav, budget).await {
            Ok(text) => return Ok(text),
            Err(BatchError::Fatal(e)) => return Err(e),
            Err(BatchError::Retry(e)) => last_err = e,
        }
    }
    Err(last_err)
}

enum BatchError {
    Retry(anyhow::Error),
    Fatal(anyhow::Error),
}

async fn batch_once(
    http: &reqwest::Client,
    cfg: &Stt,
    wav: &[u8],
    budget: Duration,
) -> std::result::Result<String, BatchError> {
    let key = cfg.key.read().map_err(BatchError::Fatal)?;
    let file = Part::bytes(wav.to_vec())
        .file_name("audio.wav")
        .mime_str("audio/wav")
        .map_err(|e| BatchError::Fatal(e.into()))?;
    let req = match cfg.provider {
        Provider::Elevenlabs => {
            let mut form = Form::new()
                .part("file", file)
                .text("model_id", cfg.model.clone());
            if !cfg.language.is_empty() {
                form = form.text("language_code", cfg.language.clone());
            }
            form = form.text("no_verbatim", cfg.no_verbatim.to_string());
            if !cfg.numbers_format.is_empty() {
                form = form.text("numbers_format", cfg.numbers_format.clone());
            }
            for term in &cfg.keyterms {
                form = form.text("keyterms", term.clone());
            }
            http.post(&cfg.endpoint)
                .header("xi-api-key", key)
                .multipart(form)
        }
        Provider::Openai => {
            let mut form = Form::new()
                .part("file", file)
                .text("model", cfg.model.clone())
                .text("response_format", "json");
            if !cfg.language.is_empty() {
                form = form.text("language", cfg.language.clone());
            }
            let url = format!(
                "{}/audio/transcriptions",
                cfg.endpoint.trim_end_matches('/')
            );
            http.post(url).bearer_auth(key).multipart(form)
        }
    };
    let resp = req
        .timeout(budget)
        .send()
        .await
        .map_err(|e| BatchError::Retry(anyhow!("STT request failed: {}", plain(e))))?;
    let status = resp.status();
    let body = resp
        .text()
        .await
        .map_err(|e| BatchError::Retry(anyhow!("STT response failed: {}", plain(e))))?;
    if !status.is_success() {
        let err = anyhow!("STT HTTP {}: {}", status.as_u16(), truncate(&body, 300));
        return Err(if status.as_u16() == 429 || status.is_server_error() {
            BatchError::Retry(err)
        } else {
            BatchError::Fatal(err)
        });
    }
    let v: Value = serde_json::from_str(&body)
        .map_err(|e| BatchError::Fatal(anyhow!("STT returned invalid JSON: {e}")))?;
    Ok(v["text"].as_str().unwrap_or("").trim().to_string())
}

/// Optional LLM cleanup pass. Returns the original text whenever the model
/// fails or its answer drifts too far from the transcript.
pub async fn normalize(http: &reqwest::Client, cfg: &Normalize, text: &str) -> (String, String) {
    match normalize_inner(http, cfg, text).await {
        Ok(out) if norm_is_safe(text, &out) => (out, "used".into()),
        Ok(_) => (text.to_string(), "fallback (diverged)".into()),
        Err(e) => (text.to_string(), format!("failed: {e}")),
    }
}

async fn normalize_inner(http: &reqwest::Client, cfg: &Normalize, text: &str) -> Result<String> {
    let key = cfg.key.read()?;
    let max_tokens = (text.split_whitespace().count() * 4 + 64).min(4096);
    let body = json!({
        "model": cfg.model,
        "temperature": 0,
        "max_tokens": max_tokens,
        "messages": [
            {"role": "system", "content": cfg.prompt},
            {"role": "user", "content": format!("<transcript>\n{text}\n</transcript>")},
        ],
    });
    let url = format!("{}/chat/completions", cfg.endpoint.trim_end_matches('/'));
    let resp = http
        .post(url)
        .bearer_auth(key)
        .json(&body)
        .timeout(Duration::from_secs(cfg.timeout_secs))
        .send()
        .await
        .map_err(|e| anyhow!("{}", plain(e)))?;
    let status = resp.status();
    let v: Value = resp.json().await.map_err(|e| anyhow!("{}", plain(e)))?;
    if !status.is_success() {
        bail!("HTTP {}", status.as_u16());
    }
    let out = v["choices"][0]["message"]["content"]
        .as_str()
        .unwrap_or("")
        .trim()
        .to_string();
    if out.is_empty() {
        bail!("empty answer");
    }
    Ok(out)
}

/// A cleanup pass shouldn't change the word count much. If the model
/// answered, refused or summarized, the length moves a lot.
fn norm_is_safe(raw: &str, out: &str) -> bool {
    let rw = raw.split_whitespace().count();
    let ow = out.split_whitespace().count();
    rw == 0 || (ow <= rw * 3 / 2 + 2 && ow >= rw / 2)
}

// ---------------------------------------------------------------------------
// Realtime stream (ElevenLabs Scribe realtime)

pub enum LiveIn {
    Audio(Vec<u8>),
    /// No more audio: commit and wait for the final transcript.
    Finish,
}

pub struct LiveSession {
    pub tx: mpsc::UnboundedSender<LiveIn>,
    pub task: JoinHandle<Result<String>>,
}

/// Open the stream in the background. Audio sent before the session is up
/// is queued, so starting never waits on the network. `partial` receives
/// the text so far as it grows.
pub fn live_start(cfg: Stt, http: reqwest::Client, partial: watch::Sender<String>) -> LiveSession {
    let (tx, rx) = mpsc::unbounded_channel();
    let task = tokio::spawn(async move { live_run(cfg, http, rx, partial).await });
    LiveSession { tx, task }
}

async fn live_run(
    cfg: Stt,
    http: reqwest::Client,
    mut rx: mpsc::UnboundedReceiver<LiveIn>,
    partial_out: watch::Sender<String>,
) -> Result<String> {
    let key = cfg.key.read()?;
    let ws = live_connect(&cfg, &http, &key).await?;
    let (mut sink, mut stream) = ws.split();

    let mut committed = String::new();
    let mut partial = String::new();
    let mut started = false;
    let mut finishing = false;
    let mut queued: Vec<Vec<u8>> = Vec::new();
    // After the commit, give trailing segments a moment to land.
    let mut grace: Option<Instant> = None;

    loop {
        tokio::select! {
            m = rx.recv(), if !finishing => match m {
                Some(LiveIn::Audio(chunk)) => {
                    if started {
                        sink.send(audio_msg(&chunk, false)).await.context("stream send")?;
                    } else {
                        queued.push(chunk);
                    }
                }
                Some(LiveIn::Finish) | None => {
                    finishing = true;
                    if started {
                        sink.send(audio_msg(&[], true)).await.context("stream commit")?;
                    }
                }
            },
            msg = stream.next() => {
                let msg = match msg {
                    Some(Ok(m)) => m,
                    Some(Err(e)) => bail!("stream error: {e}"),
                    None => break,
                };
                let text = match msg {
                    Message::Text(t) => t,
                    Message::Close(_) => break,
                    _ => continue,
                };
                let Ok(v) = serde_json::from_str::<Value>(text.as_str()) else { continue };
                let kind = v["message_type"].as_str().unwrap_or("");
                match kind {
                    "session_started" => {
                        started = true;
                        for chunk in queued.drain(..) {
                            sink.send(audio_msg(&chunk, false)).await.context("stream send")?;
                        }
                        if finishing {
                            sink.send(audio_msg(&[], true)).await.context("stream commit")?;
                        }
                    }
                    "partial_transcript" => {
                        partial = v["text"].as_str().unwrap_or("").to_string();
                        let _ = partial_out.send(join(&committed, &partial));
                    }
                    "committed_transcript" => {
                        committed = join(&committed, v["text"].as_str().unwrap_or(""));
                        partial.clear();
                        let _ = partial_out.send(committed.clone());
                        if finishing {
                            grace = Some(Instant::now() + Duration::from_millis(400));
                        }
                    }
                    k if k.contains("error") || is_fatal(k) => {
                        let detail = v["error"].as_str().or(v["message"].as_str()).unwrap_or(k);
                        bail!("stream: {detail}");
                    }
                    _ => {}
                }
            }
            _ = async { sleep_until(grace.unwrap()).await }, if grace.is_some() => break,
        }
    }
    let _ = sink.close().await;
    if !finishing {
        bail!("stream closed before recording ended");
    }
    Ok(join(&committed, &partial))
}

fn is_fatal(kind: &str) -> bool {
    matches!(
        kind,
        "quota_exceeded"
            | "rate_limited"
            | "unaccepted_terms"
            | "invalid_request"
            | "chunk_size_exceeded"
            | "insufficient_audio_activity"
            | "session_time_limit_exceeded"
            | "resource_exhausted"
            | "queue_overflow"
            | "commit_throttled"
    )
}

type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// Authenticate with the API key header; if that is refused, fall back to a
/// single-use token in the URL.
async fn live_connect(cfg: &Stt, http: &reqwest::Client, key: &str) -> Result<Ws> {
    let mut url = reqwest::Url::parse(&cfg.live.endpoint).context("live endpoint")?;
    {
        let mut q = url.query_pairs_mut();
        q.append_pair("model_id", &cfg.live.model)
            .append_pair("audio_format", "pcm_16000")
            .append_pair("commit_strategy", "manual");
        if cfg.no_verbatim {
            q.append_pair("no_verbatim", "true");
        }
        if !cfg.language.is_empty() {
            q.append_pair("language_code", &cfg.language);
        }
        for term in &cfg.keyterms {
            q.append_pair("keyterms", term);
        }
    }
    let mut req = url.as_str().into_client_request()?;
    req.headers_mut()
        .insert("xi-api-key", key.parse().context("API key header")?);
    match timeout(CONNECT_TIMEOUT, tokio_tungstenite::connect_async(req)).await {
        Err(_) => bail!("stream connect timed out"),
        Ok(Ok((ws, _))) => return Ok(ws),
        Ok(Err(tungstenite::Error::Http(r))) if matches!(r.status().as_u16(), 401 | 403) => {}
        Ok(Err(e)) => bail!("stream connect failed: {e}"),
    }

    let token_url = format!(
        "{}/v1/single-use-token/realtime_scribe",
        "https://api.elevenlabs.io"
    );
    let v: Value = http
        .post(token_url)
        .header("xi-api-key", key)
        .timeout(CONNECT_TIMEOUT)
        .send()
        .await
        .map_err(|e| anyhow!("token request failed: {}", plain(e)))?
        .json()
        .await
        .map_err(|e| anyhow!("token response: {}", plain(e)))?;
    let token = v["token"]
        .as_str()
        .ok_or_else(|| anyhow!("no token in response"))?;
    url.query_pairs_mut().append_pair("token", token);
    let (ws, _) = timeout(
        CONNECT_TIMEOUT,
        tokio_tungstenite::connect_async(url.as_str()),
    )
    .await
    .map_err(|_| anyhow!("stream connect timed out"))?
    .context("stream connect failed")?;
    Ok(ws)
}

fn audio_msg(pcm: &[u8], commit: bool) -> Message {
    Message::text(
        json!({
            "message_type": "input_audio_chunk",
            "audio_base_64": base64::engine::general_purpose::STANDARD.encode(pcm),
            "commit": commit,
            "sample_rate": crate::audio::SAMPLE_RATE,
        })
        .to_string(),
    )
}

fn join(a: &str, b: &str) -> String {
    match (a.trim(), b.trim()) {
        ("", r) => r.to_string(),
        (l, "") => l.to_string(),
        (l, r) => format!("{l} {r}"),
    }
}

/// reqwest errors without the URL (which can carry a token).
fn plain(e: reqwest::Error) -> String {
    let kind = if e.is_timeout() {
        "timed out"
    } else if e.is_connect() {
        "could not connect"
    } else {
        "request error"
    };
    format!("{kind}: {}", e.without_url())
}

pub fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        s.chars().take(n).collect::<String>() + "…"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn joins() {
        assert_eq!(join("a ", " b"), "a b");
        assert_eq!(join("", "b"), "b");
        assert_eq!(join("a", ""), "a");
    }

    #[test]
    fn norm_safety() {
        assert!(norm_is_safe("one two three four", "One, two, three, four."));
        assert!(!norm_is_safe("one two three four five six", "Sure!"));
    }
}
