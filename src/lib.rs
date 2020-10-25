//! A simple write-ahead-logging crate.
//!
//! Features
//!  - Optimized for sequential reads & writes
//!  - Easy atomic log compaction
//!  - Advisory locking
//!  - CRC32 checksums
//!  - Range scans
//!  - Persistent log entry index
//!
//! The entire log is scanned through on startup in order to detect & clean interrupted
//! writes and determine the length of the log. It's recommended to compact the log when
//! old entries are no longer likely to be used.
//! 
//! ## Usage:
//! 
//! ```
//! use simple_wal::LogFile;
//!
//! let path = std::path::Path::new("./wal-log");
//! 
//! {
//!     let mut log = LogFile::open(path).unwrap();
//! 
//!     // write to log
//!     log.write(&mut b"log entry".to_vec()).unwrap();
//!     log.write(&mut b"foobar".to_vec()).unwrap();
//!     log.write(&mut b"123".to_vec()).unwrap();
//!    
//!     // flush to disk
//!     log.flush().unwrap();
//! }
//!
//! {
//!     let mut log = LogFile::open(path).unwrap();
//! 
//!     // Iterate through the log
//!     let mut iter = log.iter(..).unwrap();
//!     assert_eq!(iter.next().unwrap().unwrap(), b"log entry".to_vec());
//!     assert_eq!(iter.next().unwrap().unwrap(), b"foobar".to_vec());
//!     assert_eq!(iter.next().unwrap().unwrap(), b"123".to_vec());
//!     assert!(iter.next().is_none());
//! }
//!
//! {
//!     let mut log = LogFile::open(path).unwrap();
//!
//!     // Compact the log
//!     log.compact(1);
//!
//!     // Iterate through the log
//!     let mut iter = log.iter(..).unwrap();
//!     assert_eq!(iter.next().unwrap().unwrap(), b"foobar".to_vec());
//!     assert_eq!(iter.next().unwrap().unwrap(), b"123".to_vec());
//!     assert!(iter.next().is_none());
//! }
//!
//! # let _ = std::fs::remove_file(path);
//! ```
//! 
//! 
//! ## Log Format:
//! 
//! ```txt
//! 00 01 02 03 04 05 06 07|08 09 10 11 12 13 14 15|.......|-4 -3 -2 -1|
//! -----------------------|-----------------------|-------|-----------|
//! starting index         |entry length           | entry | crc32     |
//! unsigned 64 bit int le |unsigned 64 bit int le | data  | 32bit, le |
//! ```
//!
//! Numbers are stored in little-endian format.
//!
//! The first 8 bytes in the WAL is the starting index.
//! 
//! Each entry follows the following format:
//! 1. A 64 bit unsigned int for the entry size.
//! 2. The entry data
//! 3. A 32 bit crc32 checksum.

use advisory_lock::AdvisoryFileLock;
use std::io::{self, Read, Write, Seek, SeekFrom};
use std::path::PathBuf;
use std::ops::{RangeBounds, Bound};
use crc32fast;
use std::convert::TryInto;
use thiserror::Error;


/// A write-ahead-log.
pub struct LogFile {
    file: AdvisoryFileLock,
    path: PathBuf,

    /// The index of the first log entry stored
    first_index: u64,
    len: u64,
}

