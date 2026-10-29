//! Bounded, seekable gzip storage for a committed log generation.
//!
//! Gzip members concatenate into an ordinary gzip file. The sidecar maps raw
//! byte offsets to members, allowing existing record indexes to seek without
//! expanding the whole archive. Publication never removes the raw source.

use flate2::{Compression, read::GzDecoder, write::GzEncoder};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
};

const BLOCK_BYTES: usize = 4 * 1024 * 1024;
const VERSION: u32 = 1;
static NEXT_STAGE: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Serialize, Deserialize)]
struct Block {
    raw_start: u64,
    raw_len: u64,
    compressed_start: u64,
    compressed_len: u64,
}

#[derive(Debug, Serialize, Deserialize)]
struct Manifest {
    version: u32,
    raw_bytes: u64,
    compressed_bytes: u64,
    sha256: String,
    blocks: Vec<Block>,
}

fn sidecar(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(".index.json");
    name.into()
}

pub(super) fn remove(path: &Path) {
    let _ = fs::remove_file(path);
    let _ = fs::remove_file(sidecar(path));
}

struct Temporary(PathBuf);
impl Drop for Temporary {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

fn temporary(path: &Path) -> io::Result<(Temporary, File)> {
    loop {
        let mut name = path.as_os_str().to_os_string();
        name.push(format!(
            ".{}.{}.staging",
            std::process::id(),
            NEXT_STAGE.fetch_add(1, Ordering::Relaxed)
        ));
        let path = PathBuf::from(name);
        match OpenOptions::new()
            .write(true)
            .read(true)
            .create_new(true)
            .open(&path)
        {
            Ok(file) => return Ok((Temporary(path), file)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
}

fn check_cancelled(cancelled: &AtomicBool) -> io::Result<()> {
    if cancelled.load(Ordering::Relaxed) {
        Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "Archive cancelled",
        ))
    } else {
        Ok(())
    }
}

/// Build and verify an archive beside its final paths. The caller retains the
/// raw file and decides separately whether this generation is safe to archive.
pub(super) fn build(
    raw: &Path,
    destination: &Path,
    bytes: u64,
    cancelled: &AtomicBool,
) -> io::Result<()> {
    let (stage, mut output) = temporary(destination)?;
    let mut input = File::open(raw)?.take(bytes);
    let mut buffer = vec![0u8; BLOCK_BYTES];
    let mut remaining = bytes;
    let mut raw_start = 0u64;
    let mut blocks = Vec::new();
    let mut digest = Sha256::new();
    // An empty file still receives a valid gzip member.
    loop {
        check_cancelled(cancelled)?;
        let length = remaining.min(BLOCK_BYTES as u64) as usize;
        input.read_exact(&mut buffer[..length])?;
        digest.update(&buffer[..length]);
        let compressed_start = output.stream_position()?;
        let mut encoder = GzEncoder::new(&mut output, Compression::default());
        encoder.write_all(&buffer[..length])?;
        encoder.finish()?;
        let compressed_end = output.stream_position()?;
        blocks.push(Block {
            raw_start,
            raw_len: length as u64,
            compressed_start,
            compressed_len: compressed_end - compressed_start,
        });
        raw_start += length as u64;
        remaining -= length as u64;
        if remaining == 0 {
            break;
        }
    }
    output.sync_all()?;
    let manifest = Manifest {
        version: VERSION,
        raw_bytes: bytes,
        compressed_bytes: output.metadata()?.len(),
        sha256: format!("{:x}", digest.finalize()),
        blocks,
    };
    drop(output);
    let mut reader = ArchiveReader::open_with_manifest(&stage.0, manifest)?;
    let mut verify = Sha256::new();
    loop {
        check_cancelled(cancelled)?;
        let count = reader.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        verify.update(&buffer[..count]);
    }
    if format!("{:x}", verify.finalize()) != reader.manifest.sha256 {
        return Err(io::Error::other("Archive verification failed"));
    }
    let metadata_path = sidecar(destination);
    let (metadata_stage, mut metadata) = temporary(&metadata_path)?;
    serde_json::to_writer(&mut metadata, &reader.manifest).map_err(io::Error::other)?;
    metadata.sync_all()?;
    drop(metadata);
    check_cancelled(cancelled)?;
    // Links fail if a destination already exists, preventing a concurrent
    // archive from replacing an active generation or its index.
    fs::hard_link(&stage.0, destination)?;
    if let Err(error) = fs::hard_link(&metadata_stage.0, &metadata_path) {
        let _ = fs::remove_file(destination);
        return Err(error);
    }
    Ok(())
}

pub(super) struct ArchiveReader {
    file: File,
    manifest: Manifest,
    position: u64,
    loaded: Option<usize>,
    block: Vec<u8>,
}

impl ArchiveReader {
    pub(super) fn open(path: &Path) -> io::Result<Self> {
        let manifest =
            serde_json::from_reader(File::open(sidecar(path))?).map_err(io::Error::other)?;
        Self::open_with_manifest(path, manifest)
    }

    pub(super) fn raw_len(&self) -> u64 {
        self.manifest.raw_bytes
    }

    fn open_with_manifest(path: &Path, manifest: Manifest) -> io::Result<Self> {
        let file = File::open(path)?;
        if manifest.version != VERSION || file.metadata()?.len() != manifest.compressed_bytes {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Invalid archive metadata",
            ));
        }
        let mut raw = 0u64;
        let mut compressed = 0u64;
        for block in &manifest.blocks {
            if block.raw_start != raw
                || block.compressed_start != compressed
                || block.raw_len > BLOCK_BYTES as u64
                || block.compressed_len == 0
                || (block.raw_len == 0 && manifest.raw_bytes != 0)
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Invalid archive block",
                ));
            }
            raw = raw
                .checked_add(block.raw_len)
                .ok_or_else(|| io::Error::other("Archive offset overflow"))?;
            compressed = compressed
                .checked_add(block.compressed_len)
                .ok_or_else(|| io::Error::other("Archive offset overflow"))?;
        }
        if manifest.blocks.is_empty()
            || raw != manifest.raw_bytes
            || compressed != manifest.compressed_bytes
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Incomplete archive index",
            ));
        }
        Ok(Self {
            file,
            manifest,
            position: 0,
            loaded: None,
            block: Vec::new(),
        })
    }

    fn load(&mut self, index: usize) -> io::Result<()> {
        if self.loaded == Some(index) {
            return Ok(());
        }
        let block = &self.manifest.blocks[index];
        self.file.seek(SeekFrom::Start(block.compressed_start))?;
        let mut decoder = GzDecoder::new((&mut self.file).take(block.compressed_len));
        self.loaded = None;
        self.block.clear();
        decoder
            .by_ref()
            .take(BLOCK_BYTES as u64 + 1)
            .read_to_end(&mut self.block)?;
        if self.block.len() as u64 != block.raw_len {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Corrupt archive member",
            ));
        }
        self.loaded = Some(index);
        Ok(())
    }
}

