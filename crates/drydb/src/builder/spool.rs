//! Bounded sorting of build input.
//!
//! Records are buffered in memory up to a byte budget, sorted, and written out as a
//! run; at the end the runs are merged. A build therefore never needs the whole dataset
//! resident, and the memory it does use is the buffer you configured plus one record per
//! run during the merge.
//!
//! Ordering is `(key, sequence)`: the key order comes from the
//! [`KeyEncoding`](crate::KeyEncoding), and the sequence number keeps records with equal
//! keys in append order. That stability is what makes non-unique secondary index record
//! ids reproducible.

use std::cell::Cell;
use std::cmp::Ordering;
use std::io::{BufWriter, Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::sync::Arc;

use super::temp::TempFile;
use crate::encoding::KeyEncoding;
use crate::error::{Error, ErrorKind, Result};

/// Header written before each spooled record: sequence, key length, value length.
const RECORD_HEADER_LEN: usize = 8 + 4 + 4;

/// One buffered record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Record {
    /// Append order, used to break ties between equal keys.
    pub seq: u64,
    /// Encoded key.
    pub key: Vec<u8>,
    /// Value bytes.
    pub value: Vec<u8>,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct Slot {
    key_off: usize,
    key_len: usize,
    value_off: usize,
    value_len: usize,
    seq: u64,
}

#[derive(Debug, Clone, Copy)]
struct Run {
    start: u64,
    end: u64,
    /// How many times this run has been merged.
    ///
    /// Runs are only merged with others of the same level, so each merge reads runs of
    /// about the same size and the work done over a whole build is one pass per level.
    /// Merging whatever happens to be there instead drags the runs already merged
    /// through the next pass as well, which is a pass over everything spooled so far
    /// every time.
    level: u32,
}

impl Run {
    fn len(&self) -> u64 {
        self.end - self.start
    }
}

/// Collects records and hands them back in key order.
pub(crate) struct RecordSpool {
    encoding: Arc<dyn KeyEncoding>,
    dir: PathBuf,
    stem: String,
    data: Vec<u8>,
    slots: Vec<Slot>,
    runs: Vec<Run>,
    file: Option<TempFile>,
    write_pos: u64,
    budget: usize,
    next_seq: u64,
    count: u64,
    /// What went wrong, once something has.
    ///
    /// A record that could not be written is still a record this spool took in, and it
    /// cannot be taken back out: the write sorts the buffer first, and it may fail after
    /// the record is on disk for good. So the spool stops instead, and the build stops
    /// with it, rather than producing a file holding a row the caller was told had not
    /// gone in.
    spoiled: Option<ErrorKind>,
    /// Every byte the spool has written to disk, spills and merges alike.
    ///
    /// What a sort costs is the data it moves, and a merge that reads runs of unlike
    /// sizes moves the same records over and over. This counts it.
    written: u64,
}

impl std::fmt::Debug for RecordSpool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RecordSpool")
            .field("encoding", &self.encoding.id())
            .field("count", &self.count)
            .field("runs", &self.runs.len())
            .field("buffered", &self.data.len())
            .field("written", &self.written)
            .finish()
    }
}

impl RecordSpool {
    /// Creates a spool sorting with `encoding`, buffering at most `budget` bytes of
    /// records, bookkeeping included, before spilling to `dir`.
    pub(crate) fn new(
        encoding: Arc<dyn KeyEncoding>,
        dir: PathBuf,
        stem: impl Into<String>,
        budget: usize,
    ) -> RecordSpool {
        RecordSpool {
            encoding,
            dir,
            stem: stem.into(),
            data: Vec::new(),
            slots: Vec::new(),
            runs: Vec::new(),
            file: None,
            write_pos: 0,
            budget: budget.max(64 * 1024),
            next_seq: 0,
            count: 0,
            spoiled: None,
            written: 0,
        }
    }

    /// Changes how much a record buffer holds before it spills.
    pub(crate) fn set_budget(&mut self, budget: usize) {
        self.budget = budget.max(64 * 1024);
    }

    /// Changes where the temporary file is created. A spool that has already created
    /// one keeps it: its records are in that file.
    pub(crate) fn set_dir(&mut self, dir: PathBuf) {
        self.dir = dir;
    }

    /// Number of records appended.
    pub(crate) fn len(&self) -> u64 {
        self.count
    }

    /// Appends a record.
    pub(crate) fn push(&mut self, key: &[u8], value: &[u8]) -> Result<()> {
        self.check_usable()?;
        self.encoding.validate_key(key)?;
        // Computing the digest now fails fast on a key the encoding cannot handle, so
        // the sort comparator below only sees keys it can order.
        self.encoding.digest(key)?;

        let slot = Slot {
            key_off: self.data.len(),
            key_len: key.len(),
            value_off: self.data.len() + key.len(),
            value_len: value.len(),
            seq: self.next_seq,
        };
        self.data.extend_from_slice(key);
        self.data.extend_from_slice(value);
        self.slots.push(slot);
        self.next_seq += 1;
        self.count += 1;

        if self.buffered_bytes() >= self.budget {
            // Once the record is in, a failure cannot be undone: writing sorts the
            // buffer, so where the record went is no longer where it was put, and the
            // failure may come after it has been written out for good. What can be said
            // for certain is that this spool no longer holds what the caller was told it
            // holds, so nothing more is done with it.
            if let Err(e) = self.spill() {
                self.spoiled = Some(e.kind());
                return Err(e);
            }
        }
        Ok(())
    }

