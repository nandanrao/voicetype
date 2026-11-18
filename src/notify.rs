//! Desktop notifications over the freedesktop D-Bus interface (mako, dunst,
//! GNOME, KDE, …). One popup per dictation, updated in place. Calls are
//! queued per popup and bounded by a timeout, so a stuck notification
//! daemon can delay a popup but never the dictation itself.

use std::collections::HashMap;
use std::time::Duration;
use tokio::sync::{OnceCell, mpsc};
use zbus::Variant as Value;

static BUS: OnceCell<Option<zbus::Connection>> = OnceCell::const_new();

async fn bus() -> Option<&'static zbus::Connection> {
    BUS.get_or_init(|| async {
        match tokio::time::timeout(Duration::from_secs(2), zbus::Connection::session()).await {
            Ok(Ok(c)) => Some(c),
            _ => {
                crate::log!("notifications: no D-Bus session bus");
                None
            }
        }
    })
    .await
    .as_ref()
}

struct Msg {
    summary: String,
    body: String,
    timeout_ms: i32,
}

/// Handle to one popup. Cheap to clone; updates are applied in order and
/// bursts are coalesced to the newest.
#[derive(Clone)]
pub struct Popup {
    tx: mpsc::UnboundedSender<Msg>,
    max_chars: usize,
}

impl Popup {
    pub fn new(max_chars: usize) -> Popup {
        let (tx, mut rx) = mpsc::unbounded_channel::<Msg>();
        tokio::spawn(async move {
            let mut id: u32 = 0;
            while let Some(mut msg) = rx.recv().await {
                while let Ok(newer) = rx.try_recv() {
                    msg = newer;
                }
                if let Some(new_id) = send(id, &msg).await {
                    id = new_id;
                }
            }
        });
        Popup { tx, max_chars }
    }

    /// `timeout_ms` 0 means "while in progress": long, but finite, so a popup
    /// can't outlive a daemon that died mid-dictation by much.
    pub fn show(&self, summary: impl Into<String>, body: impl Into<String>, timeout_ms: i32) {
        let timeout_ms = if timeout_ms == 0 {
            15 * 60 * 1000
        } else {
            timeout_ms
        };
        let _ = self.tx.send(Msg {
            summary: summary.into(),
            body: tail(body.into(), self.max_chars),
            timeout_ms,
        });
    }
}

/// Notification daemons cut long bodies off at the bottom, which would hide
/// the newest words of a long dictation, so long bodies show their end.

fn tail(body: String, max: usize) -> String {
    let count = body.chars().count();
    if max == 0 || count <= max {
        return body;
    }
    let rest: String = body.chars().skip(count - max).collect();
    // Start at a word boundary when there is one nearby.
    let rest = match rest.find(char::is_whitespace) {
        Some(i) if i < 40 => rest[i..].trim_start().to_string(),
        _ => rest,
    };
    format!("…{rest}")
}

#[cfg(test)]
mod tests {
    use super::tail;

    #[test]
    fn keeps_short_bodies() {
        assert_eq!(tail("hello world".into(), 50), "hello world");
    }

    #[test]
    fn shows_the_end_of_long_bodies() {
        let t = tail("one two three four five six".into(), 10);
        assert_eq!(t, "…five six");
    }
}

async fn send(replaces: u32, msg: &Msg) -> Option<u32> {
    let bus = bus().await?;
    let mut hints: HashMap<&str, Value> = HashMap::new();
    hints.insert("urgency", Value::U8(0));
    // Keep these out of notification history: they are progress, not news.
    hints.insert("transient", Value::Bool(true));
    let body = (
        "voicetype",
        replaces,
        "audio-input-microphone",
        msg.summary.as_str(),
        msg.body.as_str(),
        Vec::<&str>::new(),
        hints,
        msg.timeout_ms,
    );
    let call = bus.call_method(
        Some("org.freedesktop.Notifications"),
        "/org/freedesktop/Notifications",
        Some("org.freedesktop.Notifications"),
        "Notify",
        &body,
    );
    match tokio::time::timeout(Duration::from_secs(2), call).await {
        Ok(Ok(reply)) => reply.body().deserialize::<u32>().ok(),
        Ok(Err(e)) => {
            crate::log!("notification failed: {e}");
            None
        }
        Err(_) => {
            crate::log!("notification timed out");
            None
        }
    }
}
