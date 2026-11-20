//! [`SimFs`]: an in-memory filesystem that can simulate crashes.
//!
//! It models the two things a real crash can lose:
//!
//! 1. **File contents** not yet synced. Each file remembers `synced_len`, how
//!    many leading bytes a `sync` has made durable.
//! 2. **Directory entries** not yet synced. Each directory has a *live* set of
//!    names (what you see now) and a *durable* set (what survives a crash),
//!    updated by `sync_dir`.
//!
//! A crash also kills the "process": file handles and locks taken before it
//! stop working (tracked with an `epoch` counter).
//!
//! Simplification: directories themselves are durable as soon as created.

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use parking_lot::Mutex;

use super::{FileLock, FileSystem, RandomAccessFile, WritableFile};
use crate::{Error, Result};

#[derive(Debug, Default)]
struct SimFile {
    data: Vec<u8>,
    synced_len: usize,
}

#[derive(Debug, Default)]
struct State {
    /// Contents of every file that may exist now or after a crash.
    files: BTreeMap<PathBuf, SimFile>,
    /// dir -> names visible now.
    live: BTreeMap<PathBuf, BTreeSet<String>>,
    /// dir -> names that survive a crash.
    durable: BTreeMap<PathBuf, BTreeSet<String>>,
    /// lock path -> id of the holder.
    locks: BTreeMap<PathBuf, u64>,
    next_lock_id: u64,
    /// Bumped by every crash; handles from an older epoch are dead.
    epoch: u64,
    fail_next_append: bool,
    fail_next_sync: bool,
    /// Mutating calls allowed before the simulated process dies.
    kill_after: Option<usize>,
    killed: bool,
}

/// Shared, cloneable in-memory filesystem. Clones see the same files.
#[derive(Debug, Default, Clone)]
pub struct SimFs {
    state: Arc<Mutex<State>>,
}

fn sim_err(msg: &str) -> Error {
    Error::Io(io::Error::other(format!("simulated: {msg}")))
}

fn not_found(path: &Path) -> Error {
    Error::Io(io::Error::new(
        io::ErrorKind::NotFound,
        format!("{} not found", path.display()),
    ))
}

fn split(path: &Path) -> (PathBuf, String) {
    let dir = path.parent().unwrap_or(Path::new("/")).to_path_buf();
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    (dir, name)
}

/// xorshift64*: tiny deterministic PRNG for reproducible crashes.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

impl State {
    /// Account for one mutating call; fails if the process is "dead".
    fn mutate(&mut self) -> Result<()> {
        if self.killed {
            return Err(sim_err("process killed"));
        }
        if let Some(n) = self.kill_after {
            if n == 0 {
                self.killed = true;
                return Err(sim_err("process killed"));
            }
            self.kill_after = Some(n - 1);
        }
        Ok(())
    }

    fn is_live(&self, path: &Path) -> bool {
        let (dir, name) = split(path);
        self.live.get(&dir).is_some_and(|s| s.contains(&name))
    }

    /// Common crash bookkeeping; `tear` decides what happens to each file's
    /// unsynced bytes.
    fn crash_with(&mut self, mut tear: impl FnMut(usize, &mut SimFile)) {
        self.epoch += 1;
        self.locks.clear();
        self.fail_next_append = false;
        self.fail_next_sync = false;
        self.kill_after = None;
        self.killed = false;
        self.live = self.durable.clone();
        let durable = &self.durable;
        self.files.retain(|path, _| {
            let (dir, name) = split(path);
            durable.get(&dir).is_some_and(|s| s.contains(&name))
        });
        for (i, file) in self.files.values_mut().enumerate() {
            tear(i, file);
            file.synced_len = file.data.len();
        }
    }
}

impl SimFs {
    pub fn new() -> Self {
        Self::default()
    }

    /// Power loss: every unsynced byte and directory change is lost.
    pub fn crash(&self) {
        self.state
            .lock()
            .crash_with(|_, f| f.data.truncate(f.synced_len));
    }

    /// Power loss with *torn writes*: for each file, some of the unsynced
    /// bytes may have reached the disk. Deterministic per `seed`. Each file
    /// gets one of:
    ///
    /// - a random prefix of its unsynced bytes kept (a short write), or
    /// - its full length kept but, from a random point in the unsynced
    ///   region on, the bytes replaced by zeros or garbage (the file size was
    ///   updated but the data never landed).
    pub fn crash_torn(&self, seed: u64) {
        self.state.lock().crash_with(|i, f| {
            let unsynced = f.data.len() - f.synced_len;
            if unsynced == 0 {
                return;
            }
            let mut rng = Rng::new(seed ^ ((i as u64 + 1) << 32));
            if rng.next().is_multiple_of(2) {
                let keep = rng.below(unsynced + 1);
                f.data.truncate(f.synced_len + keep);
            } else {
                let start = f.synced_len + rng.below(unsynced);
                let zeros = rng.next().is_multiple_of(2);
                for b in &mut f.data[start..] {
                    *b = if zeros { 0 } else { rng.next() as u8 };
                }
            }
        });
    }