impl LogFile {
    /// The first entry in the log
    fn first_entry<'l>(&'l mut self) -> Result<LogEntry<'l>, LogError> {
        self.file.seek(SeekFrom::Start(8))?;

        let index = self.first_index;

        Ok(LogEntry {
            log: self,
            index
        })
    }

    /// Returns the index/sequence number of the first entry in the log
    pub fn first_index(&self) -> u64 {
        self.first_index
    }

    /// Returns the index/sequence number of the last entry in the log
    pub fn last_index(&self) -> u64 {
        self.first_index + self.len - 1
    }

    /// Iterate through the log
    pub fn iter<'s, R: RangeBounds<u64>>(&'s mut self, range: R) -> Result<LogIterator<'s>, LogError> {
        if self.len == 0 {
            return Ok(LogIterator {
                next: None,
                last_index: self.first_index
            });
        }

        let last_index = match range.end_bound() {
            Bound::Unbounded => self.last_index(),
            Bound::Included(x) if self.last_index() > *x => *x,
            Bound::Excluded(x) if self.last_index() > *x - 1 => *x - 1,
            _ => return Err(LogError::OutOfBounds)
        };

        let start = match range.start_bound() {
            Bound::Unbounded => self.first_entry()?,
            Bound::Included(x) => self.first_entry()?.seek(*x)?,
            Bound::Excluded(x) => self.first_entry()?.seek(*x + 1)?
        };

        Ok(LogIterator {
            next: Some(start),
            last_index
        })
    }

    /// Write the given log entry to the end of the log
    pub fn write<R: AsMut<[u8]>>(&mut self, entry: &mut R) -> io::Result<()> {
        let end_pos = self.file.seek(SeekFrom::End(0))?;
        
        let entry = entry.as_mut();
        
        let hash = {
            let mut hasher = crc32fast::Hasher::new();
            hasher.update(entry);
            &mut hasher.finalize().to_le_bytes()
        };

        let result = 
            self.file.write_all(&mut (entry.len() as u64).to_le_bytes())
                .and_then(|_| self.file.write_all(entry))
                .and_then(|_| self.file.write_all(hash));

        if result.is_ok() {
            self.len += 1;
        } else {
            // Go back to the end of the file and trim the data written.
            self.file.seek(SeekFrom::Start(end_pos))?;
            self.file.set_len(end_pos + 1)?;
        }
        
        result
    }

    /// Flush writes to disk
    pub fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }

    /// Open the log. Takes out an advisory lock.
    ///
    /// This is O(n): we have to iterate to the end of the log in order to clean interrupted writes and determine the length of the log
    pub fn open<P: AsRef<std::path::Path>>(
        path: P,
    ) -> Result<LogFile, LogError> {
        let mut file = AdvisoryFileLock::new(&path, advisory_lock::FileLockMode::Exclusive)?;
        let path = path.as_ref().to_owned();


        let file_size = file.metadata()?.len();
        let mut entries: u64 = 0;
        let mut first_index: u64 = 0;

        if file_size >= 20 {
            first_index = file.read_u64()?;

            let mut pos = 8;

            while file_size - pos > 8 {
                let entry_data_len = file.read_u64()? + 4; // 4 byte checksum

                if file_size - pos - 8 < entry_data_len {
                    // the entry was not fully written
                    break;
                }

                entries += 1;
                pos = file.seek(SeekFrom::Current(entry_data_len.try_into().unwrap()))?;
            }

            file.set_len(pos)?;
        } else {
            file.write_all(&mut [0;8][..])?;
            file.set_len(8)?;
        }


        Ok(LogFile {
            path,
            file,
            first_index,
            len: entries
        })
    }

    /// Compact the log, removing entries older than `new_start_index`.
    ///
    /// This is done by copying all entries `>= new_start_index` to a temporary file, than overriding the
    /// old log file once the copy is complete.
    ///
    /// Before compacting, the log is flushed.
    pub fn compact(&mut self, new_start_index: u64) -> Result<(), LogError> {
        self.flush()?;

        {
            let entry = self.first_entry()?;
            entry.seek(new_start_index)?;
        }
        

        let mut temp_file_path = std::env::temp_dir().to_path_buf();
        temp_file_path.set_file_name(format!("log-{}", rand::random::<u32>()));
        let mut new_file = AdvisoryFileLock::new(temp_file_path.as_path(), advisory_lock::FileLockMode::Exclusive)?;

        new_file.write_all(&mut new_start_index.to_le_bytes())?;
        io::copy(&mut *self.file, &mut *new_file)?;

        std::fs::rename(temp_file_path, self.path.clone())?;
        self.file = new_file;

        self.len = self.len - (new_start_index - self.first_index);
        self.first_index = new_start_index;

        Ok(())
    }
}


#[derive(Debug, Error)]
pub enum LogError {
    #[error("Bad checksum")]
    BadChecksum,
    #[error("Out of bounds")]
    OutOfBounds,
    #[error("{0}")]
    IoError(#[source] #[from] io::Error),
    #[error("the log is locked")]
    AlreadyLocked,
}

impl From<advisory_lock::FileLockError> for LogError {
    fn from(err: advisory_lock::FileLockError) -> Self {
        match err {
            advisory_lock::FileLockError::IOError(err) => LogError::IoError(err),
            advisory_lock::FileLockError::AlreadyLocked => LogError::AlreadyLocked
        }
    }
}

struct LogEntry<'l> {
    log: &'l mut LogFile,
    index: u64
}

impl<'l> LogEntry<'l> {
    /// Reads into the io::Write. Then returns the next log entry.
    ///
    /// N.b. the next log entry might be out-of-bounds. The implementation of this method may change.
    fn read_to_next<W: Write>(self, write: &mut W) -> Result<LogEntry<'l>, LogError> {
        let LogEntry {log, index} = self;
        let len = log.file.read_u64()?;

        let mut hasher = crc32fast::Hasher::new();

        {
            let mut bytes_left: usize = len.try_into().expect("Log entry is too large to read on a 32 bit platform.");
            let mut buf = [0; 8 * 1024];

            while bytes_left > 0 {
                let read = bytes_left.min(buf.len());
                let read = log.file.read(&mut buf[..read])?;
                
                hasher.update(&buf[..read]);
                write.write_all(&buf[..read])?;

                bytes_left -= read;
            }
        }

        let checksum = log.file.read_u32()?;

        if checksum != hasher.finalize() {
            return Err(LogError::BadChecksum);
        }

