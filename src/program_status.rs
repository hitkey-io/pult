//! The state a program reports about itself over OSC 7501 — Claude Code does, as `state=…:app=claude-code`.
//!
//! Claude Code asks its terminal at start and on attach (`ESC ] 7501 ; ?`), and reports only to a terminal that
//! names the states it understands. alacritty_terminal drops unknown OSC sequences and reads the PTY on its own
//! thread, so [`StatusPty`] sits between the two: it answers the query, takes the reports out of the byte stream
//! and hands them to the window.
//!
//! Report: `ESC ] 7501 ; state=<s>:app=<name>[:id=<id>][:kind=permission|question|auth][:progress=0..100]
//! [:title=<base64>][:msg=<base64>]`, ended by BEL or `ESC \`. `state=clear[:id=<id>]` withdraws a record.

use std::{
    io::{self, ErrorKind, Read, Write},
    sync::Arc,
};

use alacritty_terminal::{
    event::{OnResize, WindowSize},
    tty::{self, ChildEvent, EventedPty, EventedReadWrite},
};
use futures::channel::mpsc::{self, UnboundedReceiver, UnboundedSender};
use polling::{Event as PollingEvent, PollMode, Poller};

/// The answer to the query: these states are understood.
const SUPPORTED: &[u8] = b"\x1b]7501;?:states=idle,working,done,blocked,error\x1b\\";
/// Start of any 7501 sequence.
const MARKER: &[u8] = b"\x1b]7501;";
/// A sequence longer than this isn't a status report: passed through untouched.
const LIMIT: usize = 4096;

/// One state, as the program names it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Idle,
    Working,
    Done,
    Blocked,
    Error,
    /// The record was withdrawn: nothing is known about the program now.
    Clear,
}

impl State {
    fn parse(text: &str) -> Option<Self> {
        Some(match text {
            "idle" => State::Idle,
            "working" => State::Working,
            "done" => State::Done,
            "blocked" => State::Blocked,
            "error" => State::Error,
            "clear" => State::Clear,
            _ => return None,
        })
    }

    /// As the program names it.
    pub fn name(&self) -> &'static str {
        match self {
            State::Idle => "idle",
            State::Working => "working",
            State::Done => "done",
            State::Blocked => "blocked",
            State::Error => "error",
            State::Clear => "clear",
        }
    }
}

impl Report {
    /// One line, as the program said it.
    pub fn describe(&self) -> String {
        let mut line = self.state.name().to_string();
        if let Some(kind) = &self.kind {
            line.push_str(&format!(" kind={kind}"));
        }
        if let Some(progress) = self.progress {
            line.push_str(&format!(" progress={progress}"));
        }
        if let Some(title) = &self.title {
            line.push_str(&format!(" title={title:?}"));
        }
        line
    }
}

/// What programs report, as it comes: `trace-status.log` in Pult's data folder, enabled by a `trace-status`
/// file next to it. For checking what a program really says.
pub fn trace(line: &str) {
    use std::io::Write as _;
    static FILE: std::sync::OnceLock<Option<std::sync::Mutex<std::fs::File>>> = std::sync::OnceLock::new();
    let file = FILE.get_or_init(|| {
        let dir = crate::journal::data_dir();
        dir.join("trace-status")
            .exists()
            .then(|| std::fs::OpenOptions::new().create(true).append(true).open(dir.join("trace-status.log")).ok())
            .flatten()
            .map(std::sync::Mutex::new)
    });
    if let Some(file) = file
        && let Ok(mut file) = file.lock()
    {
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
        let _ = writeln!(file, "{}.{:03} {line}", now.as_secs(), now.subsec_millis());
    }
}

/// What a program reports at one moment. Unknown fields and whole unknown records are ignored: a newer program
/// may say more than Pult reads.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Report {
    pub state: State,
    /// Why the program is blocked, when it says so.
    pub kind: Option<String>,
    /// 0..=100, when the program says so.
    pub progress: Option<u8>,
    /// What the program is doing, when it says so.
    pub title: Option<String>,
}

/// Takes the 7501 sequences out of a byte stream. Bytes come in arbitrary pieces, so a cut sequence is kept
/// until its end arrives; `MARKER` cut in two is kept the same way.
#[derive(Default)]
pub struct Scanner {
    pending: Vec<u8>,
    /// Bytes fed since the last take, without 7501 sequences.
    clean: Vec<u8>,
    reports: Vec<Report>,
    queried: bool,
}

