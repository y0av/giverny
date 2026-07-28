//! Byte-stream tee: a second `vte::Parser` observing the PTY output stream
//! ahead of `Term::advance`, extracting escape sequences that
//! `alacritty_terminal` does not surface.
//!
//! The tee is strictly read-only: the same untouched byte slice is fed to the
//! real processor afterwards. Because the `vte::Parser` state machine persists
//! across `observe` calls, escape sequences split across read chunks are
//! handled correctly.
//!
//! Extracted:
//! - OSC 7   — working directory reports (`file://host/path`), host-verified
//! - OSC 133 — semantic prompt marks (A/B/C/D)
//! - OSC 9 / OSC 777 — application notifications
//! - OSC 7791 — Giverny's private state channel (nonce-authenticated; emitted
//!   by Claude Code hooks via their `terminalSequence` output)
//! - DECSET/DECRST 1049/1047/47 — alt-screen transitions (snapshot freeze)

use std::path::PathBuf;

/// Number of Giverny's private OSC channel.
pub const PRIVATE_OSC: &[u8] = b"7791";

/// Semantic prompt marks per OSC 133 (FinalTerm / iTerm2 convention).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptMark {
    /// `OSC 133;A` — prompt is about to be drawn.
    PromptStart,
    /// `OSC 133;B` — user input begins.
    CommandStart,
    /// `OSC 133;C` — command output begins.
    OutputStart,
    /// `OSC 133;D[;exit]` — command finished.
    CommandDone(Option<i32>),
}

/// Events extracted from the stream by the tee.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TeeEvent {
    /// OSC 7 with a host that matches this machine (or none).
    CwdChanged(PathBuf),
    /// OSC 7 with a foreign host — path shown but must not be used for spawn/resume.
    RemoteCwd(PathBuf),
    /// OSC 133 prompt mark.
    Prompt(PromptMark),
    /// OSC 9 (body only) or OSC 777;notify;title;body.
    Notify { title: String, body: String },
    /// Authenticated private-channel payload (raw, semicolon-joined).
    Private(String),
    /// Private-channel message with a wrong/missing nonce (forgery or echo).
    PrivateRejected,
    /// DECSET/DECRST 1049/1047/47: entering (true) / leaving (false) alt screen.
    AltScreen(bool),
}

/// The tee. Feed every PTY read through [`Tee::observe`] *before* advancing
/// the real terminal, then drain [`Tee::take_events`].
pub struct Tee {
    parser: vte::Parser,
    perform: TeePerform,
}

impl Tee {
    /// `nonce`: per-spawn secret required on the private OSC channel.
    /// `local_hostname`: this machine's hostname for OSC 7 verification
    /// (comparison is case-insensitive; empty/`localhost` always accepted).
    pub fn new(nonce: String, local_hostname: Option<String>) -> Self {
        Self {
            parser: vte::Parser::new(),
            perform: TeePerform {
                events: Vec::new(),
                nonce,
                local_hostname: local_hostname.map(|h| h.to_ascii_lowercase()),
            },
        }
    }

    pub fn observe(&mut self, bytes: &[u8]) {
        self.parser.advance(&mut self.perform, bytes);
    }

    /// Drain events extracted since the last call.
    pub fn take_events(&mut self) -> Vec<TeeEvent> {
        std::mem::take(&mut self.perform.events)
    }

    pub fn has_events(&self) -> bool {
        !self.perform.events.is_empty()
    }
}

struct TeePerform {
    events: Vec<TeeEvent>,
    nonce: String,
    local_hostname: Option<String>,
}

