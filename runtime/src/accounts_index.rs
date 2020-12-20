use crate::inline_spl_token_v2_0;
use dashmap::{DashMap, DashSet};
use ouroboros::self_referencing;
use solana_sdk::{
    clock::Slot,
    pubkey::{Pubkey, PUBKEY_BYTES},
};
use std::ops::{
    Bound,
    Bound::{Excluded, Included, Unbounded},
};
use std::sync::atomic::{AtomicU64, Ordering};
use std::{
    collections::{
        btree_map::{self, BTreeMap},
        HashMap, HashSet,
    },
    ops::{Range, RangeBounds},
    sync::{Arc, RwLock, RwLockReadGuard, RwLockWriteGuard},
};
pub const ITER_BATCH_SIZE: usize = 1000;

pub type SlotList<T> = Vec<(Slot, T)>;
pub type SlotSlice<'s, T> = &'s [(Slot, T)];
pub type Ancestors = HashMap<Slot, usize>;

pub type RefCount = u64;
pub type AccountMap<K, V> = BTreeMap<K, V>;

type AccountMapEntry<T> = Arc<AccountMapEntryInner<T>>;
type MintIndexEntry = DashSet<Pubkey>;

enum ScanTypes<R: RangeBounds<Pubkey>> {
    Standard(Option<R>),
    Indexed(IndexKey),
}

#[derive(Debug, Clone, Copy)]
pub enum IndexKey {
    ProgramId(Pubkey),
    TokenOwner(Pubkey),
    Mint(Pubkey),
}

#[derive(Debug)]
pub struct AccountMapEntryInner<T> {
    ref_count: AtomicU64,
    pub slot_list: RwLock<SlotList<T>>,
}

#[self_referencing]
pub struct ReadAccountMapEntry<T: 'static> {
    owned_entry: AccountMapEntry<T>,
    #[borrows(owned_entry)]
    slot_list_guard: RwLockReadGuard<'this, SlotList<T>>,
}

impl<T: Clone> ReadAccountMapEntry<T> {
    pub fn from_account_map_entry(account_map_entry: AccountMapEntry<T>) -> Self {
        ReadAccountMapEntryBuilder {
            owned_entry: account_map_entry,
            slot_list_guard_builder: |lock| lock.slot_list.read().unwrap(),
        }
        .build()
    }

    pub fn slot_list(&self) -> &SlotList<T> {
        &*self.borrow_slot_list_guard()
    }

    pub fn ref_count(&self) -> &AtomicU64 {
        &self.borrow_owned_entry_contents().ref_count
    }
}

#[self_referencing]
pub struct WriteAccountMapEntry<T: 'static> {
    owned_entry: AccountMapEntry<T>,
    #[borrows(owned_entry)]
    slot_list_guard: RwLockWriteGuard<'this, SlotList<T>>,
}

impl<T: 'static + Clone> WriteAccountMapEntry<T> {
    pub fn from_account_map_entry(account_map_entry: AccountMapEntry<T>) -> Self {
        WriteAccountMapEntryBuilder {
            owned_entry: account_map_entry,
            slot_list_guard_builder: |lock| lock.slot_list.write().unwrap(),
        }
        .build()
    }

    pub fn slot_list(&mut self) -> &SlotList<T> {
        &*self.borrow_slot_list_guard()
    }

    pub fn slot_list_mut<RT>(
        &mut self,
        user: impl for<'this> FnOnce(&mut RwLockWriteGuard<'this, SlotList<T>>) -> RT,
    ) -> RT {
        self.with_slot_list_guard_mut(user)
    }

    pub fn ref_count(&self) -> &AtomicU64 {
        &self.borrow_owned_entry_contents().ref_count
    }

    // Try to update an item in the slot list the given `slot` If an item for the slot
    // already exists in the list, remove the older item, add it to `reclaims`, and insert
    // the new item.
    pub fn update(&mut self, slot: Slot, account_info: T, reclaims: &mut SlotList<T>) {
        // filter out other dirty entries from the same slot
        let mut same_slot_previous_updates: Vec<(usize, &(Slot, T))> = self
            .slot_list()
            .iter()
            .enumerate()
            .filter(|(_, (s, _))| *s == slot)
            .collect();
        assert!(same_slot_previous_updates.len() <= 1);
        if let Some((list_index, (s, previous_update_value))) = same_slot_previous_updates.pop() {
            reclaims.push((*s, previous_update_value.clone()));
            self.slot_list_mut(|list| list.remove(list_index));
        } else {
            // Only increment ref count if the account was not prevously updated in this slot
            self.ref_count().fetch_add(1, Ordering::Relaxed);
        }
        self.slot_list_mut(|list| list.push((slot, account_info)));
    }
}

#[derive(Debug, Default)]
pub struct RootsTracker {
    roots: HashSet<Slot>,
    max_root: Slot,
    uncleaned_roots: HashSet<Slot>,
    previous_uncleaned_roots: HashSet<Slot>,
}

pub struct AccountsIndexIterator<'a, T> {
    account_maps: &'a RwLock<AccountMap<Pubkey, AccountMapEntry<T>>>,
    start_bound: Bound<Pubkey>,
    end_bound: Bound<Pubkey>,
    is_finished: bool,
}

impl<'a, T> AccountsIndexIterator<'a, T> {
    fn clone_bound(bound: Bound<&Pubkey>) -> Bound<Pubkey> {
        match bound {
            Unbounded => Unbounded,
            Included(k) => Included(*k),
            Excluded(k) => Excluded(*k),
        }
    }

    pub fn new<R>(
        account_maps: &'a RwLock<AccountMap<Pubkey, AccountMapEntry<T>>>,
        range: Option<R>,
    ) -> Self
    where
        R: RangeBounds<Pubkey>,
    {
        Self {
            start_bound: range
                .as_ref()
                .map(|r| Self::clone_bound(r.start_bound()))
                .unwrap_or(Unbounded),
            end_bound: range
                .as_ref()
                .map(|r| Self::clone_bound(r.end_bound()))
                .unwrap_or(Unbounded),
            account_maps,
            is_finished: false,
        }
    }
}

impl<'a, T: 'static + Clone> Iterator for AccountsIndexIterator<'a, T> {
    type Item = Vec<(Pubkey, AccountMapEntry<T>)>;
    fn next(&mut self) -> Option<Self::Item> {
        if self.is_finished {
            return None;
        }

        let chunk: Vec<(Pubkey, AccountMapEntry<T>)> = self
            .account_maps
            .read()
            .unwrap()
            .range((self.start_bound, self.end_bound))
            .map(|(pubkey, account_map_entry)| (*pubkey, account_map_entry.clone()))
            .take(ITER_BATCH_SIZE)
            .collect();

        if chunk.is_empty() {
            self.is_finished = true;
            return None;
        }

        self.start_bound = Excluded(chunk.last().unwrap().0);
        Some(chunk)
    }
}

