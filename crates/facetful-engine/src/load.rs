//! Opening an image while it downloads. Bytes are appended as they arrive;
//! once the header and dictionaries are in, the table exists with the row
//! groups seen so far, and each group joins its catalog as soon as its bytes
//! are complete. The word index can be built group by group meanwhile, so it
//! is ready when the last byte is. When the footer arrives, `finish` checks
//! the streamed groups against it and the table carries on with the footer's
//! catalog (statistics, layout flags): no copy, no second parse of the data.
//!
//! The table is not queried before `finish`: masks and statistics assume a
//! table that no longer changes.

use crate::format::read::{self, NextGroup, ReadAt};
use crate::format::FormatError;
use crate::Table;

pub struct TableLoad<S: ReadAt> {
    /// bytes received before the header and dictionaries were complete
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
        TableLoad {
            pending: Vec::with_capacity(expected_len),
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

    /// Append the next bytes of the image.
    pub fn push(&mut self, chunk: &[u8]) -> Result<(), FormatError> {
        match &mut self.table {
            None => {
                self.pending.extend_from_slice(chunk);
                let Some((cat, first)) = read::open_prefix(&self.pending)? else { return Ok(()) };
                let mut bytes = std::mem::take(&mut self.pending);
                // the header says how long the image is: room for all of it
                // now rather than doubling as it arrives
                if let Some(n) = cat.image_len {
                    bytes.reserve_exact((n as usize).saturating_sub(bytes.len()));
                }
                let src = S::from_memory(bytes).ok_or(FormatError::Io("source cannot hold memory"))?;
                self.table = Some(Table::from_catalog(src, cat));
                self.next = first;
                #[cfg(feature = "search")]
                self.start_index()?;
            }
            Some(t) => t.src.memory_mut().ok_or(FormatError::Io("source cannot grow"))?.extend_from_slice(chunk),
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

    /// The header and dictionaries are in: the schema is known.
    pub fn header_ready(&self) -> bool {
        self.table.is_some()
    }

    /// Row groups whose bytes are complete.
    pub fn groups_ready(&self) -> usize {
        self.table.as_ref().map_or(0, |t| t.cat.groups.len())
    }

    /// Bytes received.
    pub fn bytes(&self) -> usize {
        self.table.as_ref().map_or(self.pending.len(), |t| t.src.len() as usize)
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