impl TeePerform {
    fn on_osc7(&mut self, payload: &[u8]) {
        // Expected: file://[host]/path — percent-encoded UTF-8 path.
        let Some(rest) = payload.strip_prefix(b"file://") else {
            return;
        };
        let (host, path) = match rest.iter().position(|&b| b == b'/') {
            Some(i) => (&rest[..i], &rest[i..]),
            // "file://" with no path — ignore.
            None => return,
        };
        let Some(path) = percent_decode(path) else {
            return;
        };
        let host = String::from_utf8_lossy(host).to_ascii_lowercase();
        let local = host.is_empty()
            || host == "localhost"
            || self
                .local_hostname
                .as_deref()
                .is_some_and(|h| h == host || host.strip_suffix(".local") == Some(h));
        let path = PathBuf::from(path);
        self.events.push(if local {
            TeeEvent::CwdChanged(path)
        } else {
            TeeEvent::RemoteCwd(path)
        });
    }

    fn on_osc133(&mut self, params: &[&[u8]]) {
        let mark = match params.get(1).copied() {
            Some(b"A") => PromptMark::PromptStart,
            Some(b"B") => PromptMark::CommandStart,
            Some(b"C") => PromptMark::OutputStart,
            Some(p) if p.first() == Some(&b'D') => {
                let exit = params
                    .get(2)
                    .and_then(|s| std::str::from_utf8(s).ok())
                    .and_then(|s| s.parse().ok());
                PromptMark::CommandDone(exit)
            }
            _ => return,
        };
        self.events.push(TeeEvent::Prompt(mark));
    }

    fn on_private(&mut self, params: &[&[u8]]) {
        // OSC 7791 ; <nonce> ; <payload...>
        let ok = params
            .get(1)
            .is_some_and(|n| !self.nonce.is_empty() && *n == self.nonce.as_bytes());
        if !ok {
            self.events.push(TeeEvent::PrivateRejected);
            return;
        }
        let payload = params[2..]
            .iter()
            .map(|p| String::from_utf8_lossy(p))
            .collect::<Vec<_>>()
            .join(";");
        self.events.push(TeeEvent::Private(payload));
    }
}

/// Cap on notification title/body length (untrusted input).
const NOTIFY_CAP: usize = 512;

fn clean_text(bytes: &[u8], cap: usize) -> String {
    let s = String::from_utf8_lossy(bytes);
    let mut out: String = s.chars().filter(|c| !c.is_control()).take(cap).collect();
    if out.len() < s.len() {
        out.shrink_to_fit();
    }
    out
}

fn percent_decode(bytes: &[u8]) -> Option<String> {
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' => {
                let h = bytes.get(i + 1).and_then(|b| (*b as char).to_digit(16))?;
                let l = bytes.get(i + 2).and_then(|b| (*b as char).to_digit(16))?;
                out.push((h * 16 + l) as u8);
                i += 3;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8(out).ok()
}

impl vte::Perform for TeePerform {
    fn osc_dispatch(&mut self, params: &[&[u8]], _bell_terminated: bool) {
        let Some(&code) = params.first() else { return };
        match code {
            b"7" => {
                if let Some(payload) = params.get(1) {
                    self.on_osc7(payload);
                }
            }
            b"133" => self.on_osc133(params),
            b"9" => {
                if let Some(body) = params.get(1) {
                    self.events.push(TeeEvent::Notify {
                        title: String::new(),
                        body: clean_text(body, NOTIFY_CAP),
                    });
                }
            }
            b"777" => {
                // 777;notify;title;body
                if params.get(1).copied() == Some(b"notify") {
                    let title = params.get(2).map(|t| clean_text(t, NOTIFY_CAP)).unwrap_or_default();
                    let body = params.get(3).map(|b| clean_text(b, NOTIFY_CAP)).unwrap_or_default();
                    self.events.push(TeeEvent::Notify { title, body });
                }
            }
            c if c == PRIVATE_OSC => self.on_private(params),
            _ => {}
        }
    }