#[derive(Debug, Default)]
pub struct MintIndex {
    index: DashMap<Pubkey, MintIndexEntry>,
    // Map from pubkey to the mint it's stored in, used for cleanup.
    // Alternative is to store Option<Pubkey> in each AccountInfo in the
    // AccountsIndex if something is an SPL account with a mint, but then
    // every AccountInfo would have to allocate `Option<Pubkey>`
    accounts_to_mints: DashMap<Pubkey, DashSet<Pubkey>>,
}

#[derive(Debug, Default)]
struct SecondaryIndex {
    // Map from index keys to index values
    index: DashMap<Pubkey, DashSet<Pubkey>>,
    // Map from index values back to index keys, used for cleanup.
    reverse_index: DashMap<Pubkey, DashSet<Pubkey>>,
}

impl SecondaryIndex {
    fn insert(&self, key: &Pubkey, value: &Pubkey) {
        {
            let values = self
                .index
                .get(key)
                .unwrap_or_else(|| self.index.entry(*key).or_insert(DashSet::new()).downgrade());
            values.insert(*value);
        }

        {
            let keys = self.reverse_index.get(value).unwrap_or_else(|| {
                self.reverse_index
                    .entry(*value)
                    .or_insert(DashSet::new())
                    .downgrade()
            });
            keys.insert(*key);
        }
    }

    fn remove(&self, value: &Pubkey) {
        // Get all keys that this value was listed in
        let keys: Option<Vec<Pubkey>> = self
            .reverse_index
            .remove(value)
            .map(|(_, keys)| keys.into_iter().collect());

        // Remove this value from those keys
        if let Some(keys) = keys {
            for key in keys {
                self.index.remove_if(&key, |_, values| {
                    values.remove(&value);
                    values.is_empty()
                });
            }
        }
    }

    fn get(&self, key: &Pubkey) -> Vec<Pubkey> {
        if let Some(entry) = self.index.get(key) {
            entry.value().iter().map(|pubkey_ref| *pubkey_ref).collect()
        } else {
            vec![]
        }
    }
}

#[derive(Debug, Default)]
pub struct AccountsIndex<T> {
    pub account_maps: RwLock<AccountMap<Pubkey, AccountMapEntry<T>>>,
    program_id_index: SecondaryIndex,
    token_owner_index: SecondaryIndex,
    mint_index: SecondaryIndex,
    roots_tracker: RwLock<RootsTracker>,
    ongoing_scan_roots: RwLock<BTreeMap<Slot, u64>>,
}