impl Read for ArchiveReader {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        if output.is_empty() || self.position >= self.manifest.raw_bytes {
            return Ok(0);
        }
        let index = self
            .manifest
            .blocks
            .partition_point(|b| b.raw_start <= self.position)
            - 1;
        self.load(index)?;
        let start = (self.position - self.manifest.blocks[index].raw_start) as usize;
        let count = output.len().min(self.block.len() - start);
        output[..count].copy_from_slice(&self.block[start..start + count]);
        self.position += count as u64;
        Ok(count)
    }
}

impl Seek for ArchiveReader {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        let offset = match from {
            SeekFrom::Start(position) => i128::from(position),
            SeekFrom::Current(delta) => i128::from(self.position) + i128::from(delta),
            SeekFrom::End(delta) => i128::from(self.manifest.raw_bytes) + i128::from(delta),
        };
        self.position = u64::try_from(offset)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "Invalid archive seek"))?;
        Ok(self.position)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::read::MultiGzDecoder;

    fn directory() -> PathBuf {
        let id = NEXT_STAGE.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("log-archive-{}-{id}", std::process::id()));
        fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn gzip_archive_seeks_across_members_and_keeps_original_bytes() {
        let dir = directory();
        let source = dir.join("raw.log");
        let archive = dir.join("raw.log.gz");
        let raw: Vec<u8> = (0..BLOCK_BYTES * 2 + 93).map(|n| (n % 251) as u8).collect();
        fs::write(&source, &raw).unwrap();
        build(&source, &archive, raw.len() as u64, &AtomicBool::new(false)).unwrap();
        let mut full = Vec::new();
        MultiGzDecoder::new(File::open(&archive).unwrap())
            .read_to_end(&mut full)
            .unwrap();
        assert_eq!(full, raw);
        let mut reader = ArchiveReader::open(&archive).unwrap();
        reader
            .seek(SeekFrom::Start((BLOCK_BYTES - 23) as u64))
            .unwrap();
        let mut slice = vec![0; 100];
        reader.read_exact(&mut slice).unwrap();
        assert_eq!(&slice, &raw[BLOCK_BYTES - 23..BLOCK_BYTES + 77]);
        reader.seek(SeekFrom::End(-20)).unwrap();
        reader.read_exact(&mut slice[..20]).unwrap();
        assert_eq!(&slice[..20], &raw[raw.len() - 20..]);
        assert!(reader.seek(SeekFrom::Start(0)).is_ok());
        assert!(reader.seek(SeekFrom::Current(-1)).is_err());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn archive_publication_preserves_existing_files_and_detects_corruption() {
        let dir = directory();
        let source = dir.join("raw.log");
        let archive = dir.join("raw.log.gz");
        fs::write(&source, b"first\nsecond\n").unwrap();
        let stop = AtomicBool::new(false);
        build(&source, &archive, 13, &stop).unwrap();
        assert!(build(&source, &archive, 13, &stop).is_err());
        assert_eq!(fs::read(&source).unwrap(), b"first\nsecond\n");
        let mut bytes = fs::read(&archive).unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 1;
        fs::write(&archive, bytes).unwrap();
        let mut reader = ArchiveReader::open(&archive).unwrap();
        assert!(reader.read_to_end(&mut Vec::new()).is_err());
        let cancelled = dir.join("cancelled.gz");
        assert!(build(&source, &cancelled, 13, &AtomicBool::new(true)).is_err());
        assert!(!cancelled.exists());
        assert!(!sidecar(&cancelled).exists());
        let short = dir.join("short.gz");
        assert!(build(&source, &short, 14, &stop).is_err());
        assert!(!short.exists());
        assert!(!sidecar(&short).exists());
        let conflict = dir.join("conflict.gz");
        fs::write(sidecar(&conflict), b"existing index").unwrap();
        assert!(build(&source, &conflict, 13, &stop).is_err());
        assert!(!conflict.exists());
        assert_eq!(fs::read(sidecar(&conflict)).unwrap(), b"existing index");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn empty_archive_is_a_valid_gzip_file() {
        let dir = directory();
        let source = dir.join("empty.log");
        let archive = dir.join("empty.gz");
        fs::write(&source, []).unwrap();
        build(&source, &archive, 0, &AtomicBool::new(false)).unwrap();
        let mut raw = Vec::new();
        MultiGzDecoder::new(File::open(&archive).unwrap())
            .read_to_end(&mut raw)
            .unwrap();
        assert!(raw.is_empty());
        assert_eq!(
            ArchiveReader::open(&archive)
                .unwrap()
                .read(&mut [0; 1])
                .unwrap(),
            0
        );
        fs::remove_dir_all(dir).unwrap();
    }
}