    fn csi_dispatch(
        &mut self,
        params: &vte::Params,
        intermediates: &[u8],
        _ignore: bool,
        action: char,
    ) {
        // DECSET (CSI ? Pm h) / DECRST (CSI ? Pm l) — watch alt-screen modes.
        if intermediates != b"?" || (action != 'h' && action != 'l') {
            return;
        }
        for param in params.iter() {
            for &p in param {
                if matches!(p, 1049 | 1047 | 47) {
                    self.events.push(TeeEvent::AltScreen(action == 'h'));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collect(tee: &mut Tee, chunks: &[&[u8]]) -> Vec<TeeEvent> {
        for c in chunks {
            tee.observe(c);
        }
        tee.take_events()
    }

    fn tee() -> Tee {
        Tee::new("s3cr3t".into(), Some("myhost".into()))
    }

    #[test]
    fn osc7_local_and_remote() {
        let mut t = tee();
        let ev = collect(
            &mut t,
            &[b"\x1b]7;file://myhost/home/yoz/dev\x07\x1b]7;file://other/tmp\x07"],
        );
        assert_eq!(
            ev,
            vec![
                TeeEvent::CwdChanged(PathBuf::from("/home/yoz/dev")),
                TeeEvent::RemoteCwd(PathBuf::from("/tmp")),
            ]
        );
    }

    #[test]
    fn osc7_percent_decode_and_localhost() {
        let mut t = tee();
        let ev = collect(&mut t, &[b"\x1b]7;file://localhost/a%20b\x07"]);
        assert_eq!(ev, vec![TeeEvent::CwdChanged(PathBuf::from("/a b"))]);
    }

    #[test]
    fn split_across_chunks() {
        let mut t = tee();
        // Escape sequence split into 4 chunks, including mid-"file://".
        let ev = collect(&mut t, &[b"hello \x1b]7;fi", b"le://", b"myhost/x", b"\x07 tail"]);
        assert_eq!(ev, vec![TeeEvent::CwdChanged(PathBuf::from("/x"))]);
    }

    #[test]
    fn prompt_marks() {
        let mut t = tee();
        let ev = collect(&mut t, &[b"\x1b]133;A\x07\x1b]133;B\x07\x1b]133;C\x07\x1b]133;D;0\x07"]);
        assert_eq!(
            ev,
            vec![
                TeeEvent::Prompt(PromptMark::PromptStart),
                TeeEvent::Prompt(PromptMark::CommandStart),
                TeeEvent::Prompt(PromptMark::OutputStart),
                TeeEvent::Prompt(PromptMark::CommandDone(Some(0))),
            ]
        );
    }

    #[test]
    fn private_channel_auth() {
        let mut t = tee();
        let ev = collect(
            &mut t,
            &[b"\x1b]7791;s3cr3t;state=working;sid=abc\x07\x1b]7791;wrong;x\x07\x1b]7791;solo\x07"],
        );
        assert_eq!(
            ev,
            vec![
                TeeEvent::Private("state=working;sid=abc".into()),
                TeeEvent::PrivateRejected,
                TeeEvent::PrivateRejected,
            ]
        );
    }

    #[test]
    fn notifications() {
        let mut t = tee();
        let ev = collect(&mut t, &[b"\x1b]9;done!\x07\x1b]777;notify;Build;finished ok\x1b\\"]);
        assert_eq!(
            ev,
            vec![
                TeeEvent::Notify { title: String::new(), body: "done!".into() },
                TeeEvent::Notify { title: "Build".into(), body: "finished ok".into() },
            ]
        );
    }

    #[test]
    fn notify_strips_controls_and_caps() {
        let mut t = tee();
        let ev = collect(&mut t, &[b"\x1b]9;a\rb\x07"]);
        assert_eq!(ev, vec![TeeEvent::Notify { title: String::new(), body: "ab".into() }]);
    }

    #[test]
    fn alt_screen_toggle() {
        let mut t = tee();
        let ev = collect(&mut t, &[b"\x1b[?1049h vim vim \x1b[?1049l"]);
        assert_eq!(ev, vec![TeeEvent::AltScreen(true), TeeEvent::AltScreen(false)]);
    }

    #[test]
    fn ignores_unrelated_sequences() {
        let mut t = tee();
        let ev = collect(&mut t, &[b"\x1b[31mred\x1b[0m\x1b]0;title\x07\x1b[?2026h"]);
        assert!(ev.is_empty());
    }
}
