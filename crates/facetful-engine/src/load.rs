//! Opening an image while it downloads. Bytes are appended as they arrive;
//! once the header and dictionaries are in, the table exists with the row
//! groups seen so far, and each group joins its catalog as soon as its bytes
//! are complete. The word index can be built group by group meanwhile, so it
//! is ready when the last byte is. When the footer arrives, `finish` checks
//! the streamed groups against it and the table carries on with the footer's
//! catalog (statistics, layout flags): no copy, no second parse of the data.
//!
//! A packed image (`format::packed`) is unpacked as it arrives: its bytes go
//! through an `Unpacker`, which appends the image they stand for to the same
//! buffer a whole row group at a time, and everything else here reads that
//! buffer as if the image had been sent as it is.
//!
//! The table is not queried before `finish`: masks and statistics assume a
//! table that no longer changes.

use crate::format::packed::{self, Unpacker};
use crate::format::read::{self, NextGroup, ReadAt};
use crate::format::FormatError;
use crate::Table;

pub struct TableLoad<S: ReadAt> {
    /// the first bytes received, until there are enough to tell a packed
    /// image from one sent as it is
    sniff: Option<Vec<u8>>,
    /// for a packed image: turns what arrives into the image
    unpacker: Option<Unpacker>,
    /// the image's bytes before its header and dictionaries were complete
    pending: Vec<u8>,
    table: Option<Table<S>>,
    /// where the next row group starts
    next: u64,
    /// the groups are over: what follows is the footer
    ended: bool,
    /// columns to index, by name, until the header gives their indices
    #[cfg(feature = "search")]
    index_names: Option<Vec<String>>,
    #[cfg(feature = "search")]
    index: Option<crate::search_index::IndexBuilder>,
    #[cfg(feature = "search")]
    index_cols: Vec<usize>,
}

impl<S: ReadAt> TableLoad<S> {
    /// `expected_len`: the image's size when known (an uncompressed
    /// Content-Length), so the buffer is allocated once instead of growing.
    /// Images whose header gives their length need none.
    pub fn new(expected_len: usize) -> TableLoad<S> {
        // a hint (Content-Length, say): room for it when there is room,
        // and growth as the bytes arrive when there is not
        let mut pending = Vec::new();
        let _ = pending.try_reserve_exact(expected_len);
        TableLoad {
            sniff: Some(Vec::new()),
            unpacker: None,
            pending,
            table: None,
            next: 0,
            ended: false,
            #[cfg(feature = "search")]
            index_names: None,
            #[cfg(feature = "search")]
            index: None,
            #[cfg(feature = "search")]
            index_cols: Vec::new(),
        }
    }

    /// Append the next bytes of the image, or of its packed form.
    pub fn push(&mut self, chunk: &[u8]) -> Result<(), FormatError> {
        if let Some(first) = &mut self.sniff {
            first.extend_from_slice(chunk);
            if first.len() < 10 {
                return Ok(());
            }
            let first = self.sniff.take().expect("checked above");
            if packed::is_packed(&first) {
                self.unpacker = Some(Unpacker::new());
            }
            return self.take(&first);
        }
        self.take(chunk)
    }

    /// `push` once the first bytes have said which form is arriving.
    fn take(&mut self, chunk: &[u8]) -> Result<(), FormatError> {
        let image = match &mut self.table {
            None => &mut self.pending,
            Some(t) => t.src.memory_mut().ok_or(FormatError::Io("source cannot grow"))?,
        };
        match &mut self.unpacker {
            Some(un) => un.push(chunk, image)?,
            None => image.extend_from_slice(chunk),
        }
        if self.table.is_none() {
            let Some((cat, first)) = read::open_prefix(&self.pending)? else { return Ok(()) };
            let mut bytes = std::mem::take(&mut self.pending);
            // the header says how long the image is: room for all of it
            // now rather than doubling as it arrives
            // (fallibly: the length comes from the download, and a corrupt
            // one must end in an error, not an allocation failure)
            if let Some(n) = cat.image_len {
                let n = usize::try_from(n).map_err(|_| FormatError::Io("not enough memory for the image"))?;
                bytes
                    .try_reserve_exact(n.saturating_sub(bytes.len()))
                    .map_err(|_| FormatError::Io("not enough memory for the image"))?;
            }
            let src = S::from_memory(bytes).ok_or(FormatError::Io("source cannot hold memory"))?;
            self.table = Some(Table::from_catalog(src, cat));
            self.next = first;
            #[cfg(feature = "search")]
            self.start_index()?;
        }
        let t = self.table.as_mut().expect("opened above");
        let bytes = t.src.read_ref(0, t.src.len() as usize).expect("memory source");
        while !self.ended {
            match read::next_group(&t.cat, bytes, self.next) {
                NextGroup::Ready(g, next) => {
                    t.cat.total_rows += g.row_count as u64;
                    t.cat.groups.push(g);
                    self.next = next;
                }
                NextGroup::NeedBytes => break,
                NextGroup::End => self.ended = true,
            }
        }
        Ok(())
    }