impl Scanner {
    /// Feed one read from the PTY.
    pub fn push(&mut self, bytes: &[u8]) {
        let mut data = std::mem::take(&mut self.pending);
        data.extend_from_slice(bytes);
        let mut rest = data.as_slice();
        while let Some(start) = find(rest, MARKER) {
            let after = &rest[start + MARKER.len()..];
            // The payload runs to BEL or ESC \.
            let end = after.iter().enumerate().find_map(|(index, byte)| match byte {
                0x07 => Some((index, index + 1)),
                0x1b if after.get(index + 1) == Some(&b'\\') => Some((index, index + 2)),
                _ => None,
            });
            let Some((payload_end, next)) = end else {
                if after.len() < LIMIT {
                    // The rest of the sequence hasn't arrived yet: keep it whole, with what precedes it.
                    self.pending.extend_from_slice(&rest[start..]);
                } else {
                    // Too long to be a report: let it through.
                    self.clean.extend_from_slice(&rest[..start + MARKER.len()]);
                    self.clean.extend_from_slice(after);
                }
                rest = &rest[..start];
                break;
            };
            self.clean.extend_from_slice(&rest[..start]);
            let payload = &after[..payload_end];
            if payload == b"?" {
                self.queried = true;
            } else if let Some(report) = parse(payload) {
                self.reports.push(report);
            }
            rest = &after[next..];
        }
        // Only a tail that can be the start of a sequence is kept until more arrives.
        let cut = find_keep(rest);
        self.clean.extend_from_slice(&rest[..cut]);
        self.pending.extend_from_slice(&rest[cut..]);
    }

    /// Everything fed since the last call without 7501 sequences.
    pub fn take_bytes(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.clean)
    }

    /// The reports fed since the last call.
    pub fn take_reports(&mut self) -> Vec<Report> {
        std::mem::take(&mut self.reports)
    }

    /// Whether the program asked whether states are understood since the last call.
    pub fn take_query(&mut self) -> bool {
        std::mem::replace(&mut self.queried, false)
    }

    /// Whether a cut sequence is held back.
    #[cfg(test)]
    pub fn holding(&self) -> bool {
        !self.pending.is_empty()
    }
}

/// How many leading bytes of `haystack` can be passed on now: everything but a tail that can still grow into
/// `MARKER` (a proper beginning of it).
fn find_keep(haystack: &[u8]) -> usize {
    (1..MARKER.len())
        .rev()
        .find(|&len| haystack.ends_with(&MARKER[..len]))
        .map_or(haystack.len(), |len| haystack.len() - len)
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|window| window == needle)
}

fn parse(payload: &[u8]) -> Option<Report> {
    let text = std::str::from_utf8(payload).ok()?;
    let mut state = None;
    let mut kind = None;
    let mut progress = None;
    let mut title = None;
    for field in text.split(':') {
        let (key, value) = field.split_once('=')?;
        match key {
            "state" => state = State::parse(value),
            "kind" => kind = Some(value.to_string()),
            "progress" => progress = value.parse::<u8>().ok(),
            "title" => title = decode(value),
            _ => {}
        }
    }
    Some(Report { state: state?, kind, progress, title })
}

/// A field value in base64 (standard alphabet, optional padding).
fn decode(value: &str) -> Option<String> {
    let mut out = Vec::with_capacity(value.len() / 4 * 3);
    let mut buffer = 0u32;
    let mut bits = 0u32;
    for byte in value.bytes().take_while(|&byte| byte != b'=') {
        let digit = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        } as u32;
        buffer = (buffer << 6) | digit;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buffer >> bits) as u8);
        }
    }
    String::from_utf8(out).ok()
}

/// The PTY as alacritty_terminal needs it, with 7501 sequences taken out of what the program writes.
pub struct StatusPty {
    inner: tty::Pty,
    /// Where the reports go.
    reports: UnboundedSender<Report>,
    /// Filtered bytes not yet handed to the event loop.
    ready: Vec<u8>,
    /// A cut 7501 sequence, waiting for the rest.
    pending: Vec<u8>,
}

/// A channel for the reports. It never blocks the reader thread; a report is dropped only if the window is gone.
pub fn channel() -> (UnboundedSender<Report>, UnboundedReceiver<Report>) {
    mpsc::unbounded()
}
impl StatusPty {
    pub fn new(inner: tty::Pty, reports: UnboundedSender<Report>) -> Self {
        Self { inner, reports, ready: Vec::new(), pending: Vec::new() }
    }

