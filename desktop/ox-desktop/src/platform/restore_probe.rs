//! Emit one timestamp per restored-thread milestone for the native launch fixture.
use gpui::{App, Bounds, Pixels, Window};
use std::sync::{
    Once, OnceLock,
    atomic::{AtomicBool, Ordering},
};

#[derive(Clone, Copy)]
pub enum Milestone {
    FirstFrame,
    ComposerReady,
    TranscriptVisible,
    ThreadLive,
}

impl Milestone {
    fn name(self) -> &'static str {
        match self {
            Self::FirstFrame => "first_frame_callback",
            Self::ComposerReady => "composer_ready",
            Self::TranscriptVisible => "transcript_visible",
            Self::ThreadLive => "thread_live",
        }
    }
}

struct Probe {
    thread: String,
    frame_callback: AtomicBool,
    marks: [Once; 4],
}

static PROBE: OnceLock<Probe> = OnceLock::new();

pub fn init() {
    if let Ok(thread) = std::env::var("OX_DESK_RESTORE_PROBE_THREAD")
        && !thread.is_empty()
    {
        let _ = PROBE.set(Probe {
            thread,
            frame_callback: AtomicBool::new(false),
            marks: Default::default(),
        });
    }
}

pub fn wants(thread: &str, mark: Milestone) -> bool {
    PROBE
        .get()
        .is_some_and(|probe| probe.thread == thread && !probe.marks[mark as usize].is_completed())
}

pub fn mark(milestone: Milestone) {
    if let Some(probe) = PROBE.get() {
        probe.marks[milestone as usize].call_once(|| {
            let ns = ox_desk_core::perf::monotonic_ns();
            println!("desk-restore {} {ns}", milestone.name());
        });
    }
}

pub fn first_frame(window: &Window) {
    if PROBE
        .get()
        .is_some_and(|probe| !probe.frame_callback.swap(true, Ordering::Relaxed))
    {
        // Match the smoke launch's first callback. GPUI can draw the initial
        // scene before this callback and submit it to the compositor afterwards.
        window.on_next_frame(|_, _| mark(Milestone::FirstFrame));
    }
}

pub fn transcript_painted(bounds: Bounds<Pixels>, _: (), window: &mut Window, _: &mut App) {
    let visible = bounds.intersect(&window.content_mask().bounds);
    if visible.size.width > Pixels::ZERO && visible.size.height > Pixels::ZERO {
        mark(Milestone::TranscriptVisible);
    }
}