    /// The image is arriving packed (known from its first 10 bytes on).
    pub fn packed(&self) -> bool {
        self.unpacker.is_some()
    }

    /// The image's bytes so far. For a packed image these are what a
    /// loader keeps for the next visit (OPFS), not the bytes it was sent.
    pub fn image(&self) -> &[u8] {
        match &self.table {
            Some(t) => t.src.read_ref(0, t.src.len() as usize).unwrap_or(&[]),
            None => &self.pending,
        }
    }

    /// The header and dictionaries are in: the schema is known.
    pub fn header_ready(&self) -> bool {
        self.table.is_some()
    }

    /// Row groups whose bytes are complete.
    pub fn groups_ready(&self) -> usize {
        self.table.as_ref().map_or(0, |t| t.cat.groups.len())
    }

    /// Bytes of the image so far: those received, or for a packed image
    /// those unpacked.
    pub fn bytes(&self) -> usize {
        let sniffed = self.sniff.as_ref().map_or(0, |s| s.len());
        sniffed + self.table.as_ref().map_or(self.pending.len(), |t| t.src.len() as usize)
    }

    /// Build the word index over these columns while the image arrives
    /// (started as soon as the dictionaries are in).
    #[cfg(feature = "search")]
    pub fn index_columns(&mut self, names: &[&str]) -> Result<(), FormatError> {
        self.index_names = Some(names.iter().map(|s| s.to_string()).collect());
        self.index = None;
        self.start_index()
    }

    #[cfg(feature = "search")]
    fn start_index(&mut self) -> Result<(), FormatError> {
        let (Some(t), Some(names)) = (&mut self.table, &self.index_names) else { return Ok(()) };
        self.index_cols = names
            .iter()
            .map(|n| t.column_index(n).ok_or(FormatError::Corrupt("no such column to index")))
            .collect::<Result<_, _>>()?;
        self.index = Some(crate::search_index::IndexBuilder::new(t, &self.index_cols)?);
        Ok(())
    }

    /// Index up to `groups` of the row groups that have arrived. Returns the
    /// groups indexed so far.
    #[cfg(feature = "search")]
    pub fn index_step(&mut self, groups: usize) -> Result<usize, FormatError> {
        let (Some(t), Some(b)) = (&mut self.table, &mut self.index) else { return Ok(0) };
        let mut n = 0;
        while n < groups && b.groups_done() < t.cat.groups.len() {
            b.add_group(t)?;
            n += 1;
        }
        Ok(b.groups_done())
    }

    /// The whole image is in: check the streamed groups against the footer
    /// and hand over the table. An index under way continues as the table's
    /// build (`search_index_step` finishes it). Should the footer disagree
    /// with what was streamed, the index starts over from the footer's groups.
    pub fn finish(self) -> Result<Table<S>, FormatError> {
        if self.unpacker.as_ref().is_some_and(|un| !un.done()) {
            return Err(FormatError::Truncated);
        }
        let mut t = self.table.ok_or(FormatError::Truncated)?;
        let full = read::open(&t.src)?;
        let agrees = t.cat.groups.len() <= full.groups.len()
            && t.cat.groups.iter().zip(&full.groups).all(|(a, b)| {
                a.offset == b.offset
                    && a.row_count == b.row_count
                    && a.cols.iter().zip(&b.cols).all(|(x, y)| x.null_count == y.null_count && x.seg_lens == y.seg_lens)
            });
        t.cat = full;
        #[cfg(feature = "search")]
        match self.index {
            Some(b) if agrees => t.search_builder = Some(b),
            _ if self.index_names.is_some() => t.begin_search_index(&self.index_cols)?,
            _ => {}
        }
        #[cfg(not(feature = "search"))]
        let _ = agrees;
        Ok(t)
    }
}
