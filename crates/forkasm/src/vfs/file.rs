//! File contents: shared fixed-size chunks, copied on write.

use std::sync::Arc;

/// Chunk size: writing one byte copies (at most) this much.
pub const CHUNK: usize = 64 * 1024;

type Chunk = Arc<[u8; CHUNK]>;

/// A file's bytes. `None` chunks are holes (zeros).
#[derive(Clone, Debug, Default)]
pub struct FileData {
    chunks: Vec<Option<Chunk>>,
    len: u64,
}

impl FileData {
    pub fn len(&self) -> u64 {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Reads from `offset` into `buf`; returns the bytes read (0 at EOF).
    pub fn read(&self, offset: u64, buf: &mut [u8]) -> usize {
        if offset >= self.len {
            return 0;
        }
        let n = buf.len().min((self.len - offset) as usize);
        let mut done = 0;
        while done < n {
            let pos = offset as usize + done;
            let (index, within) = (pos / CHUNK, pos % CHUNK);
            let take = (CHUNK - within).min(n - done);
            match &self.chunks[index] {
                Some(chunk) => buf[done..done + take].copy_from_slice(&chunk[within..within + take]),
                None => buf[done..done + take].fill(0),
            }
            done += take;
        }
        n
    }

    /// Writes `data` at `offset`, growing the file (with a hole) if needed.
    pub fn write(&mut self, offset: u64, data: &[u8]) {
        let end = offset as usize + data.len();
        if self.chunks.len() < end.div_ceil(CHUNK) {
            self.chunks.resize(end.div_ceil(CHUNK), None);
        }
        let mut done = 0;
        while done < data.len() {
            let pos = offset as usize + done;
            let (index, within) = (pos / CHUNK, pos % CHUNK);
            let take = (CHUNK - within).min(data.len() - done);
            let chunk = self.chunks[index].get_or_insert_with(|| Arc::new([0; CHUNK]));
            Arc::make_mut(chunk)[within..within + take].copy_from_slice(&data[done..done + take]);
            done += take;
        }
        self.len = self.len.max(end as u64);
    }

    pub fn truncate(&mut self, len: u64) {
        if len < self.len {
            let len_usize = len as usize;
            self.chunks.truncate(len_usize.div_ceil(CHUNK));
            // Zero the tail of the last chunk so a later extension reads zeros.
            if let Some(Some(last)) = self.chunks.last_mut()
                && !len_usize.is_multiple_of(CHUNK)
            {
                Arc::make_mut(last)[len_usize % CHUNK..].fill(0);
            }
        }
        self.len = len;
    }

    /// Chunks physically shared with `other` (for tests and stats).
    pub fn shared_chunks(&self, other: &FileData) -> usize {
        self.chunks
            .iter()
            .zip(&other.chunks)
            .filter(|(a, b)| matches!((a, b), (Some(a), Some(b)) if Arc::ptr_eq(a, b)))
            .count()
    }
}
