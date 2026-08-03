use std::collections::HashSet;

#[derive(Debug)]
pub(crate) struct OAuthStateReplayWindow {
    capacity: usize,
    values: HashSet<String>,
    slots: Vec<Option<String>>,
    cursor: usize,
}
impl OAuthStateReplayWindow {
    pub(crate) fn new(capacity: usize) -> Result<Self, &'static str> {
        if capacity == 0 {
            return Err("OAuth replay window capacity must be positive");
        }
        Ok(Self {
            capacity,
            values: HashSet::with_capacity(capacity),
            slots: vec![None; capacity],
            cursor: 0,
        })
    }

    pub(crate) fn consume(&mut self, state: &str) -> bool {
        if self.values.contains(state) {
            return false;
        }
        if let Some(evicted) = self.slots[self.cursor].take() {
            self.values.remove(&evicted);
        }
        self.slots[self.cursor] = Some(state.to_owned());
        self.values.insert(state.to_owned());
        self.cursor += 1;
        if self.cursor == self.capacity {
            self.cursor = 0;
        }
        true
    }

    #[cfg(test)]
    #[must_use]
    pub(crate) fn len(&self) -> usize {
        self.values.len()
    }

    #[cfg(test)]
    #[must_use]
    pub(crate) fn is_empty(&self) -> bool {
        self.values.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::OAuthStateReplayWindow;

    #[test]
    fn replay_window_rejects_duplicates_and_evicts_oldest_slot() -> Result<(), &'static str> {
        let mut window = OAuthStateReplayWindow::new(2)?;
        assert!(window.is_empty());
        assert!(window.consume("a"));
        assert!(!window.consume("a"));
        assert!(window.consume("b"));
        assert_eq!(window.len(), 2);
        assert!(window.consume("c"));
        assert!(window.consume("a"));
        Ok(())
    }
}