    /// One read through the scanner. `Ok(0)` only when the PTY itself ended; a read that held only reports
    /// says "nothing yet", which the event loop takes correctly while its buffer is still empty.
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if !self.ready.is_empty() {
            return Ok(self.drain(out));
        }
        if out.is_empty() {
            return Err(ErrorKind::WouldBlock.into());
        }
        let mut buffer = [0u8; 4096];
        let got = self.inner.reader().read(&mut buffer)?;
        if got == 0 {
            return Ok(0);
        }
        let mut scanner = Scanner { pending: std::mem::take(&mut self.pending), ..Scanner::default() };
        scanner.push(&buffer[..got]);
        if scanner.take_query() {
            self.inner.writer().write_all(SUPPORTED)?;
        }
        for report in scanner.take_reports() {
            let _ = self.reports.unbounded_send(report);
        }
        self.ready = scanner.take_bytes();
        self.pending = scanner.pending;
        if self.ready.is_empty() {
            return Err(ErrorKind::WouldBlock.into());
        }
        Ok(self.drain(out))
    }

    fn drain(&mut self, out: &mut [u8]) -> usize {
        let copied = self.ready.len().min(out.len());
        out[..copied].copy_from_slice(&self.ready[..copied]);
        self.ready.drain(..copied);
        copied
    }
}

impl EventedReadWrite for StatusPty {
    type Reader = Self;
    type Writer = std::fs::File;

    /// SAFETY: same contract as `tty::Pty`: the PTY is owned by `self` and outlives its registration.
    unsafe fn register(
        &mut self,
        poll: &Arc<Poller>,
        interest: PollingEvent,
        opts: PollMode,
    ) -> io::Result<()> {
        // SAFETY: see above.
        unsafe { self.inner.register(poll, interest, opts) }
    }

    fn reregister(
        &mut self,
        poll: &Arc<Poller>,
        interest: PollingEvent,
        opts: PollMode,
    ) -> io::Result<()> {
        self.inner.reregister(poll, interest, opts)
    }

    fn deregister(&mut self, poll: &Arc<Poller>) -> io::Result<()> {
        self.inner.deregister(poll)
    }

    /// The PTY itself is its reader: `read` on `Self` goes through the scanner.
    fn reader(&mut self) -> &mut Self::Reader {
        self
    }

    fn writer(&mut self) -> &mut Self::Writer {
        self.inner.writer()
    }
}

impl Read for StatusPty {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        StatusPty::read(self, out)
    }
}

impl EventedPty for StatusPty {
    fn next_child_event(&mut self) -> Option<ChildEvent> {
        self.inner.next_child_event()
    }
}

