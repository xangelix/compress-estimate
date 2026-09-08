//! Data-source abstraction: random-access reads shared across threads.

use std::fs::File;
use std::io;
use std::os::unix::fs::FileExt;
use std::path::Path;

/// A seekable data source supporting positioned reads from many threads at
/// once — the access pattern our stratified sampler relies on.
pub trait Source: Send + Sync {
    /// Total length in bytes.
    fn len(&self) -> u64;

    /// Read up to `buf.len()` bytes starting at `offset`; returns bytes read.
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize>;

    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Read exactly `buf.len()` bytes (or fewer at EOF) starting at `offset`.
    fn read_exact_at(&self, mut offset: u64, mut buf: &mut [u8]) -> io::Result<()> {
        while !buf.is_empty() {
            let n = self.read_at(offset, buf)?;
            if n == 0 {
                break;
            }
            offset += n as u64;
            buf = &mut buf[n..];
        }
        Ok(())
    }
}

impl Source for File {
    fn len(&self) -> u64 {
        self.metadata().map(|m| m.len()).unwrap_or(0)
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        FileExt::read_at(self, buf, offset)
    }
}

impl Source for [u8] {
    fn len(&self) -> u64 {
        <[u8]>::len(self) as u64
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        let start = (offset as usize).min(self.len());
        let n = buf.len().min(self.len() - start);
        buf[..n].copy_from_slice(&self[start..start + n]);
        Ok(n)
    }
}

impl Source for &[u8] {
    fn len(&self) -> u64 {
        let data: &[u8] = self;
        data.len() as u64
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        let data: &[u8] = self;
        let start = (offset as usize).min(data.len());
        let n = buf.len().min(data.len() - start);
        buf[..n].copy_from_slice(&data[start..start + n]);
        Ok(n)
    }
}

/// Open a file as a [`Source`].
pub fn open_path(path: &Path) -> io::Result<File> {
    File::open(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn slice_source_reads() {
        let data: Vec<u8> = (0..=255).cycle().take(1000).collect();
        let mut buf = [0u8; 100];
        data.as_slice().read_exact_at(950, &mut buf).unwrap();
        assert_eq!(&buf[..50], &data[950..1000]);
        // Short read at EOF leaves the tail untouched but succeeds.
    }

    #[test]
    fn file_source_parallel_reads() {
        let mut path = std::env::temp_dir();
        path.push(format!("ce-src-test-{}", std::process::id()));
        {
            let mut f = File::create(&path).unwrap();
            let data: Vec<u8> = (0..65536u32).map(|i| (i % 251) as u8).collect();
            f.write_all(&data).unwrap();
        }
        let f = File::open(&path).unwrap();
        assert_eq!(Source::len(&f), 65536);
        let fr = &f;
        std::thread::scope(|s| {
            for t in 0..4 {
                s.spawn(move || {
                    let mut buf = [0u8; 64];
                    let off = (t * 1000) as u64;
                    Source::read_exact_at(fr, off, &mut buf).unwrap();
                    for (i, &b) in buf.iter().enumerate() {
                        assert_eq!(b, ((off as usize + i) % 251) as u8);
                    }
                });
            }
        });
        std::fs::remove_file(&path).ok();
    }
}
