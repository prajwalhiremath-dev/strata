//! [`OsFs`]: the real filesystem.

use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::Write;
use std::path::Path;

use super::{FileLock, FileSystem, RandomAccessFile, WritableFile};
use crate::{Error, Result};

/// The operating system's filesystem.
#[derive(Debug, Default, Clone, Copy)]
pub struct OsFs;

struct OsFile(File);

impl WritableFile for OsFile {
    fn append(&mut self, data: &[u8]) -> Result<()> {
        self.0.write_all(data)?;
        Ok(())
    }

    fn sync(&mut self) -> Result<()> {
        // On macOS this is fcntl(F_FULLFSYNC): a real flush of the drive's
        // cache. Plain fsync() there only hands data to the drive.
        self.0.sync_all()?;
        Ok(())
    }
}

/// Read-only handle. On unix `read_exact_at` is `pread`: it takes the offset
/// as an argument, so concurrent readers don't fight over a file cursor.
#[cfg(unix)]
struct OsRandomFile(File);

#[cfg(unix)]
impl RandomAccessFile for OsRandomFile {
    fn read_at(&self, offset: u64, len: usize) -> Result<Vec<u8>> {
        use std::os::unix::fs::FileExt;
        let mut buf = vec![0; len];
        self.0.read_exact_at(&mut buf, offset)?;
        Ok(buf)
    }

    fn size(&self) -> Result<u64> {
        Ok(self.0.metadata()?.len())
    }
}

/// Portable fallback: one cursor behind a mutex (seek, then read).
#[cfg(not(unix))]
struct OsRandomFile(parking_lot::Mutex<File>);

#[cfg(not(unix))]
impl RandomAccessFile for OsRandomFile {
    fn read_at(&self, offset: u64, len: usize) -> Result<Vec<u8>> {
        use std::io::{Read, Seek, SeekFrom};
        let mut f = self.0.lock();
        let mut buf = vec![0; len];
        f.seek(SeekFrom::Start(offset))?;
        f.read_exact(&mut buf)?;
        Ok(buf)
    }

    fn size(&self) -> Result<u64> {
        Ok(self.0.lock().metadata()?.len())
    }
}

/// Holding the open `File` holds the lock; dropping it releases it.
struct OsLock(#[allow(dead_code)] File);

impl FileLock for OsLock {}

impl FileSystem for OsFs {
    fn create_dir_all(&self, dir: &Path) -> Result<()> {
        fs::create_dir_all(dir)?;
        Ok(())
    }

    fn create(&self, path: &Path) -> Result<Box<dyn WritableFile>> {
        let f = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)?;
        Ok(Box::new(OsFile(f)))
    }

    fn read(&self, path: &Path) -> Result<Vec<u8>> {
        Ok(fs::read(path)?)
    }

    fn open_random(&self, path: &Path) -> Result<Box<dyn RandomAccessFile>> {
        let f = File::open(path)?;
        #[cfg(unix)]
        return Ok(Box::new(OsRandomFile(f)));
        #[cfg(not(unix))]
        return Ok(Box::new(OsRandomFile(parking_lot::Mutex::new(f))));
    }

    fn list(&self, dir: &Path) -> Result<Vec<String>> {
        let mut names = Vec::new();
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            if entry.file_type()?.is_file() {
                names.push(entry.file_name().to_string_lossy().into_owned());
            }
        }
        names.sort();
        Ok(names)
    }

    fn remove(&self, path: &Path) -> Result<()> {
        fs::remove_file(path)?;
        Ok(())
    }

    fn rename(&self, from: &Path, to: &Path) -> Result<()> {
        // rename(2) is atomic: anyone looking sees the old name or the new
        // one, never neither, and never a half-written target.
        fs::rename(from, to)?;
        Ok(())
    }

    fn sync_dir(&self, dir: &Path) -> Result<()> {
        // A directory is a file listing names -> inodes. Syncing it makes
        // creations/removals/renames in it durable.
        File::open(dir)?.sync_all()?;
        Ok(())
    }

    fn lock(&self, path: &Path) -> Result<Box<dyn FileLock>> {
        let f = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)?;
        match f.try_lock() {
            Ok(()) => Ok(Box::new(OsLock(f))),
            Err(TryLockError::WouldBlock) => Err(Error::AlreadyOpen(path.display().to_string())),
            Err(TryLockError::Error(e)) => Err(Error::Io(e)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Error;
    use crate::fs::FileSystem;

    #[test]
    fn os_roundtrip_and_list() {
        let dir = tempfile::tempdir().unwrap();
        let fs = OsFs;
        let p = dir.path().join("000001.log");
        let mut f = fs.create(&p).unwrap();
        f.append(b"hello ").unwrap();
        f.append(b"world").unwrap();
        f.sync().unwrap();
        fs.sync_dir(dir.path()).unwrap();
        assert_eq!(fs.read(&p).unwrap(), b"hello world");
        assert_eq!(fs.list(dir.path()).unwrap(), vec!["000001.log".to_string()]);
        fs.remove(&p).unwrap();
        assert!(fs.list(dir.path()).unwrap().is_empty());
    }

    #[test]
    fn os_read_at() {
        let dir = tempfile::tempdir().unwrap();
        let fs = OsFs;
        let p = dir.path().join("t.sst");
        let mut f = fs.create(&p).unwrap();
        f.append(b"hello world").unwrap();
        f.sync().unwrap();
        let r = fs.open_random(&p).unwrap();
        assert_eq!(r.read_at(6, 5).unwrap(), b"world");
        assert_eq!(r.size().unwrap(), 11);
        assert!(r.read_at(8, 5).is_err(), "past the end");
        assert!(fs.open_random(&dir.path().join("missing")).is_err());
    }

    #[test]
    fn os_rename() {
        let dir = tempfile::tempdir().unwrap();
        let fs = OsFs;
        let (a, b) = (dir.path().join("1.sst.tmp"), dir.path().join("1.sst"));
        fs.create(&a).unwrap().append(b"table").unwrap();
        fs.rename(&a, &b).unwrap();
        fs.sync_dir(dir.path()).unwrap();
        assert_eq!(fs.read(&b).unwrap(), b"table");
        assert!(fs.read(&a).is_err());
        assert_eq!(fs.list(dir.path()).unwrap(), vec!["1.sst".to_string()]);
        assert!(fs.rename(&a, &b).is_err(), "source is gone");
    }

    #[test]
    fn os_lock_is_exclusive() {
        let dir = tempfile::tempdir().unwrap();
        let fs = OsFs;
        let p = dir.path().join("LOCK");
        let held = fs.lock(&p).unwrap();
        assert!(matches!(fs.lock(&p), Err(Error::AlreadyOpen(_))));
        drop(held);
        fs.lock(&p).unwrap();
    }
}