impl OnResize for StatusPty {
    fn on_resize(&mut self, window_size: WindowSize) {
        self.inner.on_resize(window_size)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(state: State) -> Report {
        Report { state, kind: None, progress: None, title: None }
    }

    #[test]
    fn reports_are_taken_out_and_the_rest_passes() {
        let mut s = Scanner::default();
        s.push(b"hello\x1b]7501;state=working:app=claude-code\x1b\\world\x1b]7501;state=done:app=claude-code\x07!");
        assert_eq!(s.take_bytes(), b"helloworld!");
        assert_eq!(s.take_reports(), vec![report(State::Working), report(State::Done)]);
        assert!(!s.take_query());
    }

    #[test]
    fn a_sequence_cut_between_reads_is_kept_whole() {
        let mut s = Scanner::default();
        s.push(b"ab\x1b]75");
        assert_eq!(s.take_bytes(), b"ab");
        assert!(s.holding());
        s.push(b"01;state=idle:app=claude-code:title=QSB0YXNr\x07cd");
        assert_eq!(s.take_bytes(), b"cd");
        let reports = s.take_reports();
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].state, State::Idle);
        assert_eq!(reports[0].title.as_deref(), Some("A task"));
    }

    #[test]
    fn the_query_is_seen_with_either_terminator() {
        let mut s = Scanner::default();
        s.push(b"\x1b]7501;?\x07x\x1b]7501;?\x1b\\");
        assert_eq!(s.take_bytes(), b"x");
        assert!(s.take_query());
        assert!(!s.take_query(), "asked about once per take");
        s.push(b"\x1b]7501;?\x07");
        assert!(s.take_query());
    }

    #[test]
    fn other_sequences_and_unknown_records_are_left_alone() {
        let mut s = Scanner::default();
        s.push(concat!(
            "\x1b]0;title\x07",
            "\x1b]7501;state=blocked:app=claude-code:kind=permission:progress=40:future=1:title=QSB0YXNr\x1b\\",
            "\x1b]7501;state=dreaming\x07",
            "\x1b]7501;app=x\x07",
        ).as_bytes());
        assert_eq!(s.take_bytes(), b"\x1b]0;title\x07");
        let reports = s.take_reports();
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].state, State::Blocked);
        assert_eq!(reports[0].kind.as_deref(), Some("permission"));
        assert_eq!(reports[0].progress, Some(40));
        assert_eq!(reports[0].title.as_deref(), Some("A task"));
    }

    #[test]
    fn clear_withdraws_the_record() {
        let mut s = Scanner::default();
        s.push(b"\x1b]7501;state=clear:id=abc\x07");
        assert_eq!(s.take_reports(), vec![report(State::Clear)]);
    }

    #[test]
    fn a_too_long_sequence_is_passed_through() {
        let mut s = Scanner::default();
        s.push(b"\x1b]7501;state=working");
        assert!(s.holding());
        let junk = vec![b'x'; LIMIT + 10];
        s.push(&junk);
        assert!(!s.holding());
        assert!(s.take_bytes().starts_with(b"\x1b]7501;state=working"));
    }

    /// A real PTY: the program asks, gets the answer, reports, and prints text around it. The event loop
    /// reads the way this test does.
    #[test]
    fn a_program_in_a_real_terminal_is_answered_and_its_reports_taken_out() {
        use alacritty_terminal::event::WindowSize;
        use std::time::{Duration, Instant};

        // The program: ask, print text and a report, wait for the answer the terminal gives, print it and more.
        // The program prints what it read as hex: `/bin/sh` drops control characters held in a variable.
        let script = format!(
            "stty raw -echo min 1 time 0 2>/dev/null; \
             printf 'A{esc}]7501;?{esc}\\\\B{esc}]7501;state=working:app=test{bel}C\\n'; \
             x=$(dd bs=64 count=1 2>/dev/null | od -An -tx1 | tr -d ' \\n'); \
             printf 'D|GOT:%s|E\\n' \"$x\"; \
             printf '{esc}]7501;state=done:app=test{esc}\\\\F'",
            esc = '\x1b',
            bel = '\x07',
        );
        let options = tty::Options {
            shell: Some(tty::Shell::new("/bin/sh".into(), vec!["-c".into(), script.to_string()])),
            working_directory: None,
            drain_on_exit: false,
            env: std::collections::HashMap::new(),
        };
        let size = WindowSize { num_lines: 24, num_cols: 80, cell_width: 8, cell_height: 16 };
        let pty = tty::new(&options, size, 0).unwrap();
        let (sender, mut reports) = channel();
        let mut pty = StatusPty::new(pty, sender);
        let (mut out, mut seen, mut done) = (Vec::new(), Vec::new(), false);
        let deadline = Instant::now() + Duration::from_secs(20);
        while Instant::now() < deadline {
            let mut buffer = [0u8; 4096];
            match pty.read(&mut buffer) {
                Ok(0) => break,
                Ok(got) => out.extend_from_slice(&buffer[..got]),
                Err(error) if error.kind() == ErrorKind::WouldBlock => {}
                Err(error) => panic!("read: {error}"),
            }
            while let Ok(report) = reports.try_recv() {
                seen.push(report);
            }
            if out.ends_with(b"F") && seen.len() == 2 {
                done = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let text = String::from_utf8_lossy(&out).replace('\n', "");
        assert!(done, "not finished, read {text:?} and {seen:?}");
        assert!(text.starts_with("ABCD|GOT:"), "text around the reports: {text:?}");
        let answer: String = SUPPORTED.iter().map(|byte| format!("{byte:02x}")).collect();
        assert!(text.contains(&format!("|GOT:{answer}|")), "the program got the answer: {text:?}");
        assert!(text.ends_with("|EF"), "text after the reports: {text:?}");
        assert!(!text.contains("7501;state="), "no report reaches the terminal: {text:?}");
        assert_eq!(seen.iter().map(|report| report.state).collect::<Vec<_>>(), [State::Working, State::Done]);
    }

    #[test]
    fn a_marker_cut_at_the_very_end_is_held() {
        let mut s = Scanner::default();
        s.push(b"ok\x1b");
        assert_eq!(s.take_bytes(), b"ok");
        s.push(b"]7501;state=error\x07");
        assert_eq!(s.take_bytes(), b"");
        assert_eq!(s.take_reports(), vec![report(State::Error)]);
    }
}
