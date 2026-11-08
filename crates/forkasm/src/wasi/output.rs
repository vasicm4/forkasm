//! Captured stdout/stderr.

use std::sync::Arc;

/// Bytes kept unsealed at the end of an [`Output`]; clones copy at most this.
const CHUNK: usize = 4096;

/// An append-only byte log. Clones share every full chunk, so forking a
/// sandbox that has printed a lot copies at most one partial chunk instead of
/// everything it printed.
#[derive(Clone, Debug, Default)]
pub struct Output {
    sealed: imbl::Vector<Arc<[u8]>>,
    tail: Vec<u8>,
    len: usize,
}

impl Output {
    pub fn append(&mut self, data: &[u8]) {
        self.tail.extend_from_slice(data);
        self.len += data.len();
        if self.tail.len() >= CHUNK {
            self.sealed.push_back(Arc::from(std::mem::take(&mut self.tail)));
        }
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Everything written from byte `start` on.
    pub fn bytes_from(&self, start: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.len.saturating_sub(start));
        let mut at = 0;
        for chunk in self.sealed.iter().map(|c| &c[..]).chain([&self.tail[..]]) {
            let end = at + chunk.len();
            if end > start {
                out.extend_from_slice(&chunk[start.saturating_sub(at)..]);
            }
            at = end;
        }
        out
    }

    /// Everything written.
    pub fn to_vec(&self) -> Vec<u8> {
        self.bytes_from(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn appends_seal_and_share() {
        let mut o = Output::default();
        let mut expected = Vec::new();
        for i in 0..3000u32 {
            let line = format!("line {i}\n");
            o.append(line.as_bytes());
            expected.extend_from_slice(line.as_bytes());
        }
        assert_eq!(o.len(), expected.len());
        assert_eq!(o.to_vec(), expected);
        assert_eq!(o.bytes_from(10_000), expected[10_000..]);
        assert!(o.tail.len() < CHUNK && !o.sealed.is_empty());
        let fork = o.clone();
        assert!(Arc::ptr_eq(&fork.sealed[0], &o.sealed[0]), "clones share sealed chunks");
        o.append(b"parent only");
        assert_eq!(fork.to_vec(), expected);
        assert!(o.bytes_from(expected.len()) == b"parent only");
    }
}