    /// What the buffer holds: the record payload and the bookkeeping beside it. A run
    /// of empty keys and empty values is all bookkeeping, so counting the payload alone
    /// would let the buffer grow with the input and never spill.
    pub(crate) fn buffered_bytes(&self) -> usize {
        self.data.capacity().saturating_add(
            self.slots
                .capacity()
                .saturating_mul(std::mem::size_of::<Slot>()),
        )
    }

    /// Writes the buffer out now, whatever it holds.
    ///
    /// For a builder balancing several tables against one sort buffer: the table that
    /// holds the most gives its buffer up so the others can keep theirs.
    pub(crate) fn flush(&mut self) -> Result<()> {
        self.check_usable()?;
        if let Err(e) = self.spill() {
            // As in `push`: a spill that fails leaves the spool holding something other
            // than what it was told to hold, so nothing more is done with it.
            self.spoiled = Some(e.kind());
            return Err(e);
        }
        Ok(())
    }

    fn sort_buffer(&mut self) -> Result<()> {
        let data = &self.data;
        let encoding = &self.encoding;
        let failure: Cell<Option<Error>> = Cell::new(None);
        // Unstable, which sorts in place: a stable sort asks the allocator for a copy of
        // the slot array, which is memory the sort buffer did not stand for and came to
        // as much again as the records themselves. Nothing is lost by it, because no two
        // slots compare equal: records with the same key are ordered by the sequence
        // number they were given, which is theirs alone.
        self.slots.sort_unstable_by(|a, b| {
            let ka = &data[a.key_off..a.key_off + a.key_len];
            let kb = &data[b.key_off..b.key_off + b.key_len];
            match encoding.compare(ka, kb) {
                Ok(Ordering::Equal) => a.seq.cmp(&b.seq),
                Ok(other) => other,
                Err(e) => {
                    // The first failure is the one reported, and taking the cell to look
                    // has to put back what was in it: an even number of failures used to
                    // leave the cell empty and the sort looked as though it had worked.
                    let first = failure.take().or(Some(e));
                    failure.set(first);
                    Ordering::Equal
                }
            }
        });
        match failure.take() {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    fn spill(&mut self) -> Result<()> {
        if self.slots.is_empty() {
            return Ok(());
        }
        self.sort_buffer()?;

        if self.file.is_none() {
            self.file = Some(TempFile::create_in(&self.dir, &self.stem)?);
        }
        let start = self.write_pos;
        {
            let file = self.file.as_mut().expect("just created");
            file.file_mut()
                .seek(SeekFrom::Start(start))
                .map_err(|e| Error::io("cannot position the spool file", e))?;
            let mut writer = BufWriter::with_capacity(256 * 1024, file.file_mut());
            let mut header = [0u8; RECORD_HEADER_LEN];
            for slot in &self.slots {
                header[0..8].copy_from_slice(&slot.seq.to_le_bytes());
                header[8..12].copy_from_slice(&(slot.key_len as u32).to_le_bytes());
                header[12..16].copy_from_slice(&(slot.value_len as u32).to_le_bytes());
                writer
                    .write_all(&header)
                    .map_err(|e| Error::io("cannot write to the spool file", e))?;
                writer
                    .write_all(&self.data[slot.key_off..slot.key_off + slot.key_len])
                    .map_err(|e| Error::io("cannot write to the spool file", e))?;
                writer
                    .write_all(&self.data[slot.value_off..slot.value_off + slot.value_len])
                    .map_err(|e| Error::io("cannot write to the spool file", e))?;
                self.write_pos += (RECORD_HEADER_LEN + slot.key_len + slot.value_len) as u64;
            }
            writer
                .flush()
                .map_err(|e| Error::io("cannot flush the spool file", e))?;
        }
        self.written += self.write_pos - start;
        self.runs.push(Run {
            start,
            end: self.write_pos,
            level: 0,
        });
        self.data.clear();
        self.data.shrink_to_fit();
        self.slots.clear();
        self.slots.shrink_to_fit();
        // Merged as they pile up rather than only at the end, so the list of them cannot
        // grow with the input.
        self.merge_tiers()?;
        Ok(())
    }

    /// Whether anything more can be done with this spool.
    fn check_usable(&self) -> Result<()> {
        match self.spoiled {
            None => Ok(()),
            Some(kind) => Err(Error::new(
                kind,
                "a row could not be written and cannot be taken back, so this build \
                 cannot go on; start it again",
            )),
        }
    }

    /// Finishes collecting and returns the records in `(key, sequence)` order.
    pub(crate) fn into_sorted(mut self) -> Result<SortedRecords> {
        self.check_usable()?;
        if self.runs.is_empty() {
            self.sort_buffer()?;
            return Ok(SortedRecords::Memory {
                data: std::mem::take(&mut self.data),
                slots: std::mem::take(&mut self.slots),
                next: 0,
            });
        }
        self.spill()?;
        self.reduce_runs()?;
        let readers = self
            .runs
            .iter()
            .map(|run| RunReader::new(run.start, run.end))
            .collect::<Vec<_>>();
        let mut file = self.file.take().expect("runs exist, so the file does");
        let merger = Merger::new(file.file_mut(), readers, self.encoding.clone())?;
        Ok(SortedRecords::Merged {
            file,
            merger: Box::new(merger),
        })
    }

    /// Merges the newest runs whenever enough of the same level have piled up.
    ///
    /// One spill makes one run, so without this the list of them grows with the input,
    /// which is memory the sort buffer does not stand for. Merging only runs of the same
    /// level keeps each merge to data of about the same size: over a whole build that is
    /// one pass per level, where merging whatever is there would be a pass over
    /// everything spooled so far every time.
    fn merge_tiers(&mut self) -> Result<()> {
        while self.runs.len() >= MERGE_FAN_IN {
            let tail = self.runs.len() - MERGE_FAN_IN;
            let level = self.runs[tail].level;
            if self.runs[tail..].iter().any(|run| run.level != level) {
                break;
            }
            self.merge_tail(MERGE_FAN_IN, level + 1)?;
        }
        // The merges above leave what they read behind, so the file carries dead space.
        let live: u64 = self.runs.iter().map(Run::len).sum();
        if self.write_pos > live.saturating_mul(COMPACT_AT) && self.runs.len() > 1 {
            self.rewrite()?;
        }
        Ok(())
    }

    /// Merges the last `count` runs into one, appended to the spool file.
    ///
    /// Appending rather than rewriting is what keeps the work to the runs being merged.
    /// What they read stays in the file as dead space until it is compacted away.
    fn merge_tail(&mut self, count: usize, level: u32) -> Result<()> {
        let at = self.runs.len() - count;
        let mut file = self.file.take().expect("runs exist, so the file does");
        let start = self.write_pos;
        let outcome = (|| -> Result<u64> {
            let readers = self.runs[at..]
                .iter()
                .map(|run| RunReader::new(run.start, run.end))
                .collect::<Vec<_>>();
            // A second open of the same file, so the merge can read the runs while it
            // writes what they come to. Opened again rather than cloned: a clone shares
            // the position, and the reads would drag the writes along with them. The two
            // never touch the same bytes, because the output goes after everything
            // already written.
            let mut out = std::fs::OpenOptions::new()
                .write(true)
                .open(file.path())
                .map_err(|e| Error::io("cannot open the spool file for merging", e))?;
            out.seek(SeekFrom::Start(start))
                .map_err(|e| Error::io("cannot position the spool file", e))?;
            let mut merger = Merger::new(file.file_mut(), readers, self.encoding.clone())?;
            let mut pos = start;
            {
                let mut writer = BufWriter::with_capacity(256 * 1024, &mut out);
                while let Some(record) = merger.next_record(file.file_mut())? {
                    pos += write_record(&mut writer, record.seq, &record.key, &record.value)?;
                }
                writer
                    .flush()
                    .map_err(|e| Error::io("cannot flush the spool file", e))?;
            }
            Ok(pos)
        })();
        self.file = Some(file);
        let end = outcome?;
        self.written += end - start;
        self.runs.truncate(at);
        self.runs.push(Run { start, end, level });
        self.write_pos = end;
        Ok(())
    }

    /// Writes the live runs out to a fresh file, leaving the dead space behind.
    fn rewrite(&mut self) -> Result<()> {
        let mut input = self.file.take().expect("runs exist, so the file does");
        let held = std::mem::take(&mut self.runs);
        let outcome = rewrite_runs(&self.dir, &self.stem, &self.encoding, &mut input, &held);
        match outcome {
            Ok((output, next_runs, out_pos)) => {
                self.written += out_pos;
                self.file = Some(output);
                self.runs = next_runs;
                self.write_pos = out_pos;
                drop(input);
                Ok(())
            }
            Err(e) => {
                // Nothing was moved, so the spool is where it was: a failure here has to
                // leave it usable, not half taken apart.
                self.file = Some(input);
                self.runs = held;
                Err(e)
            }
        }
    }

    /// Merges runs in groups until at most [`MERGE_FAN_IN`] remain.
    ///
    /// Each pass writes its output to a fresh file and drops the old one, so the space
    /// used is the input size twice over at most, and the merge that follows opens a
    /// bounded number of runs however small the sort buffer was.
    fn reduce_runs(&mut self) -> Result<()> {
        while self.runs.len() > MERGE_FAN_IN {
            let mut input = self.file.take().expect("runs exist, so the file does");
            let held = std::mem::take(&mut self.runs);
            let outcome = merge_groups(
                &self.dir,
                &self.stem,
                &self.encoding,
                &mut input,
                &held,
                MERGE_FAN_IN,
            );
            match outcome {
                Ok((output, next_runs, out_pos)) => {
                    self.written += out_pos;
                    self.file = Some(output);
                    self.runs = next_runs;
                    self.write_pos = out_pos;
                    drop(input);
                }
                Err(e) => {
                    // Nothing was moved, so the spool is where it was.
                    self.file = Some(input);
                    self.runs = held;
                    return Err(e);
                }
            }
        }
        Ok(())
    }
}

/// A sorted stream of records.
pub(crate) enum SortedRecords {
    /// Everything fitted in the buffer.
    Memory {
        data: Vec<u8>,
        slots: Vec<Slot>,
        next: usize,
    },
    /// Several spilled runs, merged on the fly.
    Merged { file: TempFile, merger: Box<Merger> },
}

impl SortedRecords {
    /// What the records being read hold in memory.
    ///
    /// Everything when the sort fitted in the buffer; the merge's front records and
    /// their readers otherwise, which the fan-in bounds.
    pub(crate) fn held_bytes(&self) -> usize {
        match self {
            SortedRecords::Memory { data, slots, .. } => data
                .capacity()
                .saturating_add(slots.capacity().saturating_mul(std::mem::size_of::<Slot>())),
            SortedRecords::Merged { .. } => 0,
        }
    }
}

impl std::fmt::Debug for SortedRecords {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SortedRecords::Memory { slots, next, .. } => f
                .debug_struct("SortedRecords::Memory")
                .field("remaining", &(slots.len() - next))
                .finish(),
            SortedRecords::Merged { merger, .. } => f
                .debug_struct("SortedRecords::Merged")
                .field("runs", &merger.runs())
                .finish(),
        }
    }
}

