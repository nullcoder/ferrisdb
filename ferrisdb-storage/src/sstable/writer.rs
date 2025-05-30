//! SSTable writer implementation

use crate::sstable::{
    Footer, IndexEntry, InternalKey, SSTableEntry, DEFAULT_BLOCK_SIZE, MAX_ENTRY_SIZE,
};
use ferrisdb_core::{Error, Operation, Result, Value};
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

/// Metadata about a written SSTable file
#[derive(Debug, Clone)]
pub struct SSTableInfo {
    /// Path to the SSTable file
    pub path: PathBuf,
    /// Total file size in bytes
    pub file_size: u64,
    /// Number of entries in the file
    pub entry_count: usize,
    /// Smallest key in the file
    pub smallest_key: InternalKey,
    /// Largest key in the file
    pub largest_key: InternalKey,
}

/// Writer for creating SSTable files
///
/// The SSTableWriter creates immutable SSTable files from sorted key-value
/// pairs, typically from a flushed MemTable. It handles block creation,
/// compression, and index generation.
///
/// # Example
///
/// ```ignore
/// use ferrisdb_storage::sstable::{writer::SSTableWriter, InternalKey};
/// use ferrisdb_core::Operation;
///
/// let mut writer = SSTableWriter::new("path/to/sstable.sst")?;
///
/// let key = InternalKey::new(b"key1".to_vec(), 100);
/// writer.add(key, b"value1".to_vec(), Operation::Put)?;
///
/// let info = writer.finish()?;
/// println!("Created SSTable with {} entries", info.entry_count);
/// ```
pub struct SSTableWriter {
    /// Buffered writer for the file
    writer: BufWriter<File>,
    /// Path to the file being written
    path: PathBuf,
    /// Current position in the file
    file_offset: u64,
    /// Buffer for the current data block
    current_block: Vec<SSTableEntry>,
    /// Current block size in bytes
    current_block_size: usize,
    /// Maximum block size
    block_size: usize,
    /// Index entries for all written blocks
    index_entries: Vec<IndexEntry>,
    /// Total number of entries written
    entry_count: usize,
    /// Smallest key seen (for metadata)
    smallest_key: Option<InternalKey>,
    /// Largest key seen (for metadata)
    largest_key: Option<InternalKey>,
    /// Last key written (for ordering verification)
    last_key: Option<InternalKey>,
    /// Whether finish() has been called
    finished: bool,
}

impl SSTableWriter {
    /// Creates a new SSTable writer
    ///
    /// # Arguments
    ///
    /// * `path` - Path where the SSTable file will be created
    ///
    /// # Errors
    ///
    /// Returns an error if the file cannot be created
    pub fn new(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let file = File::create(&path)?;
        let writer = BufWriter::new(file);

        Ok(Self {
            writer,
            path,
            file_offset: 0,
            current_block: Vec::new(),
            current_block_size: 0,
            block_size: DEFAULT_BLOCK_SIZE,
            index_entries: Vec::new(),
            entry_count: 0,
            smallest_key: None,
            largest_key: None,
            last_key: None,
            finished: false,
        })
    }

    /// Creates a new SSTable writer with a custom block size
    ///
    /// # Arguments
    ///
    /// * `path` - Path where the SSTable file will be created
    /// * `block_size` - Target size for data blocks in bytes
    pub fn with_block_size(path: impl AsRef<Path>, block_size: usize) -> Result<Self> {
        let mut writer = Self::new(path)?;
        writer.block_size = block_size;
        Ok(writer)
    }