        Ok(LogEntry {
            log,
            index: index + 1
        })
    }

    /// Seek to the index
    pub fn seek(self, to_index: u64) -> Result<LogEntry<'l>, LogError> {
        let LogEntry {log, index} = self;

        if to_index > log.first_index + log.len || to_index < log.first_index {
            return Err(LogError::OutOfBounds)
        }

        if index > to_index {
            return log.first_entry()?.seek(to_index)
        }

        for _ in index..to_index {
            let len = log.file.read_u64()?;

            // Move forwards through the length of the current log entry and the 4 byte checksum
            log.file.seek(SeekFrom::Current((len + 4).try_into().unwrap()))?;
        }

        Ok(LogEntry {
            log,
            index: to_index
        })
    }
}

pub struct LogIterator<'l> {
    next: Option<LogEntry<'l>>,
    last_index: u64
}

impl<'l> Iterator for LogIterator<'l> {
    type Item = Result<Vec<u8>, LogError>;

    fn next(&mut self) -> Option<Self::Item> {
        let entry = self.next.take()?;

        if entry.index > self.last_index { return None };

        let mut content = Vec::new();

        Some(
            match entry.read_to_next(&mut content) {
                Ok(next) => {
                    self.next = Some(next);

                    Ok(content)
                },
                Err(err) => Err(err)
            }
        )
    }
}

trait ReadExt {
    fn read_u64(&mut self) -> Result<u64, io::Error>;
    fn read_u32(&mut self) -> Result<u32, io::Error>;
}

impl<R: Read> ReadExt for R {
    fn read_u64(&mut self) -> Result<u64, io::Error> {
        let mut bytes = [0;8];
        self.read_exact(&mut bytes)?;
        Ok(u64::from_le_bytes(bytes))
    }
    fn read_u32(&mut self) -> Result<u32, io::Error> {
        let mut bytes = [0;4];
        self.read_exact(&mut bytes)?;
        Ok(u32::from_le_bytes(bytes))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn it_works() {
        let path = std::path::Path::new("./wal-log-test");

        let _ = std::fs::remove_file(path);

        let entries = & [
            b"test".to_vec(),
            b"foobar".to_vec()
        ];

        {
            let mut log = LogFile::open(path).unwrap();

            // write to log
            for entry in entries {
                log.write(&mut entry.clone()).unwrap();
            }

            log.flush().unwrap();

            // read back and ensure entries match what was written
            for (read, written) in log.iter(..).unwrap().zip(entries.iter()) {
                assert_eq!(&read.unwrap(), written);
            }
        }

        {
            // test after closing and reopening
            let mut log = LogFile::open(path).unwrap();


            let read= log.iter(..).unwrap().map(|entry| {
                entry.unwrap()
            });
        
            assert!(read.eq(entries.to_vec()));
        }

        std::fs::remove_file(path).unwrap();
    }


    #[test]
    fn compaction() {
        let path = std::path::Path::new("./wal-log-compaction");

        let _ = std::fs::remove_file(path);

        let entries = & [
            b"test".to_vec(),
            b"foobar".to_vec(),
            b"bbb".to_vec(),
            b"aaaaa".to_vec(),
            b"11".to_vec(),
            b"222".to_vec(),
            [9; 200].to_vec(),
            b"bar".to_vec()
        ];

        {
            let mut log = LogFile::open(path).unwrap();

            // write to log
            for entry in entries {
                log.write(&mut entry.clone()).unwrap();
            }

            assert_eq!(log.first_index(), 0);

            log.compact(4).unwrap();

            assert_eq!(log.first_index(), 4);
            assert!(log.iter(..).unwrap().map(|a| a.unwrap()).eq(entries[4..].to_vec().into_iter()));

            log.flush().unwrap();
        }

        {
            let mut log = LogFile::open(path).unwrap();
            assert_eq!(log.first_index(), 4);
            assert!(log.iter(..).unwrap().map(|a| a.unwrap()).eq(entries[4..].to_vec().into_iter()));
        }

        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn handles_trimmed_wal() {

        let path = std::path::Path::new("./wal-log-test-trimmed");

        let _ = std::fs::remove_file(path);

        let entries = & [
            b"test".to_vec(),
            b"foobar".to_vec()
        ];

        {
            let mut log = LogFile::open(path).unwrap();

            // write to log
            for entry in entries {
                log.write(&mut entry.clone()).unwrap();
            }

            log.flush().unwrap();
        }
        
        {
            // trim last log entry to cause chaos
            let mut file = std::fs::OpenOptions::new().write(true).read(true).open(path).unwrap();
            file.set_len(38).unwrap();
            file.flush().unwrap();
        }
        

        {
            // test after closing and reopening
            let mut log = LogFile::open(path).unwrap();


            let read= log.iter(..).unwrap().map(|entry| {
                entry.unwrap()
            });
        
            assert!(read.eq(entries[..1].to_vec()));
        }

        std::fs::remove_file(path).unwrap();
    }
}