    /// Flip every bit of the byte at `offset` (simulates media corruption).
    pub fn corrupt_byte(&self, path: &Path, offset: usize) {
        let mut st = self.state.lock();
        let f = st.files.get_mut(path).expect("corrupt_byte: no such file");
        f.data[offset] ^= 0xFF;
    }

    /// The next `append` fails with an I/O error and writes nothing.
    pub fn fail_next_append(&self) {
        self.state.lock().fail_next_append = true;
    }

    /// The next `sync` fails with an I/O error.
    pub fn fail_next_sync(&self) {
        self.state.lock().fail_next_sync = true;
    }

    /// After `n` more mutating calls (create, append, sync, sync_dir,
    /// remove, rename), the simulated process dies: every further mutating
    /// call fails
    /// and changes nothing, until one of the `crash*` methods is called.
    pub fn kill_after(&self, n: usize) {
        self.state.lock().kill_after = Some(n);
    }

    /// Length of a visible file, if it exists.
    pub fn file_len(&self, path: &Path) -> Option<usize> {
        let st = self.state.lock();
        if st.is_live(path) {
            st.files.get(path).map(|f| f.data.len())
        } else {
            None
        }
    }

    /// Test helper: copy a file, fully durable (contents and directory entry).
    pub fn copy_file(&self, from: &Path, to: &Path) {
        let mut st = self.state.lock();
        let data = st
            .files
            .get(from)
            .expect("copy_file: no source")
            .data
            .clone();
        let len = data.len();
        st.files.insert(
            to.to_path_buf(),
            SimFile {
                data,
                synced_len: len,
            },
        );
        let (dir, name) = split(to);
        st.live.entry(dir.clone()).or_default().insert(name.clone());
        st.durable.entry(dir).or_default().insert(name);
    }
}

struct SimHandle {
    state: Arc<Mutex<State>>,
    path: PathBuf,
    epoch: u64,
}

impl SimHandle {
    fn check(&self, st: &mut State) -> Result<()> {
        if st.epoch != self.epoch {
            return Err(sim_err("file handle from before a crash"));
        }
        st.mutate()
    }
}

impl WritableFile for SimHandle {
    fn append(&mut self, data: &[u8]) -> Result<()> {
        let mut st = self.state.lock();
        self.check(&mut st)?;
        if std::mem::take(&mut st.fail_next_append) {
            return Err(sim_err("append failed"));
        }
        st.files
            .get_mut(&self.path)
            .ok_or_else(|| not_found(&self.path))?
            .data
            .extend_from_slice(data);
        Ok(())
    }

    fn sync(&mut self) -> Result<()> {
        let mut st = self.state.lock();
        self.check(&mut st)?;
        if std::mem::take(&mut st.fail_next_sync) {
            return Err(sim_err("sync failed"));
        }
        let f = st
            .files
            .get_mut(&self.path)
            .ok_or_else(|| not_found(&self.path))?;
        f.synced_len = f.data.len();
        Ok(())
    }
}

/// Read handle. Reads the file's *live* contents (what a running process
/// would see, synced or not) and dies with the process on a crash.
struct SimReader {
    state: Arc<Mutex<State>>,
    path: PathBuf,
    epoch: u64,
}

impl SimReader {
    fn with_file<T>(&self, f: impl FnOnce(&SimFile) -> Result<T>) -> Result<T> {
        let st = self.state.lock();
        if st.epoch != self.epoch {
            return Err(sim_err("file handle from before a crash"));
        }
        f(st.files
            .get(&self.path)
            .ok_or_else(|| not_found(&self.path))?)
    }
}

impl RandomAccessFile for SimReader {
    fn read_at(&self, offset: u64, len: usize) -> Result<Vec<u8>> {
        self.with_file(|file| {
            let range = usize::try_from(offset)
                .ok()
                .and_then(|start| Some(start..start.checked_add(len)?))
                .filter(|r| r.end <= file.data.len());
            match range {
                Some(r) => Ok(file.data[r].to_vec()),
                None => Err(Error::Io(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "read past end of file",
                ))),
            }
        })
    }

    fn size(&self) -> Result<u64> {
        self.with_file(|file| Ok(file.data.len() as u64))
    }
}