    /// Adds a key-value pair with operation to the SSTable
    ///
    /// Keys must be added in sorted order according to InternalKey ordering
    /// (user_key ascending, then timestamp descending). The writer verifies
    /// ordering to prevent creating invalid SSTables.
    ///
    /// # Arguments
    ///
    /// * `key` - The internal key (user_key + timestamp)
    /// * `value` - The value to associate with the key
    /// * `operation` - The operation type (Put/Delete)
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The writer has already been finished
    /// - The key or value exceeds maximum size limits
    /// - Keys are not in sorted order
    /// - An I/O error occurs
    pub fn add(&mut self, key: InternalKey, value: Value, operation: Operation) -> Result<()> {
        if self.finished {
            return Err(Error::ResourceConsumed(
                "SSTable writer already finished".to_string(),
            ));
        }

        // Validate sizes
        let key_size = key.user_key.len();
        let value_size = value.len();
        if key_size > MAX_ENTRY_SIZE {
            return Err(Error::EntrySizeExceeded {
                size: key_size,
                max_size: MAX_ENTRY_SIZE,
            });
        }
        if value_size > MAX_ENTRY_SIZE {
            return Err(Error::EntrySizeExceeded {
                size: value_size,
                max_size: MAX_ENTRY_SIZE,
            });
        }

        // Verify ordering
        if let Some(ref last) = self.last_key {
            if key <= *last {
                return Err(Error::KeyOrderingViolation {
                    last_key: last.to_string(),
                    new_key: key.to_string(),
                });
            }
        }

        // Create entry with the provided operation
        let entry = SSTableEntry::new(key.clone(), value, operation);
        let entry_size = entry.serialized_size();

        // Update metadata (clone where we need the key again)
        if self.smallest_key.is_none() {
            self.smallest_key = Some(key.clone());
        }
        self.largest_key = Some(key.clone());

        // Check if we need to flush the current block
        if !self.current_block.is_empty() && self.current_block_size + entry_size > self.block_size
        {
            self.flush_block()?;
        }

        // Add to current block
        self.current_block.push(entry);
        self.current_block_size += entry_size;
        self.entry_count += 1;

        // Update last_key last to take ownership (no clone needed)
        self.last_key = Some(key);

        Ok(())
    }

    /// Finishes writing the SSTable and returns metadata
    ///
    /// This method:
    /// 1. Flushes any remaining data block
    /// 2. Writes the index block
    /// 3. Writes the bloom filter (placeholder for now)
    /// 4. Writes the footer
    /// 5. Syncs the file to disk
    ///
    /// After calling finish(), the writer cannot be used again.
    pub fn finish(mut self) -> Result<SSTableInfo> {
        if self.finished {
            return Err(Error::ResourceConsumed(
                "SSTable writer already finished".to_string(),
            ));
        }

        // Flush any remaining block
        if !self.current_block.is_empty() {
            self.flush_block()?;
        }

        // Write index block
        let index_offset = self.file_offset;
        let index_length = self.write_index_block()?;

        // Write bloom filter (placeholder for now)
        let bloom_offset = self.file_offset;
        let bloom_length = self.write_bloom_filter()?;

        // Write footer
        let footer = Footer::new(index_offset, index_length, bloom_offset, bloom_length);
        self.writer.write_all(&footer.to_bytes())?;
        self.file_offset += footer.to_bytes().len() as u64;

        // Sync to disk
        self.writer.flush()?;
        let file = self
            .writer
            .into_inner()
            .map_err(|e| Error::Io(e.into_parts().0))?;
        file.sync_all()?;

        self.finished = true;

        Ok(SSTableInfo {
            path: self.path,
            file_size: self.file_offset,
            entry_count: self.entry_count,
            smallest_key: self.smallest_key.ok_or_else(|| {
                Error::EmptyOperation("Cannot finish SSTable with no entries".to_string())
            })?,
            largest_key: self.largest_key.ok_or_else(|| {
                Error::EmptyOperation("Cannot finish SSTable with no entries".to_string())
            })?,
        })
    }

    /// Flushes the current block to disk
    fn flush_block(&mut self) -> Result<()> {
        if self.current_block.is_empty() {
            return Ok(());
        }

        // Remember the first key for the index
        let first_key = self.current_block[0].key.user_key.clone();
        let block_offset = self.file_offset;

        // Write block header (entry count - u32 supports up to 4B entries per block)
        let entry_count = self.current_block.len() as u32;
        self.writer.write_all(&entry_count.to_le_bytes())?;
        self.file_offset += 4;

        // Write entries
        for entry in &self.current_block {
            Self::write_entry(&mut self.writer, &mut self.file_offset, entry)?;
        }

        // Calculate and write checksum (placeholder - just use 0 for now)
        let checksum: u32 = 0; // TODO: Implement actual CRC32
        self.writer.write_all(&checksum.to_le_bytes())?;
        self.file_offset += 4;

        // Add index entry
        self.index_entries
            .push(IndexEntry::new(block_offset, first_key));

        // Clear current block
        self.current_block.clear();
        self.current_block_size = 0;

        Ok(())
    }

