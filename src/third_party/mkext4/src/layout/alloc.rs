//! The deterministic block allocator: one monotone cursor over the whole
//! device that skips *reserved runs* (backup superblocks/GDTs and flex
//! metadata, whose positions are fixed by geometry).
//!
//! Allocation order therefore IS emission order: journal, then per-inode
//! metadata, then file data in declaration order — everything ascending.

/// A run of physical blocks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Run {
    /// First block.
    pub start: u64,
    /// Length in blocks.
    pub len: u64,
}

/// Monotone skipping allocator.
#[derive(Debug)]
pub struct Allocator {
    cursor: u64,
    end: u64,
    /// Sorted, disjoint reserved runs the cursor jumps over.
    reserved: Vec<Run>,
    /// Index of the next reserved run at/after the cursor.
    next_reserved: usize,
}

impl Allocator {
    /// Create an allocator over `[start, end)` skipping `reserved`
    /// (must be sorted and disjoint; runs may lie outside the range).
    pub fn new(start: u64, end: u64, reserved: Vec<Run>) -> Allocator {
        debug_assert!(reserved
            .windows(2)
            .all(|w| w[0].start + w[0].len <= w[1].start));
        let mut a = Allocator {
            cursor: start,
            end,
            reserved,
            next_reserved: 0,
        };
        a.skip_reserved();
        a
    }

    fn skip_reserved(&mut self) {
        while let Some(r) = self.reserved.get(self.next_reserved) {
            if r.start + r.len <= self.cursor {
                self.next_reserved += 1;
            } else if r.start <= self.cursor {
                self.cursor = r.start + r.len;
                self.next_reserved += 1;
            } else {
                break;
            }
        }
    }

    /// Blocks remaining allocatable.
    #[cfg(test)]
    pub fn remaining(&self) -> u64 {
        let mut free = self.end.saturating_sub(self.cursor);
        for r in &self.reserved[self.next_reserved.min(self.reserved.len())..] {
            if r.start >= self.end {
                break;
            }
            let start = r.start.max(self.cursor);
            let end = (r.start + r.len).min(self.end);
            free -= end.saturating_sub(start);
        }
        free
    }

    /// Allocate `blocks`, returning ascending runs (split where reserved
    /// runs or `max_run` interrupt). Returns `None` when space runs out
    /// (allocator state is then exhausted — callers treat it as fatal).
    pub fn take(&mut self, mut blocks: u64, max_run: u64) -> Option<Vec<Run>> {
        let mut out: Vec<Run> = Vec::new();
        while blocks > 0 {
            self.skip_reserved();
            if self.cursor >= self.end {
                return None;
            }
            // Room until the next reserved run or device end.
            let limit = self
                .reserved
                .get(self.next_reserved)
                .map(|r| r.start)
                .unwrap_or(self.end)
                .min(self.end);
            let take = blocks.min(limit - self.cursor).min(max_run);
            // No merging: a new run starts only when max_run capped the
            // previous one (deliberate split) or the cursor jumped a
            // reserved run (discontiguous).
            out.push(Run {
                start: self.cursor,
                len: take,
            });
            self.cursor += take;
            blocks -= take;
        }
        Some(out)
    }

    /// Allocate exactly one block.
    pub fn take_one(&mut self) -> Option<u64> {
        self.take(1, 1).map(|runs| runs[0].start)
    }

    /// Current cursor position (next block to be allocated).
    pub fn cursor(&self) -> u64 {
        self.cursor
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skips_reserved_runs() {
        let mut a = Allocator::new(
            10,
            100,
            vec![Run { start: 20, len: 5 }, Run { start: 40, len: 2 }],
        );
        let runs = a.take(20, u64::MAX).unwrap();
        assert_eq!(
            runs,
            vec![Run { start: 10, len: 10 }, Run { start: 25, len: 10 }]
        );
        let runs = a.take(10, u64::MAX).unwrap();
        assert_eq!(
            runs,
            vec![Run { start: 35, len: 5 }, Run { start: 42, len: 5 }]
        );
    }

    #[test]
    fn max_run_caps_segments() {
        let mut a = Allocator::new(0, 1000, vec![]);
        let runs = a.take(10, 4).unwrap();
        // Extent building needs the splits, so capped runs are never
        // merged back together even when physically contiguous.
        assert!(runs.iter().all(|r| r.len <= 4));
        assert_eq!(runs.iter().map(|r| r.len).sum::<u64>(), 10);
    }

    #[test]
    fn exhaustion_returns_none() {
        let mut a = Allocator::new(0, 10, vec![Run { start: 2, len: 3 }]);
        assert_eq!(a.remaining(), 7);
        assert!(a.take(7, u64::MAX).is_some());
        assert!(a.take(1, u64::MAX).is_none());
    }

    #[test]
    fn cursor_starts_inside_reserved() {
        let mut a = Allocator::new(0, 100, vec![Run { start: 0, len: 10 }]);
        assert_eq!(a.take_one(), Some(10));
    }
}
