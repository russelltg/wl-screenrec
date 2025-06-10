use std::time::Duration;

use log::debug;

pub struct FpsLimit<T> {
    min_dt: Duration,
    on_deck: Option<(Duration, T)>,
    next_target_time: Option<Duration>,
}

// fps limit for VRR is pretty tricky. We can't just discard frames with close timestamps, because imagine the situation
// where we get the following stream of timestamps (in ms)
// 0, 16, 17, 10000
// we obviously want to drop the 16, not the 17, because that 17 is displayed for a very long time.
// so, basically, we need to add a frame of latency and buffer a frame to know if we should skip a frame
impl<T> FpsLimit<T> {
    pub fn new(max_fps: f64) -> Self {
        assert_ne!(max_fps, 0.);
        Self {
            min_dt: Duration::from_secs_f64(1. / max_fps),
            on_deck: None,
            next_target_time: None,
        }
    }

    pub fn on_new_frame(&mut self, f: T, ts: Duration) -> Option<T> {
        // always send the first frame, could be a long gap after.
        if self.next_target_time.is_none() {
            self.next_target_time = Some(ts + self.min_dt);
            return Some(f);
        }

        // don't have enough info to make a decision, hold on...
        if self.on_deck.is_none() {
            self.on_deck = Some((ts, f));
            return None;
        }

        let (old_ts, old_t) = self.on_deck.take().unwrap();
        let next_target_time = self.next_target_time.unwrap();
        self.on_deck = Some((ts, f));

        if ts < next_target_time {
            // drop
            debug!("--max-fps dropping frame with ts {old_ts:?}");

            None
        } else {
            debug!("--max-fps including frame with ts {old_ts:?}");

            // max to handle skips better
            self.next_target_time = Some(next_target_time.max(old_ts) + self.min_dt);
            Some(old_t)
        }
    }

    pub fn flush(&mut self) -> Option<T> {
        self.on_deck.take().map(|(_, t)| t)
    }
}

#[cfg(test)]
mod test {
    use std::time::Duration;

    use crate::fps_limit::FpsLimit;

    #[test]
    fn basic() {
        let mut l = FpsLimit::<u32>::new(1.);
        let s = Duration::from_secs_f32;

        let out_frames: Vec<_> = [
            l.on_new_frame(0, s(0.)),
            l.on_new_frame(1, s(0.5)),
            l.on_new_frame(2, s(1.1)),
            l.on_new_frame(3, s(1.2)),
            l.on_new_frame(4, s(1.3)),
            l.on_new_frame(5, s(5.)),
            l.flush(),
        ]
        .into_iter()
        .flatten()
        .collect();

        assert_eq!(out_frames, [0, 1, 4, 5])
    }

    #[test]
    fn synthetic_120hz() {
        let mut l = FpsLimit::<u32>::new(30.);

        let mut acc = vec![];
        for i in 0..120 {
            if let Some(r) = l.on_new_frame(i, Duration::from_micros((i * 1_000_000 / 120) as u64))
            {
                acc.push(r);
            }
        }

        if let Some(r) = l.flush() {
            acc.push(r);
        }

        let ct = acc.len();
        assert!(ct >= 28 && ct < 32, "ct={ct} acc={acc:?}");
    }

    #[test]
    fn large_skip() {
        let mut l = FpsLimit::<u32>::new(1.);
        let s = Duration::from_secs_f32;

        let out_frames: Vec<_> = [
            l.on_new_frame(0, s(0.)),
            l.on_new_frame(1, s(0.5)),
            l.on_new_frame(2, s(10.0)),
            l.on_new_frame(3, s(10.1)),
            l.on_new_frame(4, s(10.2)),
            l.on_new_frame(5, s(10.3)),
            l.flush(),
        ]
        .into_iter()
        .flatten()
        .collect();

        assert_eq!(out_frames, [0, 1, 2, 5])
    }
}
