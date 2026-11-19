// Codex emits OSC 7501 lifecycle frames independently of its visible TUI.
// Match only complete known frames. The reader retains no terminal text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Signal {
    pub sequence: u64,
    pub cycle: u64,
    pub submission: u64,
    pub observed_ms: i64,
    pub kind: &'static str,
}

const FRAMES: &[(&[u8], &str)] = &[
    (b"7501;state=working:app=codex", "working"),
    (b"7501;state=done:app=codex", "completed"),
    (b"7501;state=error:app=codex", "interrupted"),
    (b"7501;state=blocked:app=codex:kind=permission", "approval"),
    (b"7501;state=blocked:app=codex:kind=question", "question"),
    (b"7501;state=blocked:app=codex:kind=auth", "auth"),
    (b"7501;state=idle:app=codex", "idle"),
    (b"7501;state=clear", "clear"),
];

#[derive(Default)]
enum Scan {
    #[default]
    Text,
    Escape,
    Osc,
    OscEscape,
}

#[derive(Default)]
pub(crate) struct CodexStatus {
    scan: Scan,
    candidates: u16,
    offset: usize,
    active: bool,
    saw_working: bool,
    cycle: u64,
    phase: &'static str,
    closed_submission: u64,
    pub(crate) last: Option<Signal>,
}

impl CodexStatus {
    pub(crate) fn observe(&mut self, packet: &[u8], submitted: u64) {
        use Scan::*;
        for &byte in packet {
            match (&self.scan, byte) {
                (Escape | OscEscape, b']') => {
                    self.scan = Osc;
                    self.candidates = (1 << FRAMES.len()) - 1;
                    self.offset = 0;
                }
                (OscEscape, b'\\') | (Osc, 7) => {
                    for (i, &(frame, kind)) in FRAMES.iter().enumerate() {
                        if self.candidates & (1 << i) != 0 && frame.len() == self.offset {
                            self.record(kind, submitted);
                            break;
                        }
                    }
                    self.scan = Text;
                }
                (Osc, 0x1b) => self.scan = OscEscape,
                (_, 0x1b) => self.scan = Escape,
                (Osc, _) => {
                    if self.candidates != 0 {
                        for (i, &(frame, _)) in FRAMES.iter().enumerate() {
                            if frame.get(self.offset) != Some(&byte) {
                                self.candidates &= !(1 << i);
                            }
                        }
                        self.offset += 1;
                    }
                }
                _ => self.scan = Text,
            }
        }
    }

    fn record(&mut self, kind: &'static str, submitted: u64) {
        match kind {
            "idle" | "clear" | "auth" => {
                self.active = false;
                self.saw_working = false;
                self.closed_submission = submitted;
                self.phase = kind;
                return;
            }
            "working" | "approval" | "question" => {
                if !self.active {
                    self.cycle += 1;
                }
                self.active = true;
                self.saw_working |= kind == "working";
            }
            "completed" | "interrupted" => {
                if !self.saw_working && submitted <= self.closed_submission {
                    return;
                }
                if !self.active {
                    self.cycle += 1;
                }
                self.active = false;
                self.saw_working = false;
                self.closed_submission = submitted;
            }
            _ => {}
        }
        // A second freshly submitted turn can finish without a rendered
        // working frame. Duplicate done/error frames already returned above.
        if self.phase != kind || matches!(kind, "completed" | "interrupted") {
            self.last = Some(Signal {
                sequence: self.last.map_or(1, |signal| signal.sequence + 1),
                cycle: self.cycle,
                submission: submitted,
                observed_ms: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |d| d.as_millis() as i64),
                kind,
            });
        }
        self.phase = kind;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn expect(status: &CodexStatus, sequence: u64, kind: &'static str) {
        let signal = status.last.unwrap();
        assert_eq!((signal.sequence, signal.kind), (sequence, kind));
    }

    #[test]
    fn short_completion_survives_every_packet_boundary_without_a_worked_footer() {
        let data =
            b"\x1b]7501;state=working:app=codex\x1b\\OK\r\n\x1b]7501;state=done:app=codex\x1b\\";
        for split in 0..=data.len() {
            let mut status = CodexStatus::default();
            status.observe(&data[..split], 0);
            status.observe(&data[split..], 0);
            expect(&status, 2, "completed");
            status.observe(b"\x1b]7501;state=done:app=codex\x07", 0);
            expect(&status, 2, "completed");
        }
    }

    #[test]
    fn errors_idle_auth_and_foreign_frames_do_not_create_completions() {
        let mut status = CodexStatus::default();
        status.observe(
            b"\x1b]7501;state=done:app=codex\x07\x1b]7501;state=idle:app=codex\x07",
            0,
        );
        status.observe(
            b"\x1b]7501;state=done:app=other\x07\x1b]7501;state=blocked:app=codex:kind=auth\x07",
            0,
        );
        assert_eq!(status.last, None);
        status.observe(
            b"\x1b]7501;state=working:app=codex\x07\x1b]7501;state=error:app=codex\x07",
            1,
        );
        status.observe(
            b"\x1b]7501;state=done:app=codex\x07\x1b]7501;state=clear\x1b\\",
            1,
        );
        expect(&status, 2, "interrupted");
        status.observe(b"\x1b]7501;state=done:app=codex\x07", 2);
        expect(&status, 3, "completed");
    }

    #[test]
    fn blocked_frames_are_distinct_and_unknown_payloads_are_bounded() {
        let mut status = CodexStatus::default();
        status.observe(b"\x1b]7501;state=done:app=codex", 1);
        status.observe(&[b'x'; 4096], 1);
        status.observe(b"\x1b\\", 1);
        assert_eq!(status.last, None);
        status.observe(b"\x1b]7501;state=blocked:app=codex:kind=permission\x07", 1);
        status.observe(b"\x1b]7501;state=blocked:app=codex:kind=permission\x07", 1);
        expect(&status, 1, "approval");
        status.observe(b"\x1b]7501;state=blocked:app=codex:kind=question\x1b\\", 1);
        expect(&status, 2, "question");
    }

    #[test]
    fn cycles_survive_a_missed_working_frame_and_retain_identity_through_approval() {
        let mut status = CodexStatus::default();
        let work = b"\x1b]7501;state=working:app=codex\x07";
        let done = b"\x1b]7501;state=done:app=codex\x07";
        status.observe(work, 0);
        status.observe(b"\x1b]7501;state=blocked:app=codex:kind=permission\x07", 0);
        status.observe(work, 0);
        status.observe(done, 0);
        assert_eq!(status.last.unwrap().cycle, 1);
        status.observe(work, 0);
        status.observe(done, 0);
        assert_eq!(status.last.unwrap().cycle, 2);
        assert_eq!(status.last.unwrap().submission, 0);
    }
}