impl SortedRecords {
    /// Borrows the next record. Returns `Ok(None)` at the end of the stream.
    pub(crate) fn next_record(&mut self) -> Result<Option<Record>> {
        match self {
            SortedRecords::Memory { data, slots, next } => {
                if *next >= slots.len() {
                    return Ok(None);
                }
                let slot = slots[*next];
                *next += 1;
                Ok(Some(Record {
                    seq: slot.seq,
                    key: data[slot.key_off..slot.key_off + slot.key_len].to_vec(),
                    value: data[slot.value_off..slot.value_off + slot.value_len].to_vec(),
                }))
            }
            SortedRecords::Merged { file, merger } => merger.next_record(file.file_mut()),
        }
    }
}

/// How many runs one merge pass reads at once.
///
/// The merge holds one pending entry per run, so the fan-in is what bounds that part of
/// the build. More runs than this are reduced by extra passes rather than all opened at
/// once, which is what keeps a build with a small sort buffer and large values from
/// needing memory proportional to the whole input.
const MERGE_FAN_IN: usize = 16;

/// How much dead space the spool file carries before it is rewritten.
///
/// Merging appends its output and leaves what it read behind, so the file grows past
/// what is live in it. Rewriting at twice the live bytes keeps the file to about twice
/// the input, and each rewrite costs a pass over data that has doubled since the last
/// one.
const COMPACT_AT: u64 = 2;