impl<T: 'static + Clone> AccountsIndex<T> {
    fn iter<R>(&self, range: Option<R>) -> AccountsIndexIterator<T>
    where
        R: RangeBounds<Pubkey>,
    {
        AccountsIndexIterator::new(&self.account_maps, range)
    }

    fn do_checked_scan_accounts<'a, F, R>(
        &'a self,
        ancestors: &Ancestors,
        func: F,
        scan_type: ScanTypes<R>,
    ) where
        F: FnMut(&Pubkey, (&T, Slot)),
        R: RangeBounds<Pubkey>,
    {
        let max_root = {
            let mut w_ongoing_scan_roots = self
                // This lock is also grabbed by clean_accounts(), so clean
                // has at most cleaned up to the current `max_root` (since
                // clean only happens *after* BankForks::set_root() which sets
                // the `max_root`)
                .ongoing_scan_roots
                .write()
                .unwrap();
            // `max_root()` grabs a lock while
            // the `ongoing_scan_roots` lock is held,
            // make sure inverse doesn't happen to avoid
            // deadlock
            let max_root = self.max_root();
            *w_ongoing_scan_roots.entry(max_root).or_default() += 1;

            max_root
        };

        // First we show that for any bank `B` that is a descendant of
        // the current `max_root`, it must be true that and `B.ancestors.contains(max_root)`,
        // regardless of the pattern of `squash()` behavior, `where` `ancestors` is the set
        // of ancestors that is tracked in each bank.
        //
        // Proof: At startup, if starting from a snapshot, generate_index() adds all banks
        // in the snapshot to the index via `add_root()` and so `max_root` will be the
        // greatest of these. Thus, so the claim holds at startup since there are no
        // descendants of `max_root`.
        //
        // Now we proceed by induction on each `BankForks::set_root()`.
        // Assume the claim holds when the `max_root` is `R`. Call the set of
        // descendants of `R` present in BankForks `R_descendants`.
        //
        // Then for any banks `B` in `R_descendants`, it must be that `B.ancestors.contains(S)`,
        // where `S` is any ancestor of `B` such that `S >= R`.
        //
        // For example:
        //          `R` -> `A` -> `C` -> `B`
        // Then `B.ancestors == {R, A, C}`
        //
        // Next we call `BankForks::set_root()` at some descendant of `R`, `R_new`,
        // where `R_new > R`.
        //
        // When we squash `R_new`, `max_root` in the AccountsIndex here is now set to `R_new`,
        // and all nondescendants of `R_new` are pruned.
        //
        // Now consider any outstanding references to banks in the system that are descended from
        // `max_root == R_new`. Take any one of these references and call it `B`. Because `B` is
        // a descendant of `R_new`, this means `B` was also a descendant of `R`. Thus `B`
        // must be a member of `R_descendants` because `B` was constructed and added to
        // BankForks before the `set_root`.
        //
        // This means by the guarantees of `R_descendants` described above, because
        // `R_new` is an ancestor of `B`, and `R < R_new < B`, then B.ancestors.contains(R_new)`.
        //
        // Now until the next `set_root`, any new banks constructed from `new_from_parent` will
        // also have `max_root == R_new` in their ancestor set, so the claim holds for those descendants
        // as well. Once the next `set_root` happens, we once again update `max_root` and the same
        // inductive argument can be applied again to show the claim holds.

        // Check that the `max_root` is present in `ancestors`. From the proof above, if
        // `max_root` is not present in `ancestors`, this means the bank `B` with the
        // given `ancestors` is not descended from `max_root, which means
        // either:
        // 1) `B` is on a different fork or
        // 2) `B` is an ancestor of `max_root`.
        // In both cases we can ignore the given ancestors and instead just rely on the roots
        // present as `max_root` indicates the roots present in the index are more up to date
        // than the ancestors given.
        let empty = HashMap::new();
        let ancestors = if ancestors.contains_key(&max_root) {
            ancestors
        } else {
            /*
            This takes of edge cases like:

            Diagram 1:

                        slot 0
                          |
                        slot 1
                      /        \
                 slot 2         |
                    |       slot 3 (max root)
            slot 4 (scan)

            By the time the scan on slot 4 is called, slot 2 may already have been
            cleaned by a clean on slot 3, but slot 4 may not have been cleaned.
            The state in slot 2 would have been purged and is not saved in any roots.
            In this case, a scan on slot 4 wouldn't accurately reflect the state when bank 4
            was frozen. In cases like this, we default to a scan on the latest roots by
            removing all `ancestors`.
            */
            &empty
        };

        /*
        Now there are two cases, either `ancestors` is empty or nonempty:

        1) If ancestors is empty, then this is the same as a scan on a rooted bank,
        and `ongoing_scan_roots` provides protection against cleanup of roots necessary
        for the scan, and  passing `Some(max_root)` to `do_scan_accounts()` ensures newer
        roots don't appear in the scan.

        2) If ancestors is non-empty, then from the `ancestors_contains(&max_root)` above, we know
        that the fork structure must look something like:

        Diagram 2:

                Build fork structure:
                        slot 0
                          |
                    slot 1 (max_root)
                    /            \
             slot 2              |
                |            slot 3 (potential newer max root)
              slot 4
                |
             slot 5 (scan)

        Consider both types of ancestors, ancestor <= `max_root` and
        ancestor > `max_root`, where `max_root == 1` as illustrated above.

        a) The set of `ancestors <= max_root` are all rooted, which means their state
        is protected by the same guarantees as 1).

        b) As for the `ancestors > max_root`, those banks have at least one reference discoverable
        through the chain of `Bank::BankRc::parent` starting from the calling bank. For instance
        bank 5's parent reference keeps bank 4 alive, which will prevent the `Bank::drop()` from
        running and cleaning up bank 4. Furthermore, no cleans can happen past the saved max_root == 1,
        so a potential newer max root at 3 will not clean up any of the ancestors > 1, so slot 4
        will not be cleaned in the middle of the scan either.
        */
        match scan_type {
            ScanTypes::Standard(range) => {
                self.do_scan_accounts(ancestors, func, range, Some(max_root));
            }
            ScanTypes::Indexed(IndexKey::ProgramId(program_id)) => {
                self.do_scan_secondary_index(
                    ancestors,
                    func,
                    &self.program_id_index,
                    &program_id,
                    Some(max_root),
                );
            }
            ScanTypes::Indexed(IndexKey::TokenOwner(owner_key)) => {
                self.do_scan_secondary_index(
                    ancestors,
                    func,
                    &self.token_owner_index,
                    &owner_key,
                    Some(max_root),
                );
            }
            ScanTypes::Indexed(IndexKey::Mint(mint_key)) => {
                self.do_scan_secondary_index(
                    ancestors,
                    func,
                    &self.mint_index,
                    &mint_key,
                    Some(max_root),
                );
            }
        }

        {
            let mut ongoing_scan_roots = self.ongoing_scan_roots.write().unwrap();
            let count = ongoing_scan_roots.get_mut(&max_root).unwrap();
            *count -= 1;
            if *count == 0 {
                ongoing_scan_roots.remove(&max_root);
            }
        }
    }

    fn do_unchecked_scan_accounts<'a, F, R>(
        &'a self,
        ancestors: &Ancestors,
        func: F,
        range: Option<R>,
    ) where
        F: FnMut(&Pubkey, (&T, Slot)),
        R: RangeBounds<Pubkey>,
    {
        self.do_scan_accounts(ancestors, func, range, None);
    }

    // Scan accounts and return latest version of each account that is either:
    // 1) rooted or
    // 2) present in ancestors
    fn do_scan_accounts<'a, F, R>(
        &'a self,
        ancestors: &Ancestors,
        mut func: F,
        range: Option<R>,
        max_root: Option<Slot>,
    ) where
        F: FnMut(&Pubkey, (&T, Slot)),
        R: RangeBounds<Pubkey>,
    {
        // TODO: expand to use mint index to find the `pubkey_list` below more efficiently
        // instead of scanning the entire range
        for pubkey_list in self.iter(range) {
            for (pubkey, list) in pubkey_list {
                let list_r = &list.slot_list.read().unwrap();
                if let Some(index) = self.latest_slot(Some(ancestors), &list_r, max_root) {
                    func(&pubkey, (&list_r[index].1, list_r[index].0));
                }
            }
        }
    }

    fn do_scan_secondary_index<F>(
        &self,
        ancestors: &Ancestors,
        mut func: F,
        index: &SecondaryIndex,
        index_key: &Pubkey,
        max_root: Option<Slot>,
    ) where
        F: FnMut(&Pubkey, (&T, Slot)),
    {
        for pubkey in index.get(index_key) {
            // Maybe these reads from the AccountsIndex can be batched everytime it
            // grabs the read lock as well...
            if let Some((list_r, index)) = self.get(&pubkey, Some(ancestors), max_root) {
                func(
                    &pubkey,
                    (&list_r.slot_list()[index].1, list_r.slot_list()[index].0),
                );
            }
        }
    }

    pub fn get_account_read_entry(&self, pubkey: &Pubkey) -> Option<ReadAccountMapEntry<T>> {
        self.account_maps
            .read()
            .unwrap()
            .get(pubkey)
            .cloned()
            .map(ReadAccountMapEntry::from_account_map_entry)
    }

    fn get_account_write_entry(&self, pubkey: &Pubkey) -> Option<WriteAccountMapEntry<T>> {
        self.account_maps
            .read()
            .unwrap()
            .get(pubkey)
            .cloned()
            .map(WriteAccountMapEntry::from_account_map_entry)
    }

    fn get_account_write_entry_else_create(
        &self,
        pubkey: &Pubkey,
    ) -> (WriteAccountMapEntry<T>, bool) {
        let mut w_account_entry = self.get_account_write_entry(pubkey);
        let mut is_newly_inserted = false;
        if w_account_entry.is_none() {
            let new_entry = Arc::new(AccountMapEntryInner {
                ref_count: AtomicU64::new(0),
                slot_list: RwLock::new(SlotList::with_capacity(32)),
            });
            let mut w_account_maps = self.account_maps.write().unwrap();
            let account_entry = w_account_maps.entry(*pubkey).or_insert_with(|| {
                is_newly_inserted = true;
                new_entry
            });
            w_account_entry = Some(WriteAccountMapEntry::from_account_map_entry(
                account_entry.clone(),
            ));
        }

        (w_account_entry.unwrap(), is_newly_inserted)
    }

    pub fn handle_dead_keys(&self, dead_keys: &[Pubkey]) {
        if !dead_keys.is_empty() {
            for key in dead_keys.iter() {
                let mut w_index = self.account_maps.write().unwrap();
                if let btree_map::Entry::Occupied(index_entry) = w_index.entry(*key) {
                    if index_entry.get().slot_list.read().unwrap().is_empty() {
                        index_entry.remove();

                        self.program_id_index.remove(key);
                        self.token_owner_index.remove(key);
                        self.mint_index.remove(key);
                    }
                }
            }
        }
    }

    /// call func with every pubkey and index visible from a given set of ancestors
    pub(crate) fn scan_accounts<F>(&self, ancestors: &Ancestors, func: F)
    where
        F: FnMut(&Pubkey, (&T, Slot)),
    {
        self.do_checked_scan_accounts(ancestors, func, ScanTypes::Standard(None::<Range<Pubkey>>));
    }

    pub(crate) fn unchecked_scan_accounts<F>(&self, ancestors: &Ancestors, func: F)
    where
        F: FnMut(&Pubkey, (&T, Slot)),
    {
        self.do_unchecked_scan_accounts(ancestors, func, None::<Range<Pubkey>>);
    }

    /// call func with every pubkey and index visible from a given set of ancestors with range
    pub(crate) fn range_scan_accounts<F, R>(&self, ancestors: &Ancestors, range: R, func: F)
    where
        F: FnMut(&Pubkey, (&T, Slot)),
        R: RangeBounds<Pubkey>,
    {
        // Only the rent logic should be calling this, which doesn't need the safety checks
        self.do_unchecked_scan_accounts(ancestors, func, Some(range));
    }

    /// call func with every pubkey and index visible from a given set of ancestors
    pub(crate) fn index_scan_accounts<F>(&self, ancestors: &Ancestors, index_key: IndexKey, func: F)
    where
        F: FnMut(&Pubkey, (&T, Slot)),
    {
        self.do_checked_scan_accounts(
            ancestors,
            func,
            ScanTypes::<Range<Pubkey>>::Indexed(index_key),
        );
    }

    pub fn get_rooted_entries(&self, slice: SlotSlice<T>, max: Option<Slot>) -> SlotList<T> {
        slice
            .iter()
            .filter(|(slot, _)| self.is_root(*slot) && max.map_or(true, |max| *slot <= max))
            .cloned()
            .collect()
    }

    // returns the rooted entries and the storage ref count
    pub fn roots_and_ref_count(
        &self,
        locked_account_entry: &ReadAccountMapEntry<T>,
        max: Option<Slot>,
    ) -> (SlotList<T>, RefCount) {
        (
            self.get_rooted_entries(&locked_account_entry.slot_list(), max),
            locked_account_entry.ref_count().load(Ordering::Relaxed),
        )
    }

    // filter any rooted entries and return them along with a bool that indicates
    // if this account has no more entries.
    pub fn purge(&self, pubkey: &Pubkey) -> (SlotList<T>, bool) {
        let mut write_account_map_entry = self.get_account_write_entry(pubkey).unwrap();
        write_account_map_entry.slot_list_mut(|slot_list| {
            let reclaims = self.get_rooted_entries(slot_list, None);
            slot_list.retain(|(slot, _)| !self.is_root(*slot));
            (reclaims, slot_list.is_empty())
        })
    }

    pub fn purge_exact(&self, pubkey: &Pubkey, slots: HashSet<Slot>) -> (SlotList<T>, bool) {
        let mut write_account_map_entry = self.get_account_write_entry(pubkey).unwrap();
        write_account_map_entry.slot_list_mut(|slot_list| {
            let reclaims = slot_list
                .iter()
                .filter(|(slot, _)| slots.contains(&slot))
                .cloned()
                .collect();
            slot_list.retain(|(slot, _)| !slots.contains(slot));
            (reclaims, slot_list.is_empty())
        })
    }

    pub fn min_ongoing_scan_root(&self) -> Option<Slot> {
        self.ongoing_scan_roots
            .read()
            .unwrap()
            .keys()
            .next()
            .cloned()
    }

    // Given a SlotSlice `L`, a list of ancestors and a maximum slot, find the latest element
    // in `L`, where the slot `S` is an ancestor or root, and if `S` is a root, then `S <= max_root`
    fn latest_slot(
        &self,
        ancestors: Option<&Ancestors>,
        slice: SlotSlice<T>,
        max_root: Option<Slot>,
    ) -> Option<usize> {
        let mut current_max = 0;
        let mut rv = None;
        for (i, (slot, _t)) in slice.iter().rev().enumerate() {
            if *slot >= current_max && self.is_ancestor_or_root(*slot, ancestors, max_root) {
                rv = Some((slice.len() - 1) - i);
                current_max = *slot;
            }
        }

        rv
    }

    // Checks that the given slot is either:
    // 1) in the `ancestors` set
    // 2) or is a root
    fn is_ancestor_or_root(
        &self,
        slot: Slot,
        ancestors: Option<&Ancestors>,
        max_root: Option<Slot>,
    ) -> bool {
        ancestors.map_or(false, |ancestors| ancestors.contains_key(&slot)) ||
        // If the slot is a root, it must be less than the maximum root specified. This
        // allows scans on non-rooted slots to specify and read data from
        // ancestors > max_root, while not seeing rooted data update during the scan
        (max_root.map_or(true, |max_root| slot <= max_root) && (self.is_root(slot)))
    }

    /// Get an account
    /// The latest account that appears in `ancestors` or `roots` is returned.
    pub(crate) fn get(
        &self,
        pubkey: &Pubkey,
        ancestors: Option<&Ancestors>,
        max_root: Option<Slot>,
    ) -> Option<(ReadAccountMapEntry<T>, usize)> {
        self.get_account_read_entry(pubkey)
            .and_then(|locked_entry| {
                let found_index =
                    self.latest_slot(ancestors, &locked_entry.slot_list(), max_root)?;
                Some((locked_entry, found_index))
            })
    }

    // Get the maximum root <= `max_allowed_root` from the given `slice`
    fn get_max_root(
        roots: &HashSet<Slot>,
        slice: SlotSlice<T>,
        max_allowed_root: Option<Slot>,
    ) -> Slot {
        let mut max_root = 0;
        for (f, _) in slice.iter() {
            if let Some(max_allowed_root) = max_allowed_root {
                if *f > max_allowed_root {
                    continue;
                }
            }
            if *f > max_root && roots.contains(f) {
                max_root = *f;
            }
        }
        max_root
    }

    pub fn update_secondary_indexes(
        &self,
        pubkey: &Pubkey,
        account_owner: &Pubkey,
        account_data: &[u8],
    ) {
        self.program_id_index.insert(account_owner, pubkey);
        if *account_owner == inline_spl_token_v2_0::id()
            && account_data.len() == inline_spl_token_v2_0::state::Account::get_packed_len()
        {
            let mint_key = Pubkey::new(&account_data[..PUBKEY_BYTES]);
            self.mint_index.insert(&mint_key, pubkey);

            let owner_key = Pubkey::new(&account_data[PUBKEY_BYTES..PUBKEY_BYTES + PUBKEY_BYTES]);
            self.token_owner_index.insert(&owner_key, pubkey);
        }
    }

    // Updates the given pubkey at the given slot with the new account information.
    // Returns true if the pubkey was newly inserted into the index, otherwise, if the
    // pubkey updates an existing entry in the index, returns false.
    pub fn upsert(
        &self,
        slot: Slot,
        pubkey: &Pubkey,
        account_info: T,
        reclaims: &mut SlotList<T>,
    ) -> bool {
        let (mut w_account_entry, is_newly_inserted) =
            self.get_account_write_entry_else_create(pubkey);
        w_account_entry.update(slot, account_info, reclaims);
        is_newly_inserted
    }

    pub fn unref_from_storage(&self, pubkey: &Pubkey) {
        if let Some(locked_entry) = self.get_account_read_entry(pubkey) {
            locked_entry.ref_count().fetch_sub(1, Ordering::Relaxed);
        }
    }

    pub fn ref_count_from_storage(&self, pubkey: &Pubkey) -> RefCount {
        if let Some(locked_entry) = self.get_account_read_entry(pubkey) {
            locked_entry.ref_count().load(Ordering::Relaxed)
        } else {
            0
        }
    }

    fn purge_older_root_entries(
        &self,
        list: &mut SlotList<T>,
        reclaims: &mut SlotList<T>,
        max_clean_root: Option<Slot>,
    ) {
        let roots_traker = &self.roots_tracker.read().unwrap();

        let max_root = Self::get_max_root(&roots_traker.roots, &list, max_clean_root);

        reclaims.extend(
            list.iter()
                .filter(|(slot, _)| Self::can_purge(max_root, *slot))
                .cloned(),
        );
        list.retain(|(slot, _)| !Self::can_purge(max_root, *slot));
    }

    pub fn clean_rooted_entries(
        &self,
        pubkey: &Pubkey,
        reclaims: &mut SlotList<T>,
        max_clean_root: Option<Slot>,
    ) {
        if let Some(mut locked_entry) = self.get_account_write_entry(pubkey) {
            locked_entry.slot_list_mut(|slot_list| {
                self.purge_older_root_entries(slot_list, reclaims, max_clean_root);
            });
        }
    }

    pub fn clean_unrooted_entries_by_slot(
        &self,
        purge_slot: Slot,
        pubkey: &Pubkey,
        reclaims: &mut SlotList<T>,
    ) {
        if let Some(mut locked_entry) = self.get_account_write_entry(pubkey) {
            locked_entry.slot_list_mut(|slot_list| {
                slot_list.retain(|(slot, entry)| {
                    if *slot == purge_slot {
                        reclaims.push((*slot, entry.clone()));
                    }
                    *slot != purge_slot
                });
            });
        }
    }

    pub fn can_purge(max_root: Slot, slot: Slot) -> bool {
        slot < max_root
    }

    pub fn is_root(&self, slot: Slot) -> bool {
        self.roots_tracker.read().unwrap().roots.contains(&slot)
    }

    pub fn add_root(&self, slot: Slot) {
        let mut w_roots_tracker = self.roots_tracker.write().unwrap();
        w_roots_tracker.roots.insert(slot);
        w_roots_tracker.uncleaned_roots.insert(slot);
        w_roots_tracker.max_root = std::cmp::max(slot, w_roots_tracker.max_root);
    }

    fn max_root(&self) -> Slot {
        self.roots_tracker.read().unwrap().max_root
    }

    /// Remove the slot when the storage for the slot is freed
    /// Accounts no longer reference this slot.
    pub fn clean_dead_slot(&self, slot: Slot) {
        let mut w_roots_tracker = self.roots_tracker.write().unwrap();
        w_roots_tracker.roots.remove(&slot);
        w_roots_tracker.uncleaned_roots.remove(&slot);
        w_roots_tracker.previous_uncleaned_roots.remove(&slot);
    }

    pub fn reset_uncleaned_roots(&self, max_clean_root: Option<Slot>) -> HashSet<Slot> {
        let mut cleaned_roots = HashSet::new();
        let mut w_roots_tracker = self.roots_tracker.write().unwrap();
        w_roots_tracker.uncleaned_roots.retain(|root| {
            let is_cleaned = max_clean_root
                .map(|max_clean_root| *root <= max_clean_root)
                .unwrap_or(true);
            if is_cleaned {
                cleaned_roots.insert(*root);
            }
            // Only keep the slots that have yet to be cleaned
            !is_cleaned
        });
        std::mem::replace(&mut w_roots_tracker.previous_uncleaned_roots, cleaned_roots)
    }

    pub fn is_uncleaned_root(&self, slot: Slot) -> bool {
        self.roots_tracker
            .read()
            .unwrap()
            .uncleaned_roots
            .contains(&slot)
    }

    pub fn all_roots(&self) -> Vec<Slot> {
        self.roots_tracker
            .read()
            .unwrap()
            .roots
            .iter()
            .cloned()
            .collect()
    }

    #[cfg(test)]
    pub fn clear_roots(&self) {
        self.roots_tracker.write().unwrap().roots.clear()
    }

    #[cfg(test)]
    pub fn uncleaned_roots_len(&self) -> usize {
        self.roots_tracker.read().unwrap().uncleaned_roots.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use solana_sdk::signature::{Keypair, Signer};

    #[test]
    fn test_get_empty() {
        let key = Keypair::new();
        let index = AccountsIndex::<bool>::default();
        let ancestors = HashMap::new();
        assert!(index.get(&key.pubkey(), Some(&ancestors), None).is_none());
        assert!(index.get(&key.pubkey(), None, None).is_none());

        let mut num = 0;
        index.unchecked_scan_accounts(&ancestors, |_pubkey, _index| num += 1);
        assert_eq!(num, 0);
    }

    #[test]
    fn test_insert_no_ancestors() {
        let key = Keypair::new();
        let index = AccountsIndex::<bool>::default();
        let mut gc = Vec::new();
        index.upsert(0, &key.pubkey(), true, &mut gc);
        assert!(gc.is_empty());

        let ancestors = HashMap::new();
        assert!(index.get(&key.pubkey(), Some(&ancestors), None).is_none());
        assert!(index.get(&key.pubkey(), None, None).is_none());

        let mut num = 0;
        index.unchecked_scan_accounts(&ancestors, |_pubkey, _index| num += 1);
        assert_eq!(num, 0);
    }

    #[test]
    fn test_insert_wrong_ancestors() {
        let key = Keypair::new();
        let index = AccountsIndex::<bool>::default();
        let mut gc = Vec::new();
        index.upsert(0, &key.pubkey(), true, &mut gc);
        assert!(gc.is_empty());

        let ancestors = vec![(1, 1)].into_iter().collect();
        assert!(index.get(&key.pubkey(), Some(&ancestors), None).is_none());

        let mut num = 0;
        index.unchecked_scan_accounts(&ancestors, |_pubkey, _index| num += 1);
        assert_eq!(num, 0);
    }

    #[test]
    fn test_insert_with_ancestors() {
        let key = Keypair::new();
        let index = AccountsIndex::<bool>::default();
        let mut gc = Vec::new();
        index.upsert(0, &key.pubkey(), true, &mut gc);
        assert!(gc.is_empty());

        let ancestors = vec![(0, 0)].into_iter().collect();
        let (list, idx) = index.get(&key.pubkey(), Some(&ancestors), None).unwrap();
        assert_eq!(list.slot_list()[idx], (0, true));

        let mut num = 0;
        let mut found_key = false;
        index.unchecked_scan_accounts(&ancestors, |pubkey, _index| {
            if pubkey == &key.pubkey() {
                found_key = true
            };
            num += 1
        });
        assert_eq!(num, 1);
        assert!(found_key);
    }

    fn setup_accounts_index_keys(num_pubkeys: usize) -> (AccountsIndex<bool>, Vec<Pubkey>) {
        let index = AccountsIndex::<bool>::default();
        let root_slot = 0;

        let mut pubkeys: Vec<Pubkey> = std::iter::repeat_with(|| {
            let new_pubkey = solana_sdk::pubkey::new_rand();
            index.upsert(root_slot, &new_pubkey, true, &mut vec![]);
            new_pubkey
        })
        .take(num_pubkeys.saturating_sub(1))
        .collect();

        if num_pubkeys != 0 {
            pubkeys.push(Pubkey::default());
            index.upsert(root_slot, &Pubkey::default(), true, &mut vec![]);
        }

        index.add_root(root_slot);

        (index, pubkeys)
    }

    fn run_test_range(
        index: &AccountsIndex<bool>,
        pubkeys: &[Pubkey],
        start_bound: Bound<usize>,
        end_bound: Bound<usize>,
    ) {
        // Exclusive `index_start`
        let (pubkey_start, index_start) = match start_bound {
            Unbounded => (Unbounded, 0),
            Included(i) => (Included(pubkeys[i]), i),
            Excluded(i) => (Excluded(pubkeys[i]), i + 1),
        };

        // Exclusive `index_end`
        let (pubkey_end, index_end) = match end_bound {
            Unbounded => (Unbounded, pubkeys.len()),
            Included(i) => (Included(pubkeys[i]), i + 1),
            Excluded(i) => (Excluded(pubkeys[i]), i),
        };
        let pubkey_range = (pubkey_start, pubkey_end);

        let ancestors: Ancestors = HashMap::new();
        let mut scanned_keys = HashSet::new();
        index.range_scan_accounts(&ancestors, pubkey_range, |pubkey, _index| {
            scanned_keys.insert(*pubkey);
        });

        let mut expected_len = 0;
        for key in &pubkeys[index_start..index_end] {
            expected_len += 1;
            assert!(scanned_keys.contains(key));
        }

        assert_eq!(scanned_keys.len(), expected_len);
    }

    fn run_test_range_indexes(
        index: &AccountsIndex<bool>,
        pubkeys: &[Pubkey],
        start: Option<usize>,
        end: Option<usize>,
    ) {
        let start_options = start
            .map(|i| vec![Included(i), Excluded(i)])
            .unwrap_or_else(|| vec![Unbounded]);
        let end_options = end
            .map(|i| vec![Included(i), Excluded(i)])
            .unwrap_or_else(|| vec![Unbounded]);

        for start in &start_options {
            for end in &end_options {
                run_test_range(index, pubkeys, *start, *end);
            }
        }
    }

    #[test]
    fn test_range_scan_accounts() {
        let (index, mut pubkeys) = setup_accounts_index_keys(3 * ITER_BATCH_SIZE);
        pubkeys.sort();

        run_test_range_indexes(&index, &pubkeys, None, None);

        run_test_range_indexes(&index, &pubkeys, Some(ITER_BATCH_SIZE), None);

        run_test_range_indexes(&index, &pubkeys, None, Some(2 * ITER_BATCH_SIZE as usize));

        run_test_range_indexes(
            &index,
            &pubkeys,
            Some(ITER_BATCH_SIZE as usize),
            Some(2 * ITER_BATCH_SIZE as usize),
        );

        run_test_range_indexes(
            &index,
            &pubkeys,
            Some(ITER_BATCH_SIZE as usize),
            Some(2 * ITER_BATCH_SIZE as usize - 1),
        );

        run_test_range_indexes(
            &index,
            &pubkeys,
            Some(ITER_BATCH_SIZE - 1 as usize),
            Some(2 * ITER_BATCH_SIZE as usize + 1),
        );
    }

    fn run_test_scan_accounts(num_pubkeys: usize) {
        let (index, _) = setup_accounts_index_keys(num_pubkeys);
        let ancestors: Ancestors = HashMap::new();

        let mut scanned_keys = HashSet::new();
        index.unchecked_scan_accounts(&ancestors, |pubkey, _index| {
            scanned_keys.insert(*pubkey);
        });
        assert_eq!(scanned_keys.len(), num_pubkeys);
    }

    #[test]
    fn test_scan_accounts() {
        run_test_scan_accounts(0);
        run_test_scan_accounts(1);
        run_test_scan_accounts(ITER_BATCH_SIZE * 10);
        run_test_scan_accounts(ITER_BATCH_SIZE * 10 - 1);
        run_test_scan_accounts(ITER_BATCH_SIZE * 10 + 1);
    }

    #[test]
    fn test_accounts_iter_finished() {
        let (index, _) = setup_accounts_index_keys(0);
        let mut iter = index.iter(None::<Range<Pubkey>>);
        assert!(iter.next().is_none());
        let mut gc = vec![];
        index.upsert(0, &solana_sdk::pubkey::new_rand(), true, &mut gc);
        assert!(iter.next().is_none());
    }

    #[test]
    fn test_is_root() {
        let index = AccountsIndex::<bool>::default();
        assert!(!index.is_root(0));
        index.add_root(0);
        assert!(index.is_root(0));
    }

    #[test]
    fn test_insert_with_root() {
        let key = Keypair::new();
        let index = AccountsIndex::<bool>::default();
        let mut gc = Vec::new();
        index.upsert(0, &key.pubkey(), true, &mut gc);
        assert!(gc.is_empty());

        index.add_root(0);
        let (list, idx) = index.get(&key.pubkey(), None, None).unwrap();
        assert_eq!(list.slot_list()[idx], (0, true));
    }

    #[test]
    fn test_clean_first() {
        let index = AccountsIndex::<bool>::default();
        index.add_root(0);
        index.add_root(1);
        index.clean_dead_slot(0);
        assert!(index.is_root(1));
        assert!(!index.is_root(0));
    }

    #[test]
    fn test_clean_last() {
        //this behavior might be undefined, clean up should only occur on older slots
        let index = AccountsIndex::<bool>::default();
        index.add_root(0);
        index.add_root(1);
        index.clean_dead_slot(1);
        assert!(!index.is_root(1));
        assert!(index.is_root(0));
    }

    #[test]
    fn test_clean_and_unclean_slot() {
        let index = AccountsIndex::<bool>::default();
        assert_eq!(0, index.roots_tracker.read().unwrap().uncleaned_roots.len());
        index.add_root(0);
        index.add_root(1);
        assert_eq!(2, index.roots_tracker.read().unwrap().uncleaned_roots.len());

        assert_eq!(
            0,
            index
                .roots_tracker
                .read()
                .unwrap()
                .previous_uncleaned_roots
                .len()
        );
        index.reset_uncleaned_roots(None);
        assert_eq!(2, index.roots_tracker.read().unwrap().roots.len());
        assert_eq!(0, index.roots_tracker.read().unwrap().uncleaned_roots.len());
        assert_eq!(
            2,
            index
                .roots_tracker
                .read()
                .unwrap()
                .previous_uncleaned_roots
                .len()
        );

        index.add_root(2);
        index.add_root(3);
        assert_eq!(4, index.roots_tracker.read().unwrap().roots.len());
        assert_eq!(2, index.roots_tracker.read().unwrap().uncleaned_roots.len());
        assert_eq!(
            2,
            index
                .roots_tracker
                .read()
                .unwrap()
                .previous_uncleaned_roots
                .len()
        );

        index.clean_dead_slot(1);
        assert_eq!(3, index.roots_tracker.read().unwrap().roots.len());
        assert_eq!(2, index.roots_tracker.read().unwrap().uncleaned_roots.len());
        assert_eq!(
            1,
            index
                .roots_tracker
                .read()
                .unwrap()
                .previous_uncleaned_roots
                .len()
        );

        index.clean_dead_slot(2);
        assert_eq!(2, index.roots_tracker.read().unwrap().roots.len());
        assert_eq!(1, index.roots_tracker.read().unwrap().uncleaned_roots.len());
        assert_eq!(
            1,
            index
                .roots_tracker
                .read()
                .unwrap()
                .previous_uncleaned_roots
                .len()
        );
    }

    #[test]
    fn test_update_last_wins() {
        let key = Keypair::new();
        let index = AccountsIndex::<bool>::default();
        let ancestors = vec![(0, 0)].into_iter().collect();
        let mut gc = Vec::new();
        index.upsert(0, &key.pubkey(), true, &mut gc);
        assert!(gc.is_empty());
        let (list, idx) = index.get(&key.pubkey(), Some(&ancestors), None).unwrap();
        assert_eq!(list.slot_list()[idx], (0, true));
        drop(list);

        let mut gc = Vec::new();
        index.upsert(0, &key.pubkey(), false, &mut gc);
        assert_eq!(gc, vec![(0, true)]);
        let (list, idx) = index.get(&key.pubkey(), Some(&ancestors), None).unwrap();
        assert_eq!(list.slot_list()[idx], (0, false));
    }

    #[test]
    fn test_update_new_slot() {
        solana_logger::setup();
        let key = Keypair::new();
        let index = AccountsIndex::<bool>::default();
        let ancestors = vec![(0, 0)].into_iter().collect();
        let mut gc = Vec::new();
        index.upsert(0, &key.pubkey(), true, &mut gc);
        assert!(gc.is_empty());
        index.upsert(1, &key.pubkey(), false, &mut gc);
        assert!(gc.is_empty());
        let (list, idx) = index.get(&key.pubkey(), Some(&ancestors), None).unwrap();
        assert_eq!(list.slot_list()[idx], (0, true));
        let ancestors = vec![(1, 0)].into_iter().collect();
        let (list, idx) = index.get(&key.pubkey(), Some(&ancestors), None).unwrap();
        assert_eq!(list.slot_list()[idx], (1, false));
    }

    #[test]
    fn test_update_gc_purged_slot() {
        let key = Keypair::new();
        let index = AccountsIndex::<bool>::default();
        let mut gc = Vec::new();
        index.upsert(0, &key.pubkey(), true, &mut gc);
        assert!(gc.is_empty());
        index.upsert(1, &key.pubkey(), false, &mut gc);
        index.upsert(2, &key.pubkey(), true, &mut gc);
        index.upsert(3, &key.pubkey(), true, &mut gc);
        index.add_root(0);
        index.add_root(1);
        index.add_root(3);
        index.upsert(4, &key.pubkey(), true, &mut gc);

        // Updating index should not purge older roots, only purges
        // previous updates within the same slot
        assert_eq!(gc, vec![]);
        let (list, idx) = index.get(&key.pubkey(), None, None).unwrap();
        assert_eq!(list.slot_list()[idx], (3, true));

        let mut num = 0;
        let mut found_key = false;
        index.unchecked_scan_accounts(&Ancestors::new(), |pubkey, _index| {
            if pubkey == &key.pubkey() {
                found_key = true;
                assert_eq!(_index, (&true, 3));
            };
            num += 1
        });
        assert_eq!(num, 1);
        assert!(found_key);
    }

    #[test]
    fn test_purge() {
        let key = Keypair::new();
        let index = AccountsIndex::<u64>::default();
        let mut gc = Vec::new();
        assert!(index.upsert(1, &key.pubkey(), 12, &mut gc));

        assert!(!index.upsert(1, &key.pubkey(), 10, &mut gc));

        let purges = index.purge(&key.pubkey());
        assert_eq!(purges, (vec![], false));
        index.add_root(1);

        let purges = index.purge(&key.pubkey());
        assert_eq!(purges, (vec![(1, 10)], true));

        assert!(!index.upsert(1, &key.pubkey(), 9, &mut gc));
    }

    #[test]
    fn test_latest_slot() {
        let slot_slice = vec![(0, true), (5, true), (3, true), (7, true)];
        let index = AccountsIndex::<bool>::default();

        // No ancestors, no root, should return None
        assert!(index.latest_slot(None, &slot_slice, None).is_none());

        // Given a root, should return the root
        index.add_root(5);
        assert_eq!(index.latest_slot(None, &slot_slice, None).unwrap(), 1);

        // Given a max_root == root, should still return the root
        assert_eq!(index.latest_slot(None, &slot_slice, Some(5)).unwrap(), 1);

        // Given a max_root < root, should filter out the root
        assert!(index.latest_slot(None, &slot_slice, Some(4)).is_none());

        // Given a max_root, should filter out roots < max_root, but specified
        // ancestors should not be affected
        let ancestors: HashMap<Slot, usize> = vec![(3, 1), (7, 1)].into_iter().collect();
        assert_eq!(
            index
                .latest_slot(Some(&ancestors), &slot_slice, Some(4))
                .unwrap(),
            3
        );
        assert_eq!(
            index
                .latest_slot(Some(&ancestors), &slot_slice, Some(7))
                .unwrap(),
            3
        );

        // Given no max_root, should just return the greatest ancestor or root
        assert_eq!(
            index
                .latest_slot(Some(&ancestors), &slot_slice, None)
                .unwrap(),
            3
        );
    }

    #[test]
    fn test_purge_older_root_entries() {
        // No roots, should be no reclaims
        let index = AccountsIndex::<bool>::default();
        let mut slot_list = vec![(1, true), (2, true), (5, true), (9, true)];
        let mut reclaims = vec![];
        index.purge_older_root_entries(&mut slot_list, &mut reclaims, None);
        assert!(reclaims.is_empty());
        assert_eq!(slot_list, vec![(1, true), (2, true), (5, true), (9, true)]);

        // Add a later root, earlier slots should be reclaimed
        slot_list = vec![(1, true), (2, true), (5, true), (9, true)];
        index.add_root(1);
        // Note 2 is not a root
        index.add_root(5);
        reclaims = vec![];
        index.purge_older_root_entries(&mut slot_list, &mut reclaims, None);
        assert_eq!(reclaims, vec![(1, true), (2, true)]);
        assert_eq!(slot_list, vec![(5, true), (9, true)]);

        // Add a later root that is not in the list, should not affect the outcome
        slot_list = vec![(1, true), (2, true), (5, true), (9, true)];
        index.add_root(6);
        reclaims = vec![];
        index.purge_older_root_entries(&mut slot_list, &mut reclaims, None);
        assert_eq!(reclaims, vec![(1, true), (2, true)]);
        assert_eq!(slot_list, vec![(5, true), (9, true)]);

        // Pass a max root >= than any root in the slot list, should not affect
        // outcome
        slot_list = vec![(1, true), (2, true), (5, true), (9, true)];
        reclaims = vec![];
        index.purge_older_root_entries(&mut slot_list, &mut reclaims, Some(6));
        assert_eq!(reclaims, vec![(1, true), (2, true)]);
        assert_eq!(slot_list, vec![(5, true), (9, true)]);

        // Pass a max root, earlier slots should be reclaimed
        slot_list = vec![(1, true), (2, true), (5, true), (9, true)];
        reclaims = vec![];
        index.purge_older_root_entries(&mut slot_list, &mut reclaims, Some(5));
        assert_eq!(reclaims, vec![(1, true), (2, true)]);
        assert_eq!(slot_list, vec![(5, true), (9, true)]);

        // Pass a max root 2. This means the latest root < 2 is 1 because 2 is not a root
        // so nothing will be purged
        slot_list = vec![(1, true), (2, true), (5, true), (9, true)];
        reclaims = vec![];
        index.purge_older_root_entries(&mut slot_list, &mut reclaims, Some(2));
        assert!(reclaims.is_empty());
        assert_eq!(slot_list, vec![(1, true), (2, true), (5, true), (9, true)]);

        // Pass a max root 1. This means the latest root < 3 is 1 because 2 is not a root
        // so nothing will be purged
        slot_list = vec![(1, true), (2, true), (5, true), (9, true)];
        reclaims = vec![];
        index.purge_older_root_entries(&mut slot_list, &mut reclaims, Some(1));
        assert!(reclaims.is_empty());
        assert_eq!(slot_list, vec![(1, true), (2, true), (5, true), (9, true)]);

        // Pass a max root that doesn't exist in the list but is greater than
        // some of the roots in the list, shouldn't return those smaller roots
        slot_list = vec![(1, true), (2, true), (5, true), (9, true)];
        reclaims = vec![];
        index.purge_older_root_entries(&mut slot_list, &mut reclaims, Some(7));
        assert_eq!(reclaims, vec![(1, true), (2, true)]);
        assert_eq!(slot_list, vec![(5, true), (9, true)]);
    }

    #[test]
    fn test_mint_index() {
        let index = AccountsIndex::<bool>::default();
        let account_key = Pubkey::new_unique();
        let mint_key = Pubkey::new_unique();
        let mut correct_account_data =
            vec![0; inline_spl_token_v2_0::state::Account::get_packed_len()];
        correct_account_data[..PUBKEY_BYTES].clone_from_slice(&(mint_key.clone().to_bytes()));

        // Wrong program id
        index.update_secondary_indexes(&account_key, &Pubkey::default(), &correct_account_data);
        assert!(index.mint_index.index.is_empty());
        assert!(index.mint_index.reverse_index.is_empty());

        // Wrong account data size
        index.update_secondary_indexes(
            &account_key,
            &inline_spl_token_v2_0::id(),
            &correct_account_data[1..],
        );
        assert!(index.mint_index.index.is_empty());
        assert!(index.mint_index.reverse_index.is_empty());

        // Just right
        index.update_secondary_indexes(
            &account_key,
            &inline_spl_token_v2_0::id(),
            &correct_account_data,
        );
        assert!(index
            .mint_index
            .index
            .get(&mint_key)
            .unwrap()
            .value()
            .contains(&account_key));
        assert!(index
            .mint_index
            .reverse_index
            .get(&account_key)
            .unwrap()
            .value()
            .contains(&mint_key));

        // Insert into index, simulate dead key
        index.upsert(0, &account_key, true, &mut vec![]);
        index
            .get_account_write_entry(&account_key)
            .unwrap()
            .slot_list_mut(|slot_list| slot_list.clear());

        // Everything should be deleted
        index.handle_dead_keys(&[account_key]);
        assert!(index.mint_index.index.is_empty());
        assert!(index.mint_index.reverse_index.is_empty());
    }
}
