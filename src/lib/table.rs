use typenum;

use parking_lot::Mutex;
use serde::Serialize;
use std::cell::UnsafeCell;
use std::marker::PhantomData;
use std::mem::MaybeUninit;
use std::sync::atomic::{fence, AtomicU32, AtomicUsize, Ordering};
use std::{fs, io, mem, ptr, slice, thread};

#[derive(Default, Clone, Debug, Serialize)]
pub struct Stats {
    pub lookups: usize,
    pub hits: usize,
    pub stores: usize,
}

impl Stats {
    pub fn merge(&self, other: &Stats) -> Stats {
        Stats {
            lookups: self.lookups + other.lookups,
            hits: self.hits + other.hits,
            stores: self.stores + other.stores,
        }
    }
}

pub struct TranspositionTable<E, N>
where
    E: Entry + Default + Clone,
    N: typenum::Unsigned,
{
    index: Box<[u8]>,
    entries: Box<[E]>,
    stats: Stats,
    n: PhantomData<N>,
}

pub trait Entry {
    fn hash(&self) -> u64;
    fn valid(&self) -> bool;
    fn better_than(&self, rhs: &Self) -> bool;
}

pub const DEFAULT_TABLE_SIZE: usize = 1 << 30;

fn new_default_slice<T>(len: usize) -> Box<[T]>
where
    T: Default,
{
    let mut slice = Box::new_uninit_slice(len);
    for e in slice.iter_mut() {
        *e = MaybeUninit::new(Default::default());
    }
    unsafe { slice.assume_init() }
}

unsafe fn as_bytes<'a, T>(v: &T) -> &'a [u8] {
    slice::from_raw_parts(
        mem::transmute::<*const T, *const u8>(v as *const T),
        mem::size_of::<T>(),
    )
}

unsafe fn slice_as_bytes<'a, T>(slice: &'a [T]) -> &'a [u8] {
    slice::from_raw_parts(
        mem::transmute::<*const T, *const u8>(slice.as_ptr()),
        slice.len() * mem::size_of::<T>(),
    )
}

#[repr(C)]
#[derive(Clone, Copy)]
struct Header {
    version: u64,
    entries: u64,
}

const DUMPFILE_VERSION: u64 = 1;

impl<E, N> TranspositionTable<E, N>
where
    E: Entry + Default + Clone,
    N: typenum::Unsigned,
{
    pub fn new() -> Self {
        Self::with_memory(DEFAULT_TABLE_SIZE)
    }

    pub fn with_memory(bytes: usize) -> Self {
        let len = bytes / (1 + mem::size_of::<E>());
        Self::with_entries(len)
    }

    pub fn with_entries(len: usize) -> Self {
        TranspositionTable::<E, N> {
            index: new_default_slice(len),
            entries: new_default_slice(len),
            stats: Default::default(),
            n: PhantomData,
        }
    }

    pub fn lookup(&mut self, h: u64) -> Option<E> {
        self.stats.lookups += 1;
        let base = h as usize;
        for j in 0..N::to_usize() {
            let i = (base + j) % self.entries.len();
            if self.index[i] != (h & 0xff) as u8 {
                continue;
            }
            if self.entries[i].valid() && self.entries[i].hash() == h {
                self.stats.hits += 1;
                return Some(self.entries[i].clone());
            }
        }
        None
    }

    pub fn store(&mut self, ent: &E) -> bool {
        debug_assert!(ent.valid());
        let mut worst: Option<usize> = None;
        let base = ent.hash() as usize;
        for j in 0..N::to_usize() {
            let i = (base + j) % self.entries.len();
            if !self.entries[i].valid() || self.entries[i].hash() == ent.hash() {
                worst = Some(i);
                break;
            } else if let Some(w) = worst {
                if self.entries[w].better_than(&self.entries[i]) {
                    worst = Some(i);
                }
            } else {
                worst = Some(i);
            }
        }
        let idx = worst.unwrap();
        if !self.entries[idx].valid() || ent.better_than(&self.entries[idx]) {
            self.index[idx] = (ent.hash() & 0xff) as u8;
            self.entries[idx] = ent.clone();
            self.stats.stores += 1;
            true
        } else {
            false
        }
    }

    pub fn stats(&self) -> Stats {
        self.stats.clone()
    }

    pub fn dump(&self, w: &mut dyn io::Write) -> io::Result<()> {
        let header = Header {
            version: DUMPFILE_VERSION,
            entries: self.entries.len() as u64,
        };
        w.write(unsafe { as_bytes(&header) })?;
        w.write(unsafe { slice_as_bytes(&*self.index) })?;
        w.write(unsafe { slice_as_bytes(&*self.entries) })?;
        Ok(())
    }

    pub fn from_reader(r: &mut dyn io::Read) -> io::Result<Self> {
        let header = unsafe {
            let mut buf: MaybeUninit<Header> = MaybeUninit::uninit();
            r.read_exact(slice::from_raw_parts_mut(
                mem::transmute::<_, *mut u8>(buf.as_mut_ptr()),
                mem::size_of::<Header>(),
            ))?;
            buf.assume_init()
        };
        if header.version != DUMPFILE_VERSION {
            return Err(io::Error::new(
                io::ErrorKind::Other,
                format!(
                    "dumpfile version mismatch: expected {} got {}",
                    DUMPFILE_VERSION, header.version,
                ),
            ));
        }
        let mut table = Self::with_entries(header.entries as usize);
        r.read_exact(&mut table.index)?;
        r.read_exact(unsafe {
            slice::from_raw_parts_mut(
                mem::transmute::<_, *mut u8>(table.entries.as_mut_ptr()),
                table.entries.len() * mem::size_of_val(&table.entries[0]),
            )
        })?;
        Ok(table)
    }

    pub fn from_file(path: &str) -> io::Result<Self> {
        let mut f = fs::File::open(path)?;
        Self::from_reader(&mut f)
    }
}

