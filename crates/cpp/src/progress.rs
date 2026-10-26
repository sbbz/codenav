//! A progress bar for the long build phases.
//!
//! Indexing the kernel spends over a minute in two loops that previously printed
//! nothing until they finished, which is indistinguishable from a hang.
//!
//! Drawn only when stderr is a terminal, so piping the build to a file or a CI
//! log still produces the plain per-phase summaries and no escape codes.

use std::io::{IsTerminal, Write};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::Instant;

/// Minimum gap between redraws. Fast enough to look live, slow enough that the
/// extraction loop's 64k updates do not turn into 64k writes to the terminal.
const REDRAW_MS: u64 = 80;

const WIDTH: usize = 28;

pub struct Progress {
    label: &'static str,
    total: usize,
    done: AtomicUsize,
    start: Instant,
    /// Milliseconds since `start` at the last redraw. Doubles as the lock: the
    /// thread that wins the compare-exchange is the one that draws.
    last_draw: AtomicU64,
    enabled: bool,
}

impl Progress {
    pub fn new(label: &'static str, total: usize) -> Self {
        Self {
            label,
            total,
            done: AtomicUsize::new(0),
            start: Instant::now(),
            last_draw: AtomicU64::new(0),
            enabled: total > 0 && std::io::stderr().is_terminal(),
        }
    }

    /// Safe to call from many worker threads at once.
    pub fn inc(&self, n: usize) {
        let done = self.done.fetch_add(n, Ordering::Relaxed) + n;
        if !self.enabled {
            return;
        }

        let now = self.start.elapsed().as_millis() as u64;
        let last = self.last_draw.load(Ordering::Relaxed);
        if now < last.saturating_add(REDRAW_MS) && done < self.total {
            return;
        }
        if self
            .last_draw
            .compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed)
            .is_err()
        {
            return; // another thread is drawing this tick
        }
        self.draw(done);
    }

    fn draw(&self, done: usize) {
        let done = done.min(self.total);
        let frac = done as f64 / self.total as f64;
        let filled = (frac * WIDTH as f64).round() as usize;

        let elapsed = self.start.elapsed().as_secs_f64();
        // Below a few percent the estimate is noise, so it is left blank rather
        // than shown wildly wrong.
        let eta = if frac > 0.03 {
            format!("eta {}", secs(elapsed / frac - elapsed))
        } else {
            String::new()
        };

        let mut e = std::io::stderr().lock();
        let _ = write!(
            e,
            "\r\x1b[2K  {:<8} [{}{}] {:>3.0}%  {}/{}  {}  {}",
            self.label,
            "█".repeat(filled),
            "░".repeat(WIDTH - filled),
            frac * 100.0,
            done,
            self.total,
            secs(elapsed),
            eta,
        );
        let _ = e.flush();
    }

    /// Clears the bar so the phase summary that follows starts on a clean line.
    pub fn finish(&self) {
        if !self.enabled {
            return;
        }
        let mut e = std::io::stderr().lock();
        let _ = write!(e, "\r\x1b[2K");
        let _ = e.flush();
    }
}

/// A status line for a phase with no loop to count, cleared by [`clear`].
///
/// Silent when stderr is not a terminal, so a piped build log never receives
/// escape codes or a line it cannot overwrite.
pub fn note(msg: &str) {
    if !std::io::stderr().is_terminal() {
        return;
    }
    let mut e = std::io::stderr().lock();
    let _ = write!(e, "\r\x1b[2K  {msg}");
    let _ = e.flush();
}

pub fn clear() {
    if !std::io::stderr().is_terminal() {
        return;
    }
    let mut e = std::io::stderr().lock();
    let _ = write!(e, "\r\x1b[2K");
    let _ = e.flush();
}

fn secs(s: f64) -> String {
    if !s.is_finite() || s < 0.0 {
        return "--".into();
    }
    let s = s.round() as u64;
    if s >= 60 {
        format!("{}m{:02}s", s / 60, s % 60)
    } else {
        format!("{s}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_regardless_of_whether_it_draws() {
        // Under `cargo test` stderr is not a terminal, so drawing is off — the
        // counter still has to work, since callers read nothing back from it.
        let p = Progress::new("t", 10);
        for _ in 0..10 {
            p.inc(1);
        }
        assert_eq!(p.done.load(Ordering::Relaxed), 10);
        p.finish();
    }

    #[test]
    fn an_empty_phase_never_draws() {
        let p = Progress::new("t", 0);
        assert!(!p.enabled, "0/0 would divide by zero");
        p.inc(1);
        p.finish();
    }

    #[test]
    fn durations_read_as_minutes_past_sixty_seconds() {
        assert_eq!(secs(9.4), "9s");
        assert_eq!(secs(59.6), "1m00s");
        assert_eq!(secs(95.0), "1m35s");
        assert_eq!(secs(f64::NAN), "--");
    }

    #[test]
    fn overshooting_the_total_does_not_panic() {
        // Worker threads increment optimistically; a miscount must not blow up
        // the repeat() in draw().
        let p = Progress::new("t", 4);
        p.inc(99);
        p.draw(99);
    }
}
