//! Microphone capture through the PulseAudio protocol (served by PipeWire on
//! most desktops). Runs on its own OS thread: the simple API blocks, and a
//! wedged sound server must never stall the daemon.

use anyhow::{Result, anyhow};
use libpulse_binding::def::BufferAttr;
use libpulse_binding::sample::{Format, Spec};
use libpulse_binding::stream::Direction;
use libpulse_simple_binding::Simple;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::mpsc::UnboundedSender;

pub const SAMPLE_RATE: u32 = 16_000;
/// 100 ms of 16 kHz mono s16le.
pub const CHUNK_BYTES: usize = 3200;

pub enum AudioMsg {
    /// Capture is open; audio follows.
    Started,
    Chunk(Vec<u8>),
    Failed(String),
}

/// Start capturing. Chunks arrive on `tx` until `stop` is set; the sender is
/// dropped when the thread ends, which closes the channel.
pub fn spawn(source: Option<String>, stop: Arc<AtomicBool>, tx: UnboundedSender<AudioMsg>) {
    std::thread::Builder::new()
        .name("capture".into())
        .spawn(move || {
            if let Err(e) = capture(source.as_deref(), &stop, &tx) {
                let _ = tx.send(AudioMsg::Failed(e.to_string()));
            }
        })
        .expect("spawning capture thread");
}

fn capture(source: Option<&str>, stop: &AtomicBool, tx: &UnboundedSender<AudioMsg>) -> Result<()> {
    let spec = Spec {
        format: Format::S16le,
        channels: 1,
        rate: SAMPLE_RATE,
    };
    // Small fragments so audio reaches the live stream with ~100 ms latency.
    let attr = BufferAttr {
        maxlength: u32::MAX,
        tlength: u32::MAX,
        prebuf: u32::MAX,
        minreq: u32::MAX,
        fragsize: CHUNK_BYTES as u32,
    };
    let pa = Simple::new(
        None,
        "voicetype",
        Direction::Record,
        source,
        "dictation",
        &spec,
        None,
        Some(&attr),
    )
    .map_err(|e| anyhow!("cannot open microphone: {e}"))?;
    if tx.send(AudioMsg::Started).is_err() {
        return Ok(());
    }
    let mut buf = vec![0u8; CHUNK_BYTES];
    while !stop.load(Ordering::Relaxed) {
        pa.read(&mut buf)
            .map_err(|e| anyhow!("microphone read failed: {e}"))?;
        if tx.send(AudioMsg::Chunk(buf.clone())).is_err() {
            break;
        }
    }
    Ok(())
}

/// Wrap raw s16le mono PCM in a WAV header.
pub fn wav(pcm: &[u8]) -> Vec<u8> {
    let len = pcm.len() as u32;
    let mut out = Vec::with_capacity(44 + pcm.len());
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + len).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM
    out.extend_from_slice(&1u16.to_le_bytes()); // mono
    out.extend_from_slice(&SAMPLE_RATE.to_le_bytes());
    out.extend_from_slice(&(SAMPLE_RATE * 2).to_le_bytes());
    out.extend_from_slice(&2u16.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(pcm);
    out
}

pub fn seconds(pcm: &[u8]) -> f64 {
    pcm.len() as f64 / (SAMPLE_RATE as f64 * 2.0)
}

/// Peak absolute sample, to tell a dead microphone (all zeros) from silence.
pub fn peak(pcm: &[u8]) -> i32 {
    pcm.as_chunks::<2>()
        .0
        .iter()
        .map(|b| (i16::from_le_bytes([b[0], b[1]]) as i32).abs())
        .max()
        .unwrap_or(0)
}
