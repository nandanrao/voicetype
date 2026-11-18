# voicetype

Dictation for Wayland. Press a key, talk, press it again: the transcript is
typed into whatever window has focus.

- **Fast**: audio streams to the speech-to-text service while you talk
  (ElevenLabs Scribe realtime), so text appears about half a second after you
  stop. Typing a long paragraph takes milliseconds.
- **Any text, any layout**: types through the Wayland virtual-keyboard
  protocol with a keymap built for exactly the characters needed, so accents,
  symbols and emoji come out right regardless of your keyboard layout.
- **Robust under back-to-back use**: start the next dictation while the last
  one is still being transcribed; they're typed in order, never interleaved.
- **Doesn't lose your words**: if streaming fails it falls back to a batch
  request; if that fails too, the audio is kept for a retry.
- **Compositor-agnostic**: works with sway, Hyprland, river, niri, labwc,
  wayfire and others; GNOME/KDE via `ydotool`.

Providers: ElevenLabs (batch and live) or any OpenAI-compatible
transcription endpoint (OpenAI, Groq, local whisper servers).

## Install

Needs Rust, `libpulse` (present on any PipeWire or PulseAudio desktop) and a
freedesktop notification daemon (mako, dunst, … optional).

```sh
cargo install --git https://github.com/nandanrao/voicetype
```

or from a checkout: `cargo build --release && cp target/release/voicetype ~/.local/bin/`.

Then create `~/.config/voicetype/config.toml` (see
[`config.example.toml`](config.example.toml)). The minimum is a key:

```toml
[stt]
key_file = "~/.config/voicetype/elevenlabs.key"
```

Check everything with `voicetype test`.

The live transcript appears in a notification that follows your newest words.
To give it more room in mako:

```ini
# ~/.config/mako/config
[app-name=voicetype]
width=500
height=200
```

and set `[notify] max_chars = 500` in the voicetype config to match.

## Bind a key

`voicetype toggle` is all a key binding needs. The daemon starts by itself on
first use (or start it with your session: `exec voicetype daemon`).

### sway

```
bindsym $mod+Shift+Space exec voicetype toggle

# Optional: while recording, Escape cancels.
mode "recording" {
    bindsym $mod+Shift+Space exec voicetype toggle
    bindsym Escape exec voicetype cancel, mode "default"
}
```

and let voicetype switch the mode based on what it is actually doing:

```toml
[hooks]
on_recording = ["swaymsg", "mode", "recording"]
on_idle = ["swaymsg", "mode", "default"]
```

### Hyprland

```
bind = SUPER SHIFT, Space, exec, voicetype toggle

submap = voicetype
bind = SUPER SHIFT, Space, exec, voicetype toggle
bind = , Escape, exec, voicetype cancel
submap = reset
```

```toml
[hooks]
on_recording = ["hyprctl", "dispatch", "submap", "voicetype"]
on_idle = ["hyprctl", "dispatch", "submap", "reset"]
```

### niri, river, others

Bind `voicetype toggle` to one key and `voicetype cancel` to another.

### Push-to-talk

Bind `voicetype start` to the key press and `voicetype stop` to the release,
e.g. in sway:

```
bindsym $mod+Space exec voicetype start
bindsym --release $mod+Space exec voicetype stop
```

### GNOME / KDE

If the compositor doesn't offer the virtual-keyboard protocol, `auto` falls
back to [`ydotool`](https://github.com/ReimuNotMoe/ydotool) (needs `ydotoold`
running and access to `/dev/uinput`; assumes a US layout). Or set
`typing.method = "command"` with e.g. `command = ["wl-copy"]` to put the text
on the clipboard.

## Commands

| | |
|---|---|
| `voicetype toggle` | start recording, or stop and type the transcript |
| `voicetype start` / `stop` | explicit versions, for push-to-talk |
| `voicetype cancel` | discard the current recording |
| `voicetype status` | what the daemon is doing |
| `voicetype type TEXT` | type TEXT (to test the typing backend) |
| `voicetype transcribe FILE` | batch-transcribe a file, print the text |
| `voicetype test` | check config, key, microphone, typing, daemon |
| `voicetype timings` | per-phase timing log |
| `voicetype quit` | stop the daemon |

## How it stays robust

All state lives in one small daemon; the command you bind is a client that
sends one word over a Unix socket and exits. The daemon handles commands
strictly in order, so fast key presses can't race.

- **Starting never waits on the network.** Recording begins immediately;
  the realtime connection is set up in the background while audio is
  buffered.
- **Every network call has a timeout.** A stalled stream falls back to the
  batch API; a failed batch keeps the audio in
  `~/.local/state/voicetype/failed/` and tells you how to retry.
- **Each dictation is independent.** It owns its audio, stream and popup.
  Transcripts are typed by a single queue in the order the recordings
  started.
- **The compositor mode follows real state.** Hooks run on actual state
  changes, and a stray `stop`/`cancel` re-syncs the mode, so a missed key
  press can't leave you one press out of step.
- **Self-healing.** If the daemon dies, the next key press starts a new one.
  If the microphone delivers nothing (or pure digital silence, a common
  failure after resume), you're told within three seconds.
- **The API key is never on a command line** (where any process could read
  it) and is re-read on each use.

Logs: `~/.local/state/voicetype/daemon.log` and `timing.log`.

## License

MIT
