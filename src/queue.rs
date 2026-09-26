//! The play queue: what's playing, what's next, shuffle and repeat.

use std::collections::HashSet;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::api::Track;

#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub enum Repeat {
    #[default]
    Off,
    All,
    One,
}

impl Repeat {
    pub fn cycle(self) -> Self {
        match self {
            Repeat::Off => Repeat::All,
            Repeat::All => Repeat::One,
            Repeat::One => Repeat::Off,
        }
    }
}

#[derive(Default)]
pub struct Queue {
    tracks: Vec<Track>,
    /// Play order as indices into `tracks` (a permutation when shuffled).
    order: Vec<usize>,
    /// Position in `order` of the current track.
    cursor: Option<usize>,
    pub shuffle: bool,
    pub repeat: Repeat,
    /// Keep going with related tracks when the queue runs out.
    pub radio: bool,
    /// Every track id played this session, so radio doesn't repeat itself.
    played: HashSet<u64>,
    /// Bumped on every change, so views showing the queue know to refresh.
    pub version: u64,
    rng: u64,
}

impl Queue {
    pub fn current(&self) -> Option<&Track> {
        self.tracks.get(self.order[self.cursor?])
    }

    /// Replace the queue with `tracks`, starting at `tracks[start]`.
    pub fn set(&mut self, tracks: Vec<Track>, start: usize) {
        self.order = (0..tracks.len()).collect();
        self.tracks = tracks;
        self.cursor = (start < self.tracks.len()).then_some(start);
        if self.shuffle {
            self.shuffle_upcoming(true);
        }
        self.mark_played();
        self.version += 1;
    }

    /// What `advance(true)` would play, without moving.
    pub fn peek_next(&self) -> Option<&Track> {
        let next = self.next_cursor(true)?;
        self.tracks.get(self.order[next])
    }

    fn next_cursor(&self, auto: bool) -> Option<usize> {
        let c = self.cursor?;
        if auto && self.repeat == Repeat::One {
            return Some(c);
        }
        if c + 1 < self.order.len() {
            Some(c + 1)
        } else if self.repeat != Repeat::Off && !self.order.is_empty() {
            Some(0)
        } else {
            None
        }
    }

    /// Move to the next track. `auto` is true when the previous track ended
    /// by itself (repeat-one then replays it; a manual skip still skips).
    pub fn advance(&mut self, auto: bool) -> Option<&Track> {
        self.cursor = Some(self.next_cursor(auto)?);
        self.mark_played();
        self.version += 1;
        self.current()
    }

    pub fn back(&mut self) -> Option<&Track> {
        let c = self.cursor?;
        self.cursor = Some(if c > 0 {
            c - 1
        } else if self.repeat == Repeat::All {
            self.order.len().checked_sub(1)?
        } else {
            return None;
        });
        self.version += 1;
        self.current()
    }

    /// Jump to a position in play order (e.g. clicked in the queue view).
    pub fn jump(&mut self, pos: usize) -> Option<&Track> {
        if pos >= self.order.len() {
            return None;
        }
        self.cursor = Some(pos);
        self.mark_played();
        self.version += 1;
        self.current()
    }

    pub fn toggle_shuffle(&mut self) {
        self.shuffle = !self.shuffle;
        if self.shuffle {
            self.shuffle_upcoming(false);
        } else {
            // Back to list order, keeping the current track current.
            let current = self.cursor.map(|c| self.order[c]);
            self.order = (0..self.tracks.len()).collect();
            self.cursor = current;
        }
        self.version += 1;
    }

    /// Shuffle everything after the cursor. With `whole_list`, tracks
    /// before the cursor are pulled in too (a fresh queue).
    fn shuffle_upcoming(&mut self, whole_list: bool) {
        let Some(c) = self.cursor else { return };
        if whole_list {
            let current = self.order.remove(c);
            self.order.insert(0, current);
            self.cursor = Some(0);
        }
        let from = self.cursor.unwrap() + 1;
        for i in (from + 1..self.order.len()).rev() {
            let j = from + (self.random() % (i - from + 1) as u64) as usize;
            self.order.swap(i, j);
        }
    }

    fn random(&mut self) -> u64 {
        if self.rng == 0 {
            self.rng = SystemTime::now().duration_since(UNIX_EPOCH).map_or(1, |d| d.as_nanos() as u64) | 1;
        }
        // xorshift64: plenty for shuffling a playlist.
        self.rng ^= self.rng << 13;
        self.rng ^= self.rng >> 7;
        self.rng ^= self.rng << 17;
        self.rng
    }

    /// Swap the current entry for another track (e.g. a playable upload of
    /// the same song).
    pub fn replace_current(&mut self, track: Track) {
        let Some(c) = self.cursor else { return };
        let i = self.order[c];
        self.tracks[i] = track;
        self.mark_played();
        self.version += 1;
    }