    /// Writes a single entry to the current block
    fn write_entry(
        writer: &mut BufWriter<File>,
        file_offset: &mut u64,
        entry: &SSTableEntry,
    ) -> Result<()> {
        // Write key length (safe cast: MAX_ENTRY_SIZE is 16MB, well within u32)
        let key_len = entry.key.user_key.len() as u32;
        writer.write_all(&key_len.to_le_bytes())?;
        *file_offset += 4;

        // Write value length (safe cast: MAX_ENTRY_SIZE is 16MB, well within u32)
        let value_len = entry.value.len() as u32;
        writer.write_all(&value_len.to_le_bytes())?;
        *file_offset += 4;

        // Write timestamp
        writer.write_all(&entry.key.timestamp.to_le_bytes())?;
        *file_offset += 8;

        // Write operation
        let op_byte = match entry.operation {
            Operation::Put => 0u8,
            Operation::Delete => 1u8,
        };
        writer.write_all(&[op_byte])?;
        *file_offset += 1;

        // Write key
        writer.write_all(&entry.key.user_key)?;
        *file_offset += entry.key.user_key.len() as u64;

        // Write value
        writer.write_all(&entry.value)?;
        *file_offset += entry.value.len() as u64;

        Ok(())
    }

    /// Writes the index block and returns its length
    fn write_index_block(&mut self) -> Result<u64> {
        let start_offset = self.file_offset;

        // Write entry count
        let entry_count = self.index_entries.len() as u32;
        self.writer.write_all(&entry_count.to_le_bytes())?;
        self.file_offset += 4;

        // Write each index entry
        for entry in &self.index_entries {
            // Write block offset
            self.writer.write_all(&entry.block_offset.to_le_bytes())?;
            self.file_offset += 8;

            // Write key length
            let key_len = entry.first_key.len() as u32;
            self.writer.write_all(&key_len.to_le_bytes())?;
            self.file_offset += 4;

            // Write key
            self.writer.write_all(&entry.first_key)?;
            self.file_offset += entry.first_key.len() as u64;
        }

        // Write checksum (placeholder)
        let checksum: u32 = 0; // TODO: Implement actual CRC32
        self.writer.write_all(&checksum.to_le_bytes())?;
        self.file_offset += 4;

        Ok(self.file_offset - start_offset)
    }

