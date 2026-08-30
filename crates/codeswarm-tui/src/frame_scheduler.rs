//! Latest-state terminal output scheduling.
//!
//! Terminal writes are ordered side effects: dropping an incremental diff can
//! invalidate every later diff. This scheduler therefore drops stale deltas
//! and requires the caller to submit a complete repaint before deltas resume.

use std::time::{Duration, Instant};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Frame {
    pub bytes: Vec<u8>,
    pub complete: bool,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct FrameScheduler {
    in_flight: bool,
    pending: Option<Frame>,
    resync_required: bool,
    repaint_requested: bool,
}

#[derive(Clone, Debug)]
pub struct ResizeCoalescer {
    pending: Option<(u16, u16)>,
    last_event: Option<Instant>,
    settle_after: Duration,
}

/// Terminal geometry used by [`RenderLoop`].
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct TerminalSize {
    pub width: u16,
    pub height: u16,
}

/// Work returned by [`RenderLoop::next`].
///
/// A resize or a dropped delta first produces [`RenderWork::Repaint`]. The
/// caller renders a complete frame for that geometry and submits it with
/// [`RenderLoop::submit_complete`]. Only then can incremental writes resume.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RenderWork {
    Repaint { size: TerminalSize },
    Write(Frame),
}

/// Small orchestration layer for a terminal render loop.
///
/// `FrameScheduler` owns write ordering while `ResizeCoalescer` owns resize
/// timing. Keeping those policies together gives callers one deterministic
/// event-loop boundary: feed input and invalidations into this type, then poll
/// [`RenderLoop::next`] from the terminal task. Resize bursts do not trigger
/// repeated full renders, and a resize can never allow a stale delta to be
/// written after the geometry changed.
#[derive(Clone, Debug)]
pub struct RenderLoop {
    scheduler: FrameScheduler,
    resize: ResizeCoalescer,
    size: TerminalSize,
    complete_repaint_required: bool,
    repaint_announced: bool,
}

impl RenderLoop {
    pub fn new(width: u16, height: u16, settle_after: Duration) -> Self {
        Self {
            scheduler: FrameScheduler::default(),
            resize: ResizeCoalescer::new(settle_after),
            size: TerminalSize { width, height },
            complete_repaint_required: false,
            repaint_announced: false,
        }
    }

    pub fn size(&self) -> TerminalSize {
        self.size
    }

    /// Record a terminal resize. The geometry is applied when the resize has
    /// settled, as observed by the next call to [`Self::next`].
    pub fn resize(&mut self, width: u16, height: u16, now: Instant) {
        self.resize.push(width, height, now);
    }

    /// Request a complete repaint for the current geometry.
    pub fn request_repaint(&mut self) {
        self.complete_repaint_required = true;
        self.repaint_announced = false;
    }

    /// Queue an incremental frame unless a complete repaint is required.
    pub fn submit_delta(&mut self, bytes: impl Into<Vec<u8>>) -> bool {
        if self.complete_repaint_required {
            return false;
        }
        let accepted = self.scheduler.submit_delta(bytes);
        if !accepted {
            self.complete_repaint_required = true;
            self.repaint_announced = false;
        }
        accepted
    }

    /// Queue a complete frame and release the loop's resynchronization gate.
    pub fn submit_complete(&mut self, bytes: impl Into<Vec<u8>>) -> bool {
        let accepted = self.scheduler.submit_complete(bytes);
        if accepted {
            self.complete_repaint_required = false;
            self.repaint_announced = false;
        }
        accepted
    }

    /// Return the next repaint request or terminal write.
    ///
    /// This method is deliberately clock-injected so resize behavior can be
    /// tested without sleeping and production callers can use their event
    /// loop's monotonic timestamp.
    pub fn next(&mut self, now: Instant) -> Option<RenderWork> {
        if let Some((width, height)) = self.resize.take_settled(now) {
            let next_size = TerminalSize { width, height };
            if next_size != self.size {
                self.size = next_size;
                self.request_repaint();
            }
        }

        if self.scheduler.needs_repaint() {
            self.complete_repaint_required = true;
        }
        if self.complete_repaint_required && !self.repaint_announced {
            self.repaint_announced = true;
            return Some(RenderWork::Repaint { size: self.size });
        }

        self.scheduler.take_next().map(RenderWork::Write)
    }

    pub fn finish_write(&mut self) {
        self.scheduler.finish_write();
    }

    pub fn has_in_flight_write(&self) -> bool {
        self.scheduler.has_in_flight_write()
    }

    pub fn has_pending_frame(&self) -> bool {
        self.scheduler.has_pending_frame()
    }

    pub fn needs_repaint(&self) -> bool {
        self.complete_repaint_required || self.scheduler.needs_repaint()
    }
}

impl ResizeCoalescer {
    pub fn new(settle_after: Duration) -> Self {
        Self {
            pending: None,
            last_event: None,
            settle_after,
        }
    }

    pub fn push(&mut self, width: u16, height: u16, now: Instant) {
        self.pending = Some((width, height));
        self.last_event = Some(now);
    }

    pub fn take_settled(&mut self, now: Instant) -> Option<(u16, u16)> {
        let last_event = self.last_event?;
        if now.duration_since(last_event) < self.settle_after {
            return None;
        }
        self.last_event = None;
        self.pending.take()
    }
}

impl FrameScheduler {
    pub fn submit_delta(&mut self, bytes: impl Into<Vec<u8>>) -> bool {
        if self.resync_required || self.in_flight || self.pending.is_some() {
            self.resync_required = true;
            if !self.repaint_requested {
                self.repaint_requested = true;
            }
            return false;
        }
        self.pending = Some(Frame {
            bytes: bytes.into(),
            complete: false,
        });
        true
    }