const ENTRIES_PER_LOCK: usize = 16;

pub struct ConcurrentTranspositionTable<E, N>
where
    E: Entry + Default + Clone,
    N: typenum::Unsigned,
{
    index: Box<[UnsafeCell<u8>]>,
    entries: Box<[UnsafeCell<E>]>,
    counters: Box<[AtomicU32]>,

    len: usize,
    write: Mutex<()>,

    handles: AtomicUsize,
    stats: Mutex<Stats>,

    n: PhantomData<N>,
}

unsafe impl<E, N> Sync for ConcurrentTranspositionTable<E, N>
where
    E: Entry + Default + Clone,
    N: typenum::Unsigned,
{
}

unsafe impl<E, N> Send for ConcurrentTranspositionTable<E, N>
where
    E: Entry + Default + Clone,
    N: typenum::Unsigned,
{
}

impl<E, N> ConcurrentTranspositionTable<E, N>
where
    E: Entry + Default + Clone,
    N: typenum::Unsigned,
{
    pub fn new() -> Self {
        Self::with_memory(DEFAULT_TABLE_SIZE)
    }

    pub fn with_memory(bytes: usize) -> Self {
        Self::with_entries(bytes / (mem::size_of::<E>() + 1))
    }

    pub fn with_entries(len: usize) -> Self {
        Self {
            index: new_default_slice(len),
            entries: new_default_slice(len),
            counters: new_default_slice(len / ENTRIES_PER_LOCK),
            len: len,
            write: Mutex::new(()),
            n: PhantomData,
            handles: AtomicUsize::new(0),
            stats: Default::default(),
        }
    }

    pub fn from_reader(r: &mut dyn io::Read) -> io::Result<Self> {
        TranspositionTable::from_reader(r).map(|t| Self::from_table(t))
    }

    pub fn from_file(path: &str) -> io::Result<Self> {
        TranspositionTable::from_file(path).map(|t| Self::from_table(t))
    }

    fn from_table(t: TranspositionTable<E, N>) -> Self {
        let entries = t.entries.len();
        Self {
            index: unsafe { mem::transmute(t.index) },
            entries: unsafe { mem::transmute(t.entries) },
            len: entries,
            counters: new_default_slice(entries / ENTRIES_PER_LOCK),
            write: Mutex::new(()),
            n: PhantomData,
            handles: AtomicUsize::new(0),
            stats: Default::default(),
        }
    }

    pub fn lookup(&self, stats: &mut Stats, h: u64) -> Option<E> {
        stats.lookups += 1;
        let base = h as usize;
        for j in 0..N::to_usize() {
            let i = (base + j) % self.len;
            if unsafe { ptr::read(self.index[i].get()) } != (h & 0xff) as u8 {
                continue;
            }
            let entry = self.entry(i);
            if entry.valid() && entry.hash() == h {
                stats.hits += 1;
                return Some(entry);
            }
        }
        None
    }

    fn entry(&self, i: usize) -> E {
        let counter = &self.counters[i % self.counters.len()];
        loop {
            let seq1 = counter.load(Ordering::Acquire);
            if seq1 & 1 == 1 {
                // Currently writing, bail
                thread::yield_now();
                continue;
            }

            let e = unsafe { ptr::read_volatile(self.entries[i].get()) };

            fence(Ordering::Acquire);
            let seq2 = counter.load(Ordering::Relaxed);
            if seq1 != seq2 {
                continue;
            }
            return e;
        }
    }

    pub fn store(&self, stats: &mut Stats, ent: &E) -> bool {
        let _lk = self.write.lock();
        debug_assert!(ent.valid());
        let mut worst: Option<usize> = None;
        let base = ent.hash() as usize;
        for j in 0..N::to_usize() {
            let i = (base + j) % self.entries.len();
            let ei = unsafe { self.entries[i].get().as_ref().unwrap() };
            if !ei.valid() || ei.hash() == ent.hash() {
                worst = Some(i);
                break;
            } else if let Some(w) = worst {
                if (unsafe { self.entries[w].get().as_ref().unwrap() }).better_than(&ei) {
                    worst = Some(i);
                }
            } else {
                worst = Some(i);
            }
        }
        let idx = worst.unwrap();
        let dst = unsafe { self.entries[idx].get().as_mut().unwrap() };

        if !dst.valid() || ent.better_than(&dst) {
            let seq = &self.counters[idx % self.counters.len()];
            seq.fetch_add(1, Ordering::Relaxed);
            fence(Ordering::Release);

            unsafe {
                ptr::write(self.index[idx].get(), (ent.hash() & 0xff) as u8);
                ptr::write(self.entries[idx].get(), ent.clone());
            }

            seq.fetch_add(1, Ordering::Release);

            stats.stores += 1;
            true
        } else {
            false
        }
    }

    pub fn dump(&self, w: &mut dyn io::Write) -> io::Result<()> {
        let header = Header {
            version: DUMPFILE_VERSION,
            entries: self.entries.len() as u64,
        };
        w.write(unsafe { as_bytes(&header) })?;
        w.write(unsafe { slice_as_bytes(&*self.index) })?;
        w.write(unsafe { slice_as_bytes(&*self.entries) })?;
        Ok(())
    }

    pub fn stats(&self) -> Stats {
        self.stats.lock().clone()
    }

    pub fn handle<'a>(&'a self) -> ConcurrentTranspositionTableHandle<'a, E, N> {
        self.handles.fetch_add(1, Ordering::SeqCst);
        ConcurrentTranspositionTableHandle {
            table: self,
            stats: Default::default(),
        }
    }
}