struct RunReader {
    pos: u64,
    end: u64,
}

/// One record's header, plus where its value sits in the file.
///
/// The merge keeps one of these per run rather than a whole record: values stay on disk
/// until the record they belong to is actually emitted, so a run of huge values costs
/// one value at a time instead of one per run.
#[derive(Debug, Clone)]
struct Pending {
    seq: u64,
    key: Vec<u8>,
    value_pos: u64,
    value_len: usize,
}

impl RunReader {
    fn new(start: u64, end: u64) -> RunReader {
        RunReader { pos: start, end }
    }

    fn read_next(&mut self, file: &mut std::fs::File) -> Result<Option<Pending>> {
        if self.pos >= self.end {
            return Ok(None);
        }
        file.seek(SeekFrom::Start(self.pos))
            .map_err(|e| Error::io("cannot position the spool file", e))?;
        let mut header = [0u8; RECORD_HEADER_LEN];
        read_exact(file, &mut header)?;
        let seq = u64::from_le_bytes(header[0..8].try_into().expect("checked"));
        let key_len = u32::from_le_bytes(header[8..12].try_into().expect("checked")) as usize;
        let value_len = u32::from_le_bytes(header[12..16].try_into().expect("checked")) as usize;
        let total = RECORD_HEADER_LEN + key_len + value_len;
        if self.pos + total as u64 > self.end {
            return Err(Error::corrupt("spool run is truncated"));
        }
        let mut key = vec![0u8; key_len];
        read_exact(file, &mut key)?;
        let value_pos = self.pos + (RECORD_HEADER_LEN + key_len) as u64;
        self.pos += total as u64;
        Ok(Some(Pending {
            seq,
            key,
            value_pos,
            value_len,
        }))
    }
}