    pub fn submit_complete(&mut self, bytes: impl Into<Vec<u8>>) -> bool {
        self.pending = Some(Frame {
            bytes: bytes.into(),
            complete: true,
        });
        self.resync_required = false;
        self.repaint_requested = false;
        true
    }

    pub fn take_next(&mut self) -> Option<Frame> {
        if self.in_flight {
            return None;
        }
        let frame = self.pending.take()?;
        self.in_flight = true;
        Some(frame)
    }

    pub fn finish_write(&mut self) {
        self.in_flight = false;
    }

    pub fn needs_repaint(&self) -> bool {
        self.repaint_requested
    }

    pub fn has_in_flight_write(&self) -> bool {
        self.in_flight
    }

    pub fn has_pending_frame(&self) -> bool {
        self.pending.is_some()
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::{FrameScheduler, RenderLoop, RenderWork, ResizeCoalescer, TerminalSize};

    #[test]
    fn stale_deltas_are_dropped_until_a_complete_repaint() {
        let mut scheduler = FrameScheduler::default();
        assert!(scheduler.submit_delta("first"));
        let first = scheduler.take_next().expect("first frame");
        assert_eq!(first.bytes, b"first");
        assert!(!scheduler.submit_delta("stale-1"));
        assert!(!scheduler.submit_delta("stale-2"));
        assert!(scheduler.needs_repaint());
        scheduler.finish_write();
        assert!(scheduler.take_next().is_none());
        assert!(scheduler.submit_complete("complete"));
        assert!(!scheduler.needs_repaint());
        assert_eq!(scheduler.take_next().expect("repaint").bytes, b"complete");
    }

    #[test]
    fn only_one_frame_can_be_in_flight() {
        let mut scheduler = FrameScheduler::default();
        assert!(scheduler.submit_delta("frame"));
        assert!(scheduler.take_next().is_some());
        assert!(scheduler.take_next().is_none());
        assert!(scheduler.has_in_flight_write());
    }

    #[test]
    fn resize_coalescer_emits_only_final_geometry() {
        let start = Instant::now();
        let mut resize = ResizeCoalescer::new(Duration::from_millis(100));
        resize.push(80, 24, start);
        resize.push(90, 30, start + Duration::from_millis(50));
        assert_eq!(
            resize.take_settled(start + Duration::from_millis(149)),
            None
        );
        assert_eq!(
            resize.take_settled(start + Duration::from_millis(150)),
            Some((90, 30))
        );
    }

    #[test]
    fn render_loop_coalesces_resize_into_one_complete_repaint() {
        let start = Instant::now();
        let mut render_loop = RenderLoop::new(80, 24, Duration::from_millis(100));
        render_loop.resize(100, 30, start);
        render_loop.resize(120, 40, start + Duration::from_millis(50));

        assert_eq!(
            render_loop.size(),
            TerminalSize {
                width: 80,
                height: 24
            }
        );
        assert_eq!(render_loop.next(start + Duration::from_millis(149)), None);
        assert_eq!(
            render_loop.next(start + Duration::from_millis(150)),
            Some(RenderWork::Repaint {
                size: TerminalSize {
                    width: 120,
                    height: 40,
                },
            })
        );
        assert_eq!(
            render_loop.size(),
            TerminalSize {
                width: 120,
                height: 40
            }
        );
        assert_eq!(render_loop.next(start + Duration::from_millis(150)), None);

        assert!(!render_loop.submit_delta("stale"));
        assert!(render_loop.submit_complete("complete"));
        assert_eq!(
            render_loop.next(start + Duration::from_millis(151)),
            Some(RenderWork::Write(super::Frame {
                bytes: b"complete".to_vec(),
                complete: true,
            }))
        );
    }

    #[test]
    fn resize_repaint_waits_for_an_in_flight_write() {
        let start = Instant::now();
        let mut render_loop = RenderLoop::new(80, 24, Duration::from_millis(10));
        assert!(render_loop.submit_delta("old"));
        assert!(matches!(
            render_loop.next(start),
            Some(RenderWork::Write(super::Frame {
                complete: false,
                ..
            }))
        ));

        render_loop.resize(100, 30, start);
        assert_eq!(
            render_loop.next(start + Duration::from_millis(10)),
            Some(RenderWork::Repaint {
                size: TerminalSize {
                    width: 100,
                    height: 30,
                },
            })
        );
        assert!(render_loop.submit_complete("new"));
        assert_eq!(render_loop.next(start + Duration::from_millis(10)), None);
        render_loop.finish_write();
        assert_eq!(
            render_loop.next(start + Duration::from_millis(11)),
            Some(RenderWork::Write(super::Frame {
                bytes: b"new".to_vec(),
                complete: true,
            }))
        );
    }

    #[test]
    fn dropped_delta_announces_repaint_once_and_recovers() {
        let start = Instant::now();
        let mut render_loop = RenderLoop::new(80, 24, Duration::ZERO);
        assert!(render_loop.submit_delta("first"));
        assert!(render_loop.next(start).is_some());
        assert!(!render_loop.submit_delta("dropped"));
        assert_eq!(
            render_loop.next(start),
            Some(RenderWork::Repaint {
                size: TerminalSize {
                    width: 80,
                    height: 24,
                },
            })
        );
        assert_eq!(render_loop.next(start), None);
        assert!(render_loop.submit_complete("repaint"));
        render_loop.finish_write();
        assert_eq!(
            render_loop.next(start),
            Some(RenderWork::Write(super::Frame {
                bytes: b"repaint".to_vec(),
                complete: true,
            }))
        );
    }
}
