//! Progress display: numbered stages with ETA, and graceful Ctrl+C.
//!
//! Progress goes to stderr (indicatif hides it when stderr is not a terminal),
//! so redirecting stdout to a file captures a clean summary.

use crate::report::{human, num};
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use std::fmt::Display;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

static STAGE: AtomicUsize = AtomicUsize::new(0);
static STAGES: AtomicUsize = AtomicUsize::new(0);
static GRACEFUL: AtomicBool = AtomicBool::new(false);
static INTERRUPTED: AtomicBool = AtomicBool::new(false);

/// ASCII only: the legacy Windows console has no glyphs for braille spinners.
const TICKS: &str = "|/-\\ ";
const BAR: &str = "=> ";

pub fn set_stages(n: usize) {
    STAGE.store(0, Ordering::Relaxed);
    STAGES.store(n, Ordering::Relaxed);
}

fn next_label(name: &str) -> String {
    let k = STAGE.fetch_add(1, Ordering::Relaxed) + 1;
    let n = STAGES.load(Ordering::Relaxed).max(k);
    format!("[{k}/{n}] {name:<12}")
}

/// One step of the pipeline. While running it shows a bar or spinner;
/// when finished it leaves a single "done" line behind.
pub struct Stage {
    pb: ProgressBar,
    label: String,
    started: Instant,
}

impl Stage {
    /// For work with an unknown total (scanning): no ETA, only counters and rate.
    pub fn spinner(name: &str) -> Stage {
        Self::new(
            name,
            ProgressBar::new_spinner(),
            "{spinner} {prefix} {msg} ({elapsed})",
        )
    }

    pub fn count(name: &str, len: u64) -> Stage {
        Self::new(
            name,
            ProgressBar::new(len),
            "  {prefix} [{bar:30}] {human_pos}/{human_len} files ({eta} left)",
        )
    }

    pub fn bytes(name: &str, len: u64) -> Stage {
        Self::new(
            name,
            ProgressBar::new(len),
            "  {prefix} [{bar:30}] {bytes}/{total_bytes} {bytes_per_sec} ({eta} left)",
        )
    }

    fn new(name: &str, pb: ProgressBar, template: &str) -> Stage {
        let label = next_label(name);
        pb.set_style(
            ProgressStyle::with_template(template)
                .expect("valid template")
                .tick_chars(TICKS)
                .progress_chars(BAR),
        );
        pb.set_prefix(label.clone());
        pb.enable_steady_tick(Duration::from_millis(150));
        Stage { pb, label, started: Instant::now() }
    }

    pub fn bar(&self) -> &ProgressBar {
        &self.pb
    }

    pub fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }

    pub fn finish(self, summary: impl Display) {
        self.pb.finish_and_clear();
        if !self.pb.is_hidden() {
            eprintln!(
                "  {} done: {summary} ({})",
                self.label,
                duration(self.started.elapsed())
            );
        }
    }
}

/// Bar for one large file being copied, shown under the main copy bar.
pub fn file_bar(mp: &MultiProgress, name: &str, len: u64) -> ProgressBar {
    let pb = mp.add(ProgressBar::new(len));
    pb.set_style(
        ProgressStyle::with_template("      > {msg} {bytes}/{total_bytes}")
            .expect("valid template"),
    );
    pb.set_message(short(name, 60));
    pb
}

/// A plain text line inside a MultiProgress (file counters under the copy bar).
pub fn text_line(mp: &MultiProgress) -> ProgressBar {
    let pb = mp.add(ProgressBar::new_spinner());
    pb.set_style(ProgressStyle::with_template("      {msg}").expect("valid template"));
    pb
}

/// "182,410 files, 412.6 GB found (8,950 files/s)"
pub fn found_message(files: u64, bytes: u64, elapsed: Duration) -> String {
    let secs = elapsed.as_secs_f64().max(0.001);
    format!(
        "{} files, {} found ({} files/s)",
        num(files),
        human(bytes),
        num((files as f64 / secs) as u64)
    )
}

/// "0:21", "13:40", "1:05:03"
pub fn duration(d: Duration) -> String {
    let s = d.as_secs();
    let (h, m, s) = (s / 3600, s / 60 % 60, s % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

fn short(name: &str, max: usize) -> String {
    let chars: Vec<char> = name.chars().collect();
    if chars.len() <= max {
        return name.to_string();
    }
    let tail: String = chars[chars.len() - (max - 3)..].iter().collect();
    format!("...{tail}")
}

/// First Ctrl+C during a graceful section stops after the current files;
/// otherwise (or on a second Ctrl+C) the process exits right away.
/// An interrupted copy leaves only `.xfer-partial` files, which a rerun replaces.
pub fn install_ctrlc() {
    let _ = ctrlc::set_handler(|| {
        if GRACEFUL.load(Ordering::SeqCst) && !INTERRUPTED.swap(true, Ordering::SeqCst) {
            eprintln!("\nStopping after the files in progress... Press Ctrl+C again to quit now.");
        } else {
            std::process::exit(130);
        }
    });
}

pub fn set_graceful(on: bool) {
    GRACEFUL.store(on, Ordering::SeqCst);
}

pub fn interrupted() -> bool {
    INTERRUPTED.load(Ordering::SeqCst)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_durations() {
        assert_eq!(duration(Duration::from_secs(21)), "0:21");
        assert_eq!(duration(Duration::from_secs(820)), "13:40");
        assert_eq!(duration(Duration::from_secs(3903)), "1:05:03");
    }

    #[test]
    fn shortens_long_names_from_the_left() {
        assert_eq!(short("abc", 10), "abc");
        assert_eq!(short("0123456789abcdef", 10), "...9abcdef");
    }
}