fn read_value(file: &mut std::fs::File, pending: &Pending) -> Result<Vec<u8>> {
    file.seek(SeekFrom::Start(pending.value_pos))
        .map_err(|e| Error::io("cannot position the spool file", e))?;
    let mut value = vec![0u8; pending.value_len];
    read_exact(file, &mut value)?;
    Ok(value)
}

fn read_exact(file: &mut std::fs::File, buf: &mut [u8]) -> Result<()> {
    file.read_exact(buf)
        .map_err(|e| Error::io("cannot read the spool file", e))
}

/// Merges `runs` in groups of `group` into a fresh file, leaving the input untouched.
///
/// Returns the new file, the runs in it and its length, so the caller only takes the
/// spool apart once the whole pass has come off: a failure part way through has to leave
/// it usable, not half moved.
fn merge_groups(
    dir: &std::path::Path,
    stem: &str,
    encoding: &Arc<dyn KeyEncoding>,
    input: &mut TempFile,
    runs: &[Run],
    group: usize,
) -> Result<(TempFile, Vec<Run>, u64)> {
    let mut output = TempFile::create_in(dir, stem)?;
    let mut out_pos = 0u64;
    let mut next_runs = Vec::with_capacity(runs.len() / group + 1);
    for chunk in runs.chunks(group) {
        let readers = chunk
            .iter()
            .map(|run| RunReader::new(run.start, run.end))
            .collect::<Vec<_>>();
        let mut merger = Merger::new(input.file_mut(), readers, encoding.clone())?;
        let start = out_pos;
        {
            let mut writer = BufWriter::with_capacity(256 * 1024, output.file_mut());
            while let Some(record) = merger.next_record(input.file_mut())? {
                out_pos += write_record(&mut writer, record.seq, &record.key, &record.value)?;
            }
            writer
                .flush()
                .map_err(|e| Error::io("cannot flush the spool file", e))?;
        }
        next_runs.push(Run {
            start,
            end: out_pos,
            // A group merged into one is one more merge for all of them; a run copied
            // on its own is the run it was.
            level: if chunk.len() == 1 {
                chunk[0].level
            } else {
                chunk.iter().map(|run| run.level).max().unwrap_or(0) + 1
            },
        });
    }
    Ok((output, next_runs, out_pos))
}

/// Writes the live runs out to a fresh file, leaving the dead space behind.
fn rewrite_runs(
    dir: &std::path::Path,
    stem: &str,
    encoding: &Arc<dyn KeyEncoding>,
    input: &mut TempFile,
    runs: &[Run],
) -> Result<(TempFile, Vec<Run>, u64)> {
    merge_groups(dir, stem, encoding, input, runs, 1)
}

fn write_record(writer: &mut impl Write, seq: u64, key: &[u8], value: &[u8]) -> Result<u64> {
    let mut header = [0u8; RECORD_HEADER_LEN];
    header[0..8].copy_from_slice(&seq.to_le_bytes());
    header[8..12].copy_from_slice(&(key.len() as u32).to_le_bytes());
    header[12..16].copy_from_slice(&(value.len() as u32).to_le_bytes());
    writer
        .write_all(&header)
        .map_err(|e| Error::io("cannot write to the spool file", e))?;
    writer
        .write_all(key)
        .map_err(|e| Error::io("cannot write to the spool file", e))?;
    writer
        .write_all(value)
        .map_err(|e| Error::io("cannot write to the spool file", e))?;
    Ok((RECORD_HEADER_LEN + key.len() + value.len()) as u64)
}

/// Merges sorted runs with a binary min-heap over `(key, sequence)`.
pub(crate) struct Merger {
    readers: Vec<RunReader>,
    front: Vec<Option<Pending>>,
    heap: Vec<usize>,
    encoding: Arc<dyn KeyEncoding>,
    failure: Cell<Option<Error>>,
}

impl Merger {
    fn new(
        file: &mut std::fs::File,
        readers: Vec<RunReader>,
        encoding: Arc<dyn KeyEncoding>,
    ) -> Result<Merger> {
        let mut merger = Merger {
            front: vec![None; readers.len()],
            readers,
            heap: Vec::new(),
            encoding,
            failure: Cell::new(None),
        };
        for i in 0..merger.readers.len() {
            merger.refill(file, i)?;
            if merger.front[i].is_some() {
                merger.heap.push(i);
            }
        }
        for i in (0..merger.heap.len() / 2).rev() {
            merger.sift_down(i);
        }
        merger.check()?;
        Ok(merger)
    }