pub struct ConcurrentTranspositionTableHandle<'a, E, N>
where
    E: Entry + Default + Clone,
    N: typenum::Unsigned,
{
    table: &'a ConcurrentTranspositionTable<E, N>,
    stats: Stats,
}

impl<'a, E, N> ConcurrentTranspositionTableHandle<'a, E, N>
where
    E: Entry + Default + Clone,
    N: typenum::Unsigned,
{
    pub fn lookup(&mut self, h: u64) -> Option<E> {
        self.table.lookup(&mut self.stats, h)
    }

    pub fn store(&mut self, e: &E) -> bool {
        self.table.store(&mut self.stats, e)
    }

    pub fn dump(&self, w: &mut dyn io::Write) -> io::Result<()> {
        self.table.dump(w)
    }
}

impl<'a, E, N> Drop for ConcurrentTranspositionTableHandle<'a, E, N>
where
    E: Entry + Default + Clone,
    N: typenum::Unsigned,
{
    fn drop(&mut self) {
        let mut lk = self.table.stats.lock();
        *lk = lk.merge(&self.stats);
        self.table.handles.fetch_sub(1, Ordering::SeqCst);
    }
}

impl<'a, E, N> Clone for ConcurrentTranspositionTableHandle<'a, E, N>
where
    E: Entry + Default + Clone,
    N: typenum::Unsigned,
{
    fn clone(&self) -> Self {
        self.table.handle()
    }
}
