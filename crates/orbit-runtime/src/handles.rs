//! A generic per-instance handle table (ADR-0006).
//!
//! Handles given to the guest are positive `i64` values that encode a slot index, a
//! generation and a per-table tag; they are never host pointers. A closed handle, a handle
//! whose slot was reused (stale generation) and a handle minted by another instance's table
//! (different tag, and a different table anyway) all fail to resolve.
//!
//! Layout (bit 63 is always 0 so a handle is positive): bits 0..16 slot index,
//! bits 16..48 generation (never 0), bits 48..63 tag.

const INDEX_BITS: u32 = 16;
const GEN_BITS: u32 = 32;
const GEN_MASK: u64 = (1 << GEN_BITS) - 1;
const TAG_MASK: u64 = (1 << 15) - 1;

/// Largest number of simultaneously open handles a table can be configured for.
pub const MAX_HANDLES: usize = 1 << INDEX_BITS;

#[derive(Debug)]
struct Slot<T> {
    generation: u32,
    value: Option<T>,
}

/// The table is full.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TableFull;

/// A bounded table of values addressed by opaque, generation-checked handles.
#[derive(Debug)]
pub struct HandleTable<T> {
    tag: u64,
    max_live: usize,
    live: usize,
    slots: Vec<Slot<T>>,
    free: Vec<u32>,
}

impl<T> HandleTable<T> {
    /// A table holding at most `max_live` (clamped to [`MAX_HANDLES`]) values. `tag` (for
    /// example the instance id) is mixed into every handle so handles of different tables
    /// differ even when slot and generation coincide.
    #[must_use]
    pub fn new(tag: u64, max_live: usize) -> Self {
        Self {
            tag: tag & TAG_MASK,
            max_live: max_live.min(MAX_HANDLES),
            live: 0,
            slots: Vec::new(),
            free: Vec::new(),
        }
    }

    fn encode(&self, index: u32, generation: u32) -> i64 {
        ((self.tag << (INDEX_BITS + GEN_BITS))
            | (u64::from(generation) << INDEX_BITS)
            | u64::from(index)) as i64
    }

    /// Returns the slot index for a handle that has the right tag and generation.
    fn decode(&self, handle: i64) -> Option<usize> {
        let h = u64::try_from(handle).ok()?;
        if (h >> (INDEX_BITS + GEN_BITS)) != self.tag {
            return None;
        }
        let index = (h & ((1 << INDEX_BITS) - 1)) as usize;
        let generation = ((h >> INDEX_BITS) & GEN_MASK) as u32;
        let slot = self.slots.get(index)?;
        (slot.value.is_some() && slot.generation == generation).then_some(index)
    }

    /// Stores `value` and returns its handle.
    ///
    /// # Errors
    /// [`TableFull`] if `max_live` values are already open.
    pub fn insert(&mut self, value: T) -> Result<i64, TableFull> {
        if self.live >= self.max_live {
            return Err(TableFull);
        }
        let index = if let Some(i) = self.free.pop() {
            i
        } else {
            self.slots.push(Slot {
                generation: 0,
                value: None,
            });
            (self.slots.len() - 1) as u32
        };
        let slot = &mut self.slots[index as usize];
        // Generations start at 1 and skip 0 when they wrap.
        slot.generation = slot.generation.wrapping_add(1).max(1);
        slot.value = Some(value);
        let generation = slot.generation;
        self.live += 1;
        Ok(self.encode(index, generation))
    }

    /// Resolves a handle.
    #[must_use]
    pub fn get(&self, handle: i64) -> Option<&T> {
        self.slots[self.decode(handle)?].value.as_ref()
    }

    /// Closes a handle and returns its value.
    pub fn remove(&mut self, handle: i64) -> Option<T> {
        let index = self.decode(handle)?;
        let value = self.slots[index].value.take();
        self.free.push(index as u32);
        self.live -= 1;
        value
    }

    /// Number of open handles.
    #[must_use]
    pub fn len(&self) -> usize {
        self.live
    }

    /// Whether no handle is open.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.live == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handles_are_positive_and_checked() {
        let mut t = HandleTable::new(7, 2);
        let a = t.insert("a").unwrap();
        let b = t.insert("b").unwrap();
        assert!(a > 0 && b > 0 && a != b);
        assert_eq!(t.insert("c"), Err(TableFull));
        assert_eq!(t.get(a), Some(&"a"));
        assert_eq!(t.remove(a), Some("a"));
        assert_eq!(t.get(a), None);
        assert_eq!(t.remove(a), None);
        // Same slot reused: the old handle must not alias the new one.
        let c = t.insert("c").unwrap();
        assert_ne!(a, c);
        assert_eq!(t.get(a), None);
        assert_eq!(t.get(c), Some(&"c"));
        // Garbage.
        for bad in [0, -1, i64::MAX, i64::MIN, 12345] {
            assert_eq!(t.get(bad), None);
        }
    }

    #[test]
    fn tag_separates_tables() {
        let mut a = HandleTable::new(1, 4);
        let mut b = HandleTable::new(2, 4);
        let ha = a.insert(()).unwrap();
        let hb = b.insert(()).unwrap();
        assert_ne!(ha, hb);
        assert!(b.get(ha).is_none() && a.get(hb).is_none());
    }
}
