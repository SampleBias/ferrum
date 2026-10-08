//! Identity for bytes charged to an [`ArenaLedger`](crate::resources::ArenaLedger).
//!
//! `ArenaLedger::evict_cache` subtracts a counter. That can free bytes no entry
//! owns. This table is the path a guest uses instead: an owner release frees
//! one charge, and eviction frees only cache entries that are evictable and
//! not live.

use crate::resources::{ArenaLedger, Pool, ResourceError};

pub const ENTRY_SLOTS: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EntryId(u32);

impl EntryId {
    pub const fn get(self) -> u32 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntryError {
    Full,
    Unknown,
    Resource(ResourceError),
}

#[derive(Clone, Copy)]
struct Slot {
    occupied: bool,
    id: u32,
    pool: Pool,
    bytes: u64,
    live: bool,
    evictable: bool,
}

const fn vacant() -> Slot {
    Slot {
        occupied: false,
        id: 0,
        pool: Pool::Cache,
        bytes: 0,
        live: false,
        evictable: false,
    }
}

#[derive(Clone, Copy)]
pub struct EntryTable {
    next_id: u32,
    slots: [Slot; ENTRY_SLOTS],
}

impl EntryTable {
    pub const fn empty() -> Self {
        Self {
            next_id: 1,
            slots: [vacant(); ENTRY_SLOTS],
        }
    }

    pub fn insert(
        &mut self,
        ledger: &mut ArenaLedger,
        pool: Pool,
        bytes: u64,
        evictable: bool,
    ) -> Result<EntryId, EntryError> {
        let index = self.free_slot().ok_or(EntryError::Full)?;
        ledger.try_alloc(pool, bytes).map_err(EntryError::Resource)?;
        let id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);
        self.slots[index] = Slot {
            occupied: true,
            id,
            pool,
            bytes,
            live: true,
            evictable,
        };
        Ok(EntryId(id))
    }

    pub fn set_live(&mut self, id: EntryId, live: bool) -> Result<(), EntryError> {
        let slot = self.slot_mut(id)?;
        slot.live = live;
        Ok(())
    }

    /// Owner release. A live entry may be freed here, because the owner is
    /// the one dropping it. The ledger charge reverses exactly once.
    pub fn release(&mut self, ledger: &mut ArenaLedger, id: EntryId) -> Result<(), EntryError> {
        let index = self.index(id)?;
        let slot = self.slots[index];
        ledger
            .free(slot.pool, slot.bytes)
            .map_err(EntryError::Resource)?;
        self.slots[index] = vacant();
        Ok(())
    }

    /// Free evictable cache entries that are not live, oldest slot first,
    /// until `max_bytes` have been released or no such entry remains.
    /// Latency and batch entries are left in place.
    pub fn evict(&mut self, ledger: &mut ArenaLedger, max_bytes: u64) -> Result<u64, EntryError> {
        let mut freed = 0u64;
        for index in 0..ENTRY_SLOTS {
            if freed >= max_bytes {
                break;
            }
            let slot = self.slots[index];
            if !slot.occupied || slot.live || !slot.evictable || slot.pool != Pool::Cache {
                continue;
            }
            if freed.saturating_add(slot.bytes) > max_bytes {
                continue;
            }
            ledger
                .free(slot.pool, slot.bytes)
                .map_err(EntryError::Resource)?;
            freed = freed.saturating_add(slot.bytes);
            self.slots[index] = vacant();
        }
        Ok(freed)
    }

    pub fn charged(&self, pool: Pool) -> u64 {
        self.slots
            .iter()
            .filter(|slot| slot.occupied && slot.pool == pool)
            .fold(0u64, |sum, slot| sum.saturating_add(slot.bytes))
    }

    fn free_slot(&self) -> Option<usize> {
        self.slots.iter().position(|slot| !slot.occupied)
    }

    fn index(&self, id: EntryId) -> Result<usize, EntryError> {
        self.slots
            .iter()
            .position(|slot| slot.occupied && slot.id == id.0)
            .ok_or(EntryError::Unknown)
    }

    fn slot_mut(&mut self, id: EntryId) -> Result<&mut Slot, EntryError> {
        let index = self.index(id)?;
        Ok(&mut self.slots[index])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ledger() -> ArenaLedger {
        ArenaLedger::new(64 * 1024 * 1024)
    }

    #[test]
    fn eviction_frees_released_cache_entries_and_leaves_live_arenas() {
        let mut entries = EntryTable::empty();
        let mut arenas = ledger();
        let latency = entries
            .insert(&mut arenas, Pool::Latency, 4096, false)
            .unwrap();
        let cache = entries
            .insert(&mut arenas, Pool::Cache, 8 * 1024 * 1024, true)
            .unwrap();
        entries.set_live(cache, false).unwrap();
        arenas.set_cache_target(1024);

        let freed = entries.evict(&mut arenas, 4 * 1024 * 1024).unwrap();
        assert_eq!(freed, 0, "an 8 MiB entry does not fit a 4 MiB batch");
        assert_eq!(entries.charged(Pool::Cache), 8 * 1024 * 1024);

        let freed = entries.evict(&mut arenas, 8 * 1024 * 1024).unwrap();
        assert_eq!(freed, 8 * 1024 * 1024);
        assert_eq!(entries.charged(Pool::Cache), 0);
        assert_eq!(entries.charged(Pool::Latency), 4096);
        assert_eq!(arenas.eviction_backlog(), 0);
        entries.release(&mut arenas, latency).unwrap();
        assert_eq!(arenas.latency_used, 0);
    }

    #[test]
    fn a_live_cache_entry_is_not_evicted() {
        let mut entries = EntryTable::empty();
        let mut arenas = ledger();
        let cache = entries
            .insert(&mut arenas, Pool::Cache, 1024, true)
            .unwrap();
        assert!(entries.set_live(cache, true).is_ok());
        arenas.set_cache_target(0);
        assert_eq!(entries.evict(&mut arenas, 4096).unwrap(), 0);
        assert_eq!(entries.charged(Pool::Cache), 1024);
        entries.release(&mut arenas, cache).unwrap();
        assert_eq!(arenas.cache_used, 0);
        assert_eq!(entries.release(&mut arenas, cache), Err(EntryError::Unknown));
    }

    #[test]
    fn insert_does_not_charge_when_the_table_or_the_cap_is_full() {
        let mut entries = EntryTable::empty();
        let mut arenas = ledger();
        for _ in 0..ENTRY_SLOTS {
            entries
                .insert(&mut arenas, Pool::Cache, 1, true)
                .unwrap();
        }
        let used = arenas.cache_used;
        assert_eq!(
            entries.insert(&mut arenas, Pool::Cache, 1, true),
            Err(EntryError::Full)
        );
        assert_eq!(arenas.cache_used, used);

        let mut entries = EntryTable::empty();
        let mut arenas = ledger();
        assert!(matches!(
            entries.insert(&mut arenas, Pool::Latency, 64 * 1024 * 1024 + 1, false),
            Err(EntryError::Resource(ResourceError::Cap))
        ));
        assert_eq!(entries.charged(Pool::Latency), 0);
        assert_eq!(arenas.latency_used, 0);
    }
}