struct SimLock {
    state: Arc<Mutex<State>>,
    path: PathBuf,
    id: u64,
}

impl FileLock for SimLock {}

impl Drop for SimLock {
    fn drop(&mut self) {
        let mut st = self.state.lock();
        // After a crash someone else may hold this path; only release our own.
        if st.locks.get(&self.path) == Some(&self.id) {
            st.locks.remove(&self.path);
        }
    }
}

impl FileSystem for SimFs {
    fn create_dir_all(&self, dir: &Path) -> Result<()> {
        let mut st = self.state.lock();
        st.live.entry(dir.to_path_buf()).or_default();
        st.durable.entry(dir.to_path_buf()).or_default();
        Ok(())
    }

    fn create(&self, path: &Path) -> Result<Box<dyn WritableFile>> {
        let mut st = self.state.lock();
        st.mutate()?;
        let (dir, name) = split(path);
        st.live
            .get_mut(&dir)
            .ok_or_else(|| not_found(&dir))?
            .insert(name);
        st.files.insert(path.to_path_buf(), SimFile::default());
        Ok(Box::new(SimHandle {
            state: self.state.clone(),
            path: path.to_path_buf(),
            epoch: st.epoch,
        }))
    }

    fn read(&self, path: &Path) -> Result<Vec<u8>> {
        let st = self.state.lock();
        if !st.is_live(path) {
            return Err(not_found(path));
        }
        Ok(st
            .files
            .get(path)
            .map(|f| f.data.clone())
            .unwrap_or_default())
    }

    fn open_random(&self, path: &Path) -> Result<Box<dyn RandomAccessFile>> {
        let st = self.state.lock();
        if !st.is_live(path) {
            return Err(not_found(path));
        }
        Ok(Box::new(SimReader {
            state: self.state.clone(),
            path: path.to_path_buf(),
            epoch: st.epoch,
        }))
    }

    fn list(&self, dir: &Path) -> Result<Vec<String>> {
        let st = self.state.lock();
        let names = st.live.get(dir).ok_or_else(|| not_found(dir))?;
        Ok(names.iter().cloned().collect()) // BTreeSet: already sorted
    }

    fn remove(&self, path: &Path) -> Result<()> {
        let mut st = self.state.lock();
        st.mutate()?;
        let (dir, name) = split(path);
        // Contents stay in `files` until a crash or sync_dir proves the
        // removal durable, so an unsynced removal can be undone by a crash.
        if !st.live.get_mut(&dir).is_some_and(|s| s.remove(&name)) {
            return Err(not_found(path));
        }
        Ok(())
    }

    /// Two simplifications, neither of which strata relies on: renaming a
    /// file that still has an open writer isn't modelled (the old handle
    /// keeps writing to the old name), and an unsynced rename onto an
    /// existing file is undone by a crash as far as names go, but the target
    /// keeps the new contents. strata only renames finished tables onto fresh
    /// names.
    fn rename(&self, from: &Path, to: &Path) -> Result<()> {
        let mut st = self.state.lock();
        st.mutate()?;
        if !st.is_live(from) {
            return Err(not_found(from));
        }
        let (to_dir, to_name) = split(to);
        if !st.live.contains_key(&to_dir) {
            return Err(not_found(&to_dir));
        }
        let src = &st.files[from];
        let copy = SimFile {
            data: src.data.clone(),
            synced_len: src.synced_len,
        };
        st.files.insert(to.to_path_buf(), copy);
        // As with `remove`, `from`'s contents stay in `files`: if a crash
        // restores the durable names, the old name comes back with them.
        let (from_dir, from_name) = split(from);
        st.live.get_mut(&from_dir).map(|s| s.remove(&from_name));
        st.live.get_mut(&to_dir).map(|s| s.insert(to_name));
        Ok(())
    }

    fn sync_dir(&self, dir: &Path) -> Result<()> {
        let mut st = self.state.lock();
        st.mutate()?;
        let names = st.live.get(dir).cloned().ok_or_else(|| not_found(dir))?;
        st.durable.insert(dir.to_path_buf(), names);
        Ok(())
    }