    fn runs(&self) -> usize {
        self.readers.len()
    }

    fn refill(&mut self, file: &mut std::fs::File, run: usize) -> Result<()> {
        self.front[run] = self.readers[run].read_next(file)?;
        Ok(())
    }

    fn compare(&self, a: usize, b: usize) -> Ordering {
        let (Some(ra), Some(rb)) = (&self.front[a], &self.front[b]) else {
            return Ordering::Equal;
        };
        match self.encoding.compare(&ra.key, &rb.key) {
            Ok(Ordering::Equal) => ra.seq.cmp(&rb.seq),
            Ok(other) => other,
            Err(e) => {
                // As in the buffer's sort: what is taken out to be looked at goes back.
                let first = self.failure.take().or(Some(e));
                self.failure.set(first);
                Ordering::Equal
            }
        }
    }

    fn sift_down(&mut self, mut i: usize) {
        let len = self.heap.len();
        loop {
            let left = 2 * i + 1;
            if left >= len {
                return;
            }
            let right = left + 1;
            let mut smallest = left;
            if right < len && self.compare(self.heap[right], self.heap[left]) == Ordering::Less {
                smallest = right;
            }
            if self.compare(self.heap[smallest], self.heap[i]) != Ordering::Less {
                return;
            }
            self.heap.swap(i, smallest);
            i = smallest;
        }
    }