    /// Add to the end of the queue.
    pub fn push(&mut self, track: Track) {
        self.tracks.push(track);
        self.order.push(self.tracks.len() - 1);
        self.version += 1;
    }

    /// Add right after the current track.
    pub fn push_next(&mut self, track: Track) {
        self.tracks.push(track);
        let at = self.cursor.map_or(self.order.len(), |c| c + 1);
        self.order.insert(at, self.tracks.len() - 1);
        self.version += 1;
    }

    /// Remove the entry at `pos` in play order (not the current track).
    pub fn remove(&mut self, pos: usize) -> bool {
        if pos >= self.order.len() || Some(pos) == self.cursor {
            return false;
        }
        self.order.remove(pos);
        if let Some(c) = self.cursor
            && pos < c
        {
            self.cursor = Some(c - 1);
        }
        self.version += 1;
        true
    }

    /// Append radio picks, skipping anything already played or queued.
    pub fn extend_radio(&mut self, tracks: Vec<Track>) -> usize {
        let mut queued: HashSet<u64> = self.tracks.iter().map(|t| t.id).collect();
        let mut added = 0;
        for t in tracks {
            if t.is_playable() && !self.played.contains(&t.id) && queued.insert(t.id) {
                self.push(t);
                added += 1;
            }
        }
        added
    }

    /// Tracks left after the current one (ignoring repeat).
    pub fn remaining(&self) -> usize {
        self.cursor.map_or(0, |c| self.order.len() - c - 1)
    }

    /// (position of the current track, total), 1-based for display.
    pub fn position(&self) -> Option<(usize, usize)> {
        Some((self.cursor? + 1, self.order.len()))
    }

    /// Tracks in play order, with the index of the current one.
    pub fn in_order(&self) -> (Vec<Track>, Option<usize>) {
        (self.order.iter().map(|&i| self.tracks[i].clone()).collect(), self.cursor)
    }

    fn mark_played(&mut self) {
        if let Some(id) = self.current().map(|t| t.id) {
            self.played.insert(id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track(id: u64) -> Track {
        serde_json::from_value(serde_json::json!({
            "id": id, "title": format!("t{id}"), "duration": 1000,
            "user": {"id": 1, "username": "u"}
        }))
        .unwrap()
    }

    fn ids(q: &Queue) -> Vec<u64> {
        q.in_order().0.iter().map(|t| t.id).collect()
    }

    #[test]
    fn plays_in_order_then_stops() {
        let mut q = Queue::default();
        q.set((1..=3).map(track).collect(), 1);
        assert_eq!(q.current().unwrap().id, 2);
        assert_eq!(q.advance(true).unwrap().id, 3);
        assert!(q.advance(true).is_none());
        assert_eq!(q.back().unwrap().id, 2);
    }

    #[test]
    fn repeat_modes() {
        let mut q = Queue::default();
        q.set((1..=2).map(track).collect(), 1);
        q.repeat = Repeat::All;
        assert_eq!(q.advance(true).unwrap().id, 1, "wraps around");
        q.repeat = Repeat::One;
        assert_eq!(q.advance(true).unwrap().id, 1, "replays when it ends");
        assert_eq!(q.advance(false).unwrap().id, 2, "manual skip still skips");
    }

    #[test]
    fn shuffle_keeps_current_and_everything() {
        let mut q = Queue::default();
        q.set((1..=50).map(track).collect(), 10);
        q.toggle_shuffle();
        assert_eq!(q.current().unwrap().id, 11);
        let mut all = ids(&q);
        assert_ne!(all, (1..=50).collect::<Vec<_>>(), "order changed");
        all.sort();
        assert_eq!(all, (1..=50).collect::<Vec<_>>(), "nothing lost");
        q.toggle_shuffle();
        assert_eq!(ids(&q), (1..=50).collect::<Vec<_>>());
        assert_eq!(q.current().unwrap().id, 11);
    }

    #[test]
    fn queue_editing() {
        let mut q = Queue::default();
        q.set((1..=3).map(track).collect(), 0);
        q.push_next(track(9));
        q.push(track(8));
        assert_eq!(ids(&q), [1, 9, 2, 3, 8]);
        assert!(!q.remove(0), "can't remove the current track");
        assert!(q.remove(1));
        assert_eq!(q.advance(false).unwrap().id, 2);
    }

    #[test]
    fn radio_skips_repeats() {
        let mut q = Queue::default();
        q.set((1..=2).map(track).collect(), 0);
        q.advance(true);
        assert_eq!(q.extend_radio(vec![track(1), track(2), track(3), track(3)]), 1);
        assert_eq!(ids(&q), [1, 2, 3]);
    }
}