    fn lock(&self, path: &Path) -> Result<Box<dyn FileLock>> {
        let mut st = self.state.lock();
        if st.locks.contains_key(path) {
            return Err(Error::AlreadyOpen(path.display().to_string()));
        }
        st.next_lock_id += 1;
        let id = st.next_lock_id;
        st.locks.insert(path.to_path_buf(), id);
        Ok(Box::new(SimLock {
            state: self.state.clone(),
            path: path.to_path_buf(),
            id,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Error;
    use crate::fs::FileSystem;
    use std::path::Path;

    fn dir() -> &'static Path {
        Path::new("/db")
    }

    fn setup() -> (SimFs, std::path::PathBuf) {
        let fs = SimFs::default();
        fs.create_dir_all(dir()).unwrap();
        (fs, dir().join("a.log"))
    }

    #[test]
    fn sim_read_at() {
        let (fs, p) = setup();
        let mut f = fs.create(&p).unwrap();
        f.append(b"hello world").unwrap();
        let r = fs.open_random(&p).unwrap();
        assert_eq!(r.read_at(6, 5).unwrap(), b"world");
        assert_eq!(r.read_at(0, 0).unwrap(), b"");
        assert_eq!(r.size().unwrap(), 11);
        assert!(r.read_at(8, 5).is_err(), "past the end");
        assert!(r.read_at(u64::MAX, 1).is_err(), "offset overflow");
        assert!(fs.open_random(&dir().join("missing")).is_err());
    }

    #[test]
    fn sim_read_at_dies_after_crash() {
        let (fs, p) = setup();
        let mut f = fs.create(&p).unwrap();
        fs.sync_dir(dir()).unwrap();
        f.append(b"abc").unwrap();
        f.sync().unwrap();
        let r = fs.open_random(&p).unwrap();
        fs.crash();
        assert!(r.read_at(0, 1).is_err());
        assert!(r.size().is_err());
    }

    #[test]
    fn unsynced_data_lost_on_crash() {
        let (fs, p) = setup();
        let mut f = fs.create(&p).unwrap();
        fs.sync_dir(dir()).unwrap();
        f.append(b"abc").unwrap();
        f.sync().unwrap();
        f.append(b"def").unwrap();
        assert_eq!(
            fs.read(&p).unwrap(),
            b"abcdef",
            "before the crash we see everything"
        );
        fs.crash();
        assert_eq!(fs.read(&p).unwrap(), b"abc");
    }

    #[test]
    fn unsynced_create_lost_on_crash() {
        let (fs, p) = setup();
        let mut f = fs.create(&p).unwrap();
        f.append(b"x").unwrap();
        f.sync().unwrap(); // file contents synced, but the directory entry is not
        fs.crash();
        assert!(fs.list(dir()).unwrap().is_empty());
    }

    #[test]
    fn unsynced_remove_undone_on_crash() {
        let (fs, p) = setup();
        let mut f = fs.create(&p).unwrap();
        f.append(b"keep").unwrap();
        f.sync().unwrap();
        fs.sync_dir(dir()).unwrap();
        fs.remove(&p).unwrap();
        assert!(fs.list(dir()).unwrap().is_empty());
        fs.crash();
        assert_eq!(fs.read(&p).unwrap(), b"keep");
        // ...but a synced removal sticks.
        fs.remove(&p).unwrap();
        fs.sync_dir(dir()).unwrap();
        fs.crash();
        assert!(fs.list(dir()).unwrap().is_empty());
    }

    fn torn_after_crash(seed: u64) -> Vec<u8> {
        let (fs, p) = setup();
        let mut f = fs.create(&p).unwrap();
        fs.sync_dir(dir()).unwrap();
        f.append(b"synced-part|").unwrap();
        f.sync().unwrap();
        f.append(&[0xAB; 200]).unwrap();
        fs.crash_torn(seed);
        fs.read(&p).unwrap()
    }

    #[test]
    fn torn_crash_is_deterministic() {
        for seed in 0..20 {
            assert_eq!(torn_after_crash(seed), torn_after_crash(seed));
        }
    }

    #[test]
    fn torn_crash_keeps_synced_prefix() {
        let mut saw_short = false;
        let mut saw_full_length_garbage = false;
        for seed in 0..100 {
            let data = torn_after_crash(seed);
            assert!(data.starts_with(b"synced-part|"), "seed {seed}");
            assert!(data.len() <= 12 + 200);
            if data.len() < 12 + 200 {
                saw_short = true;
            } else if data[12..] != [0xAB; 200] {
                saw_full_length_garbage = true;
            }
        }
        assert!(
            saw_short && saw_full_length_garbage,
            "both torn modes occur"
        );
    }

    #[test]
    fn lock_is_exclusive_until_dropped() {
        let (fs, _) = setup();
        let l = dir().join("LOCK");
        let held = fs.lock(&l).unwrap();
        assert!(matches!(fs.lock(&l), Err(Error::AlreadyOpen(_))));
        drop(held);
        fs.lock(&l).unwrap();
    }

    #[test]
    fn crash_releases_locks() {
        let (fs, _) = setup();
        let l = dir().join("LOCK");
        let held = fs.lock(&l).unwrap();
        fs.crash();
        let _again = fs.lock(&l).unwrap();
        drop(held); // the dead process's handle going away must not unlock the new holder
        assert!(matches!(fs.lock(&l), Err(Error::AlreadyOpen(_))));
    }

    #[test]
    fn fail_next_append_writes_nothing() {
        let (fs, p) = setup();
        let mut f = fs.create(&p).unwrap();
        f.append(b"ok").unwrap();
        fs.fail_next_append();
        assert!(matches!(f.append(b"lost"), Err(Error::Io(_))));
        f.append(b"!").unwrap();
        assert_eq!(fs.read(&p).unwrap(), b"ok!");
    }

    #[test]
    fn fail_next_sync_fails_once() {
        let (fs, p) = setup();
        let mut f = fs.create(&p).unwrap();
        f.append(b"x").unwrap();
        fs.fail_next_sync();
        assert!(matches!(f.sync(), Err(Error::Io(_))));
        f.sync().unwrap();
    }

    #[test]
    fn kill_after_stops_all_mutation() {
        let (fs, p) = setup();
        let mut f = fs.create(&p).unwrap();
        fs.kill_after(1);
        f.append(b"a").unwrap();
        assert!(matches!(f.append(b"b"), Err(Error::Io(_))));
        assert!(fs.create(&dir().join("other")).is_err());
        fs.crash();
        let mut g = fs.create(&p).unwrap();
        g.append(b"alive").unwrap();
    }

    fn synced_file(fs: &SimFs, p: &Path, data: &[u8]) {
        let mut f = fs.create(p).unwrap();
        f.append(data).unwrap();
        f.sync().unwrap();
        fs.sync_dir(dir()).unwrap();
    }

    #[test]
    fn rename_moves_contents() {
        let (fs, p) = setup();
        let q = dir().join("b.sst");
        synced_file(&fs, &p, b"abc");
        fs.rename(&p, &q).unwrap();
        assert_eq!(fs.read(&q).unwrap(), b"abc");
        assert!(fs.read(&p).is_err());
        assert_eq!(fs.list(dir()).unwrap(), vec!["b.sst".to_string()]);
    }

    #[test]
    fn unsynced_rename_undone_by_crash() {
        let (fs, p) = setup();
        let q = dir().join("b.sst");
        synced_file(&fs, &p, b"abc");
        fs.rename(&p, &q).unwrap();
        fs.crash();
        assert_eq!(fs.read(&p).unwrap(), b"abc");
        assert!(fs.read(&q).is_err());
        assert_eq!(fs.list(dir()).unwrap(), vec!["a.log".to_string()]);
    }

    #[test]
    fn synced_rename_survives_crash() {
        let (fs, p) = setup();
        let q = dir().join("b.sst");
        synced_file(&fs, &p, b"abc");
        fs.rename(&p, &q).unwrap();
        fs.sync_dir(dir()).unwrap();
        fs.crash();
        assert_eq!(fs.read(&q).unwrap(), b"abc");
        assert_eq!(fs.list(dir()).unwrap(), vec!["b.sst".to_string()]);
    }

    #[test]
    fn rename_replaces_target() {
        let (fs, p) = setup();
        let q = dir().join("b.sst");
        synced_file(&fs, &p, b"new");
        synced_file(&fs, &q, b"old");
        fs.rename(&p, &q).unwrap();
        assert_eq!(fs.read(&q).unwrap(), b"new");
        assert_eq!(fs.list(dir()).unwrap(), vec!["b.sst".to_string()]);
    }

    #[test]
    fn rename_counts_for_kill_after() {
        let (fs, p) = setup();
        synced_file(&fs, &p, b"abc");
        fs.kill_after(0);
        assert!(fs.rename(&p, &dir().join("b.sst")).is_err());
        fs.crash();
        assert_eq!(fs.list(dir()).unwrap(), vec!["a.log".to_string()]);
    }

    #[test]
    fn rename_missing_is_err() {
        let (fs, p) = setup();
        assert!(fs.rename(&p, &dir().join("b.sst")).is_err());
    }

    #[test]
    fn copy_file_is_durable() {
        let (fs, p) = setup();
        let mut f = fs.create(&p).unwrap();
        f.append(b"data").unwrap();
        let q = dir().join("b.log");
        fs.copy_file(&p, &q);
        fs.crash();
        assert_eq!(fs.read(&q).unwrap(), b"data");
        assert_eq!(fs.file_len(&q), Some(4));
    }
}