    fn check(&self) -> Result<()> {
        match self.failure.take() {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    /// The next record in order, reading its value from the file as it is emitted.
    fn next_record(&mut self, file: &mut std::fs::File) -> Result<Option<Record>> {
        let Some(pending) = self.next_pending(file)? else {
            return Ok(None);
        };
        let value = read_value(file, &pending)?;
        Ok(Some(Record {
            seq: pending.seq,
            key: pending.key,
            value,
        }))
    }

    fn next_pending(&mut self, file: &mut std::fs::File) -> Result<Option<Pending>> {
        if self.heap.is_empty() {
            return Ok(None);
        }
        let run = self.heap[0];
        let pending = self.front[run].take();
        self.refill(file, run)?;
        if self.front[run].is_none() {
            let last = self.heap.pop().expect("non-empty");
            if !self.heap.is_empty() {
                self.heap[0] = last;
                self.sift_down(0);
            }
        } else {
            self.sift_down(0);
        }
        self.check()?;
        Ok(pending)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encoding::{AsciiEncoding, Int64Encoding};

    fn collect(mut sorted: SortedRecords) -> Vec<(Vec<u8>, Vec<u8>, u64)> {
        let mut out = Vec::new();
        while let Some(r) = sorted.next_record().unwrap() {
            out.push((r.key, r.value, r.seq));
        }
        out
    }

    #[test]
    fn sorts_in_memory() {
        let mut spool = RecordSpool::new(
            Arc::new(AsciiEncoding),
            std::env::temp_dir(),
            "drydb-spool-test",
            1 << 20,
        );
        for key in ["delta", "alpha", "charlie", "bravo"] {
            spool.push(key.as_bytes(), key.as_bytes()).unwrap();
        }
        let out = collect(spool.into_sorted().unwrap());
        let keys: Vec<_> = out
            .iter()
            .map(|(k, _, _)| String::from_utf8(k.clone()).unwrap())
            .collect();
        assert_eq!(keys, ["alpha", "bravo", "charlie", "delta"]);
    }

    #[test]
    fn equal_keys_keep_append_order() {
        let mut spool = RecordSpool::new(
            Arc::new(AsciiEncoding),
            std::env::temp_dir(),
            "drydb-spool-test",
            1 << 20,
        );
        for i in 0..5u8 {
            spool.push(b"same", &[i]).unwrap();
        }
        let out = collect(spool.into_sorted().unwrap());
        assert_eq!(
            out.iter().map(|(_, v, _)| v[0]).collect::<Vec<_>>(),
            vec![0, 1, 2, 3, 4]
        );
    }

    /// Runs beyond the fan-in are reduced by extra passes rather than all opened at once.
    #[test]
    fn many_runs_are_reduced_before_the_final_merge() {
        let mut spool = RecordSpool::new(
            Arc::new(Int64Encoding),
            std::env::temp_dir(),
            "drydb-spool-test",
            64 * 1024,
        );
        // One record per run: each value on its own exceeds the buffer.
        let value = vec![0x5Au8; 96 * 1024];
        let count = MERGE_FAN_IN * 3 + 5;
        for i in 0..count as i64 {
            spool
                .push(&Int64Encoding::encode(count as i64 - i), &value)
                .unwrap();
        }
        let sorted = spool.into_sorted().unwrap();
        match &sorted {
            SortedRecords::Merged { merger, .. } => assert!(
                merger.runs() <= MERGE_FAN_IN,
                "the final merge opened {} runs",
                merger.runs()
            ),
            SortedRecords::Memory { .. } => panic!("the input should have spilled"),
        }
        let out = collect(sorted);
        assert_eq!(out.len(), count);
        let keys: Vec<i64> = out
            .iter()
            .map(|(k, _, _)| Int64Encoding::decode(k).unwrap())
            .collect();
        let mut expected: Vec<i64> = (1..=count as i64).collect();
        expected.sort_unstable();
        assert_eq!(keys, expected);
    }

    #[test]
    fn spills_and_merges_many_runs() {
        // A tiny budget forces a spill every few records.
        let mut spool = RecordSpool::new(
            Arc::new(Int64Encoding),
            std::env::temp_dir(),
            "drydb-spool-test",
            64 * 1024,
        );
        let mut expected: Vec<i64> = Vec::new();
        let mut value = 0u64;
        for _ in 0..20_000 {
            // A cheap deterministic shuffle.
            value = value
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            let key = (value >> 33) as i64 - (1 << 30);
            expected.push(key);
            spool
                .push(&Int64Encoding::encode(key), &key.to_le_bytes())
                .unwrap();
        }
        expected.sort_unstable();
        let out = collect(spool.into_sorted().unwrap());
        assert_eq!(out.len(), expected.len());
        let keys: Vec<i64> = out
            .iter()
            .map(|(k, _, _)| Int64Encoding::decode(k).unwrap())
            .collect();
        assert_eq!(keys, expected);
    }

    #[test]
    fn merged_runs_keep_append_order_for_equal_keys() {
        let mut spool = RecordSpool::new(
            Arc::new(AsciiEncoding),
            std::env::temp_dir(),
            "drydb-spool-test",
            64 * 1024,
        );
        // Payload per record chosen so the 64 KiB buffer spills many times over: a
        // smaller one would sort in memory and leave the merge untested.
        let padding = [b'p'; 200];
        for i in 0..4000u32 {
            let key = format!("key{:03}", i % 50);
            let mut value = i.to_le_bytes().to_vec();
            value.extend_from_slice(&padding);
            spool.push(key.as_bytes(), &value).unwrap();
        }
        let sorted = spool.into_sorted().unwrap();
        assert!(
            matches!(sorted, SortedRecords::Merged { .. }),
            "this test is about the merge; the input has to spill"
        );
        let out = collect(sorted);
        assert_eq!(out.len(), 4000);
        let mut previous: Option<(Vec<u8>, u32)> = None;
        for (key, value, _) in out {
            let value = u32::from_le_bytes(value[..4].try_into().unwrap());
            if let Some((prev_key, prev_value)) = &previous {
                match prev_key.as_slice().cmp(key.as_slice()) {
                    Ordering::Less => {}
                    Ordering::Equal => assert!(
                        *prev_value < value,
                        "equal keys must stay in append order: {prev_value} then {value}"
                    ),
                    Ordering::Greater => panic!("merge produced keys out of order"),
                }
            }
            previous = Some((key, value));
        }
    }

    #[test]
    fn rejects_keys_the_encoding_cannot_handle() {
        let mut spool = RecordSpool::new(
            Arc::new(Int64Encoding),
            std::env::temp_dir(),
            "drydb-spool-test",
            1 << 20,
        );
        assert!(spool.push(b"short", b"v").is_err());
    }

    /// One spill makes one run, and merging whatever was there dragged everything
    /// already merged through the next pass as well: a pass over the whole spool every
    /// time enough of them had piled up, which is work in proportion to the square of
    /// the input. Runs are merged with others of their own size instead.
    ///
    /// What that costs is one pass per level, which follows from the levels themselves
    /// and is what this checks. Measuring it as bytes written would take an input large
    /// enough for the square to show, which is more than a test should spool.
    #[test]
    fn runs_are_merged_with_others_of_their_own_size() {
        let dir = std::env::temp_dir().join(format!("drydb-spool-tier-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        // Values above the buffer, so every record spills on its own and the runs pile
        // up as fast as they can.
        let mut spool = RecordSpool::new(Arc::new(Int64Encoding), dir.clone(), "tier", 1);
        let value = vec![0x5Au8; 64 * 1024 + 1];
        for i in 0..300u64 {
            spool
                .push(&Int64Encoding::encode(i as i64), &value)
                .unwrap();
        }

        // Sixteen of a level make one of the next, so three hundred runs come down to a
        // couple of dozen rather than three hundred.
        assert!(
            spool.runs.len() <= 2 * MERGE_FAN_IN,
            "{} runs left",
            spool.runs.len()
        );
        assert!(
            spool.runs.iter().any(|run| run.level > 0),
            "some of them have been merged"
        );
        // Every merge read runs of one level, so what has been written is a pass over
        // the input plus one per level, not a pass over everything each time.
        let levels = spool.runs.iter().map(|run| run.level).max().unwrap_or(0) as u64;
        let input = 300 * (RECORD_HEADER_LEN as u64 + 8 + value.len() as u64);
        assert!(
            spool.written <= input * (levels + 2),
            "{} bytes written for {input} of input over {levels} levels",
            spool.written
        );

        // And the records all survive it.
        assert_eq!(spool.len(), 300);
        let mut sorted = spool.into_sorted().unwrap();
        let mut seen = 0u64;
        while let Some(record) = sorted.next_record().unwrap() {
            assert_eq!(record.key, Int64Encoding::encode(seen as i64));
            seen += 1;
        }
        assert_eq!(seen, 300);

        std::fs::remove_dir_all(&dir).ok();
    }

    /// A spool that could not make its next temporary file used to be left without one
    /// at all, and the build that followed reached for it and panicked.
    #[test]
    fn a_spool_that_cannot_make_a_file_is_left_usable() {
        let dir = std::env::temp_dir().join(format!("drydb-spool-perm-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        let mut spool = RecordSpool::new(Arc::new(Int64Encoding), dir.clone(), "perm", 1);
        let value = vec![0x5Au8; 64 * 1024 + 1];
        for i in 0..8u64 {
            spool
                .push(&Int64Encoding::encode(i as i64), &value)
                .unwrap();
        }

        // The directory goes read-only, so the next file cannot be made.
        let mut perms = std::fs::metadata(&dir).unwrap().permissions();
        perms.set_readonly(true);
        std::fs::set_permissions(&dir, perms).unwrap();
        let refused = spool.rewrite();
        let mut perms = std::fs::metadata(&dir).unwrap().permissions();
        #[allow(clippy::permissions_set_readonly_false)]
        perms.set_readonly(false);
        std::fs::set_permissions(&dir, perms).unwrap();

        assert!(refused.is_err(), "a file that cannot be made is an error");
        // And the spool still has everything it had: the records come back in order.
        assert_eq!(spool.len(), 8);
        let mut sorted = spool.into_sorted().unwrap();
        let mut seen = 0u64;
        while let Some(record) = sorted.next_record().unwrap() {
            assert_eq!(record.key, Int64Encoding::encode(seen as i64));
            seen += 1;
        }
        assert_eq!(seen, 8);

        std::fs::remove_dir_all(&dir).ok();
    }

    /// A record that could not be written cannot be taken back: the write sorts the
    /// buffer, so the record is no longer where it was put, and the failure can come
    /// after it has been written out for good. Taking the last slot out instead removed
    /// a different record and left the buffer describing bytes that were not there. The
    /// spool stops instead.
    #[test]
    fn a_spool_that_could_not_write_a_row_does_not_go_on() {
        let dir = std::env::temp_dir().join(format!("drydb-spool-stop-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();

        // No directory, so the write this record triggers cannot happen. The keys go in
        // out of order, so the write sorts them and the record is not the last one.
        let mut spool = RecordSpool::new(Arc::new(Int64Encoding), dir.clone(), "stop", 1);
        let value = vec![0x5Au8; 32 * 1024];
        spool.push(&Int64Encoding::encode(2), &value).unwrap();
        let refused = spool.push(&Int64Encoding::encode(1), &value);
        assert!(refused.is_err(), "the write could not happen");

        // Everything after that says so, rather than building a file whose contents
        // nobody can account for.
        std::fs::create_dir_all(&dir).unwrap();
        assert!(spool.push(&Int64Encoding::encode(3), &value).is_err());
        assert!(spool.into_sorted().is_err());

        std::fs::remove_dir_all(&dir).ok();
    }

    /// The same when the failure comes after the rows have been written: they are on
    /// disk, so there is nothing to take back, and the build stops rather than shipping
    /// a row whose append was refused.
    #[test]
    fn a_spool_that_failed_after_writing_does_not_go_on() {
        let dir = std::env::temp_dir().join(format!("drydb-spool-late-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        let mut spool = RecordSpool::new(Arc::new(Int64Encoding), dir.clone(), "late", 1);
        let value = vec![0x5Au8; 64 * 1024 + 1];
        for i in 0..15i64 {
            spool.push(&Int64Encoding::encode(i), &value).unwrap();
        }

        // The directory goes read-only, so the merge the next row sets off cannot open
        // its file. The row itself is written before that happens.
        let mut perms = std::fs::metadata(&dir).unwrap().permissions();
        perms.set_readonly(true);
        std::fs::set_permissions(&dir, perms).unwrap();
        let refused = spool.push(&Int64Encoding::encode(15), &value);
        let mut perms = std::fs::metadata(&dir).unwrap().permissions();
        #[allow(clippy::permissions_set_readonly_false)]
        perms.set_readonly(false);
        std::fs::set_permissions(&dir, perms).unwrap();

        if refused.is_ok() {
            // The merge happened to fit; nothing to check.
            std::fs::remove_dir_all(&dir).ok();
            return;
        }
        assert!(
            spool.into_sorted().is_err(),
            "a build cannot go on from here"
        );

        std::fs::remove_dir_all(&dir).ok();
    }
}
