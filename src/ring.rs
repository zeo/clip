use std::collections::VecDeque;
use std::sync::Arc;

/// media foundation time base: 100 ns ticks
pub const SECOND: i64 = 10_000_000;

#[derive(Debug)]
pub struct Packet {
    pub pts: i64,
    pub duration: i64,
    pub key: bool,
    pub data: Vec<u8>,
}

/// Encoded packets covering at least `span` of the most recent stream, always starting on a keyframe.
/// Whole GOPs are evicted from the front so any snapshot decodes on its own.
pub struct Ring {
    packets: VecDeque<Arc<Packet>>,
    keys: VecDeque<i64>,
    span: i64,
    bytes: usize,
}

impl Ring {
    pub fn new(span: i64) -> Self {
        Self { packets: VecDeque::new(), keys: VecDeque::new(), span, bytes: 0 }
    }

    pub fn push(&mut self, packet: Packet) {
        if self.packets.is_empty() && !packet.key {
            return;
        }
        if packet.key {
            self.keys.push_back(packet.pts);
        }
        self.bytes += packet.data.len();
        let end = packet.pts + packet.duration;
        self.packets.push_back(Arc::new(packet));
        while self.keys.len() > 1 && end - self.keys[1] >= self.span {
            self.keys.pop_front();
            while self.packets.front().is_some_and(|p| p.pts < self.keys[0]) {
                let dropped = self.packets.pop_front().expect("front checked");
                self.bytes -= dropped.data.len();
            }
        }
    }

    /// Packets from the last keyframe at or before `since` through the newest one.
    pub fn since(&self, since: i64) -> Vec<Arc<Packet>> {
        let start = self.keys.iter().rev().find(|&&k| k <= since).or(self.keys.front()).copied();
        let Some(start) = start else { return Vec::new() };
        self.packets.iter().skip_while(|p| p.pts < start).cloned().collect()
    }

    pub fn end(&self) -> Option<i64> {
        self.packets.back().map(|p| p.pts + p.duration)
    }

    pub fn bytes(&self) -> usize {
        self.bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FRAME: i64 = SECOND / 10;

    fn feed(ring: &mut Ring, frames: i64, gop: i64) {
        for i in 0..frames {
            ring.push(Packet { pts: i * FRAME, duration: FRAME, key: i % gop == 0, data: vec![0; 100] });
        }
    }

    #[test]
    fn keeps_at_least_the_span_and_starts_on_a_keyframe() {
        let mut ring = Ring::new(3 * SECOND);
        feed(&mut ring, 100, 10);
        let all = ring.since(i64::MIN);
        assert!(all[0].key);
        let covered = ring.end().unwrap() - all[0].pts;
        assert!(covered >= 3 * SECOND, "covered {covered}");
        assert!(covered < 4 * SECOND, "kept a whole extra gop: {covered}");
        assert_eq!(ring.bytes(), all.len() * 100);
    }

    #[test]
    fn drops_packets_before_the_first_keyframe() {
        let mut ring = Ring::new(SECOND);
        ring.push(Packet { pts: 0, duration: FRAME, key: false, data: vec![1] });
        assert!(ring.since(i64::MIN).is_empty());
        ring.push(Packet { pts: FRAME, duration: FRAME, key: true, data: vec![2] });
        assert_eq!(ring.since(i64::MIN).len(), 1);
    }

    #[test]
    fn since_snaps_back_to_the_previous_keyframe() {
        let mut ring = Ring::new(10 * SECOND);
        feed(&mut ring, 50, 10);
        let clip = ring.since(25 * FRAME);
        assert_eq!(clip[0].pts, 20 * FRAME);
        assert!(clip[0].key);
        assert_eq!(clip.len(), 30);
        assert_eq!(ring.since(-5).first().unwrap().pts, 0);
    }
}