    /// Writes a placeholder bloom filter and returns its length
    fn write_bloom_filter(&mut self) -> Result<u64> {
        let start_offset = self.file_offset;

        // For now, just write a minimal bloom filter structure
        // TODO: Implement actual bloom filter

        // Write empty bit array (just 8 bytes of zeros)
        self.writer.write_all(&[0u8; 8])?;
        self.file_offset += 8;

        // Write hash count (0 for placeholder)
        self.writer.write_all(&0u32.to_le_bytes())?;
        self.file_offset += 4;

        // Write checksum (placeholder)
        let checksum: u32 = 0;
        self.writer.write_all(&checksum.to_le_bytes())?;
        self.file_offset += 4;

        Ok(self.file_offset - start_offset)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferrisdb_core::Operation;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn test_sstable_writer_basic() {
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("test.sst");

        let mut writer = SSTableWriter::new(&path).unwrap();

        // Add some entries
        let key1 = InternalKey::new(b"key1".to_vec(), 100);
        writer
            .add(key1.clone(), b"value1".to_vec(), Operation::Put)
            .unwrap();

        let key2 = InternalKey::new(b"key2".to_vec(), 200);
        writer
            .add(key2.clone(), b"value2".to_vec(), Operation::Put)
            .unwrap();

        let key3 = InternalKey::new(b"key3".to_vec(), 300);
        writer
            .add(key3.clone(), Vec::new(), Operation::Delete)
            .unwrap();

        // Finish writing
        let info = writer.finish().unwrap();

        assert_eq!(info.entry_count, 3);
        assert_eq!(info.smallest_key, key1);
        assert_eq!(info.largest_key, key3);
        assert!(info.file_size > 0);
        assert!(path.exists());

        // Verify file size
        let metadata = fs::metadata(&path).unwrap();
        assert_eq!(metadata.len(), info.file_size);
    }

    #[test]
    fn test_sstable_writer_empty_error() {
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("empty.sst");

        let writer = SSTableWriter::new(&path).unwrap();

        // Try to finish without adding any entries
        let result = writer.finish();
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), Error::EmptyOperation(_)));
    }

    #[test]
    fn test_sstable_writer_multiple_blocks() {
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("multi_block.sst");

        // Create writer with small block size to force multiple blocks
        let mut writer = SSTableWriter::with_block_size(&path, 128).unwrap();

        // Add entries that will span multiple blocks
        for i in 0..20 {
            let key = InternalKey::new(format!("key_{:04}", i).into_bytes(), i as u64);
            let value = format!("value_{}", i).into_bytes();
            writer.add(key, value, Operation::Put).unwrap();
        }

        let info = writer.finish().unwrap();
        assert_eq!(info.entry_count, 20);
        assert!(info.file_size > 128); // Should be larger than one block
    }

    #[test]
    fn test_sstable_writer_large_entries() {
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("large.sst");

        let mut writer = SSTableWriter::new(&path).unwrap();

        // Add large value
        let key = InternalKey::new(b"large_key".to_vec(), 100);
        let large_value = vec![b'x'; 10000]; // 10KB value
        writer.add(key, large_value, Operation::Put).unwrap();

        let info = writer.finish().unwrap();
        assert_eq!(info.entry_count, 1);
        assert!(info.file_size > 10000); // Should be larger than the value
    }

    #[test]
    fn test_sstable_writer_finish_twice_error() {
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("test.sst");

        let mut writer = SSTableWriter::new(&path).unwrap();

        let key = InternalKey::new(b"key".to_vec(), 100);
        writer.add(key, b"value".to_vec(), Operation::Put).unwrap();

        // First finish should succeed
        let _info = writer.finish().unwrap();

        // Can't use writer after finish (it's consumed), so this test
        // verifies the move semantics prevent reuse
    }

    #[test]
    fn test_sstable_writer_entry_too_large() {
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("too_large.sst");

        let mut writer = SSTableWriter::new(&path).unwrap();

        // Try to add entry that exceeds MAX_ENTRY_SIZE
        let huge_key = InternalKey::new(vec![b'k'; MAX_ENTRY_SIZE + 1], 100);
        let result = writer.add(huge_key, b"value".to_vec(), Operation::Put);

        assert!(result.is_err());
        match result.unwrap_err() {
            Error::EntrySizeExceeded { size, max_size } => {
                assert_eq!(size, MAX_ENTRY_SIZE + 1);
                assert_eq!(max_size, MAX_ENTRY_SIZE);
            }
            _ => panic!("Expected EntrySizeExceeded error"),
        }
    }

    #[test]
    fn test_sstable_writer_value_too_large() {
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("value_too_large.sst");

        let mut writer = SSTableWriter::new(&path).unwrap();

        // Try to add entry with value that exceeds MAX_ENTRY_SIZE
        let key = InternalKey::new(b"normal_key".to_vec(), 100);
        let huge_value = vec![b'v'; MAX_ENTRY_SIZE + 1];
        let result = writer.add(key, huge_value, Operation::Put);

        assert!(result.is_err());
        match result.unwrap_err() {
            Error::EntrySizeExceeded { size, max_size } => {
                assert_eq!(size, MAX_ENTRY_SIZE + 1);
                assert_eq!(max_size, MAX_ENTRY_SIZE);
            }
            _ => panic!("Expected EntrySizeExceeded error"),
        }
    }

    #[test]
    fn test_sstable_writer_mixed_operations() {
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("mixed_ops.sst");

        let mut writer = SSTableWriter::new(&path).unwrap();

        // Add mix of puts and deletes
        let put1 = InternalKey::new(b"key1".to_vec(), 100);
        writer
            .add(put1, b"value1".to_vec(), Operation::Put)
            .unwrap();

        let del1 = InternalKey::new(b"key2".to_vec(), 200);
        writer.add(del1, Vec::new(), Operation::Delete).unwrap();

        let put2 = InternalKey::new(b"key3".to_vec(), 300);
        writer
            .add(put2, b"value3".to_vec(), Operation::Put)
            .unwrap();

        let info = writer.finish().unwrap();
        assert_eq!(info.entry_count, 3);
    }

    #[test]
    fn test_sstable_writer_same_key_different_timestamps() {
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("mvcc.sst");

        let mut writer = SSTableWriter::new(&path).unwrap();

        // Add same key with different timestamps (MVCC)
        // Note: timestamps must be in descending order for the same key
        let key1 = InternalKey::new(b"key".to_vec(), 300);
        writer
            .add(key1.clone(), Vec::new(), Operation::Delete)
            .unwrap();

        let key2 = InternalKey::new(b"key".to_vec(), 200);
        writer
            .add(key2, b"value2".to_vec(), Operation::Put)
            .unwrap();

        let key3 = InternalKey::new(b"key".to_vec(), 100);
        writer
            .add(key3.clone(), b"value1".to_vec(), Operation::Put)
            .unwrap();

        let info = writer.finish().unwrap();
        assert_eq!(info.entry_count, 3);
        assert_eq!(info.smallest_key, key1);
        assert_eq!(info.largest_key, key3);
    }

    #[test]
    fn test_sstable_writer_ordering_error() {
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("bad_order.sst");

        let mut writer = SSTableWriter::new(&path).unwrap();

        // Add first key
        let key1 = InternalKey::new(b"key2".to_vec(), 100);
        writer
            .add(key1, b"value1".to_vec(), Operation::Put)
            .unwrap();

        // Try to add key that violates ordering (key1 < key2)
        let key2 = InternalKey::new(b"key1".to_vec(), 100);
        let result = writer.add(key2, b"value2".to_vec(), Operation::Put);

        assert!(result.is_err());
        match result.unwrap_err() {
            Error::KeyOrderingViolation { last_key, new_key } => {
                assert!(last_key.contains("key2@100"));
                assert!(new_key.contains("key1@100"));
            }
            _ => panic!("Expected KeyOrderingViolation error"),
        }

        // Also test same key with newer timestamp (should fail)
        let key3 = InternalKey::new(b"key2".to_vec(), 200);
        let result = writer.add(key3, b"value3".to_vec(), Operation::Put);

        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            Error::KeyOrderingViolation { .. }
        ));
    }

    #[test]
    fn test_sstable_writer_block_boundary() {
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("boundary.sst");

        // Use a specific block size
        let block_size = 256;
        let mut writer = SSTableWriter::with_block_size(&path, block_size).unwrap();

        // Add entries that exactly fill one block
        let mut total_size = 0;
        let mut count = 0;

        while total_size < block_size - 100 {
            // Leave some room
            let key = InternalKey::new(format!("key_{:04}", count).into_bytes(), count as u64);
            let value = b"val".to_vec();
            let entry = SSTableEntry::new(key.clone(), value.clone(), Operation::Put);

            writer.add(key, value, Operation::Put).unwrap();
            total_size += entry.serialized_size();
            count += 1;
        }

        // Add one more entry that should trigger a new block
        let key = InternalKey::new(b"trigger_new_block".to_vec(), 1000);
        writer
            .add(key, b"large_value_to_exceed_block".to_vec(), Operation::Put)
            .unwrap();

        let info = writer.finish().unwrap();
        assert_eq!(info.entry_count, count + 1);
        assert!(info.file_size > block_size as u64); // Should have multiple blocks
    }
}
