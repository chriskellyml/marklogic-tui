//! Shared reader for committed daily log partitions.
use super::archive;
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File},
    io::{self, Read, Seek, SeekFrom},
    path::{Path, PathBuf},
};
pub trait ReadSeek: Read + Seek + Send {}
impl<T: Read + Seek + Send> ReadSeek for T {}

pub fn local_path(root: &Path, name: &str) -> io::Result<PathBuf> {
    let path = Path::new(name);
    if path.file_name().and_then(|n| n.to_str()) != Some(name) || name == "." || name == ".." {
        return Err(io::Error::other("Invalid storage filename"));
    }
    let path = root.join(path);
    if fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_symlink()) {
        return Err(io::Error::other("Storage links are not supported"));
    }
    Ok(path)
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Part {
    pub day: String,
    pub path: String,
    pub bytes: u64,
    pub frozen: bool,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Manifest {
    pub parts: Vec<Part>,
}
impl Manifest {
    pub fn load(path: &Path) -> io::Result<Self> {
        let result: Self = serde_json::from_reader(File::open(path)?).map_err(io::Error::other)?;
        for part in &result.parts {
            local_path(path.parent().unwrap(), &part.path)?;
        }
        Ok(result)
    }
    pub fn bytes(&self) -> u64 {
        self.parts.iter().map(|p| p.bytes).sum()
    }
}

pub struct PartsReader {
    inputs: Vec<(u64, Box<dyn ReadSeek>)>,
    position: u64,
    bytes: u64,
}
impl PartsReader {
    pub fn open(path: &Path) -> io::Result<Self> {
        let manifest = Manifest::load(path)?;
        let mut inputs = Vec::new();
        let mut bytes = 0u64;
        // Open before publication can retire the previous generation. Existing
        // readers retain their handles when the files are unlinked (Unix).
        for part in manifest.parts {
            let path = local_path(path.parent().unwrap(), &part.path)?;
            let input: Box<dyn ReadSeek> = if part.frozen {
                let reader = archive::ArchiveReader::open(&path)?;
                if reader.raw_len() != part.bytes {
                    return Err(io::Error::other("Frozen part has the wrong length"));
                }
                Box::new(reader)
            } else {
                let file = File::open(path)?;
                if file.metadata()?.len() < part.bytes {
                    return Err(io::Error::other(
                        "Thawed part is shorter than its committed length",
                    ));
                }
                Box::new(file)
            };
            bytes = bytes
                .checked_add(part.bytes)
                .ok_or_else(|| io::Error::other("Storage length overflow"))?;
            inputs.push((bytes, input));
        }
        Ok(Self {
            inputs,
            position: 0,
            bytes,
        })
    }
    pub fn len(&self) -> u64 {
        self.bytes
    }
}
impl Read for PartsReader {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        if self.position >= self.bytes || output.is_empty() {
            return Ok(0);
        }
        let index = self
            .inputs
            .partition_point(|(end, _)| *end <= self.position);
        let start = if index == 0 {
            0
        } else {
            self.inputs[index - 1].0
        };
        let (end, reader) = &mut self.inputs[index];
        reader.seek(SeekFrom::Start(self.position - start))?;
        let count = output.len().min((*end - self.position) as usize);
        let count = reader.read(&mut output[..count])?;
        if count == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "Incomplete storage part",
            ));
        }
        self.position += count as u64;
        Ok(count)
    }
}
impl Seek for PartsReader {
    fn seek(&mut self, seek: SeekFrom) -> io::Result<u64> {
        let next = match seek {
            SeekFrom::Start(p) => p as i128,
            SeekFrom::End(p) => self.bytes as i128 + p as i128,
            SeekFrom::Current(p) => self.position as i128 + p as i128,
        };
        self.position = u64::try_from(next)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "Invalid seek"))?;
        Ok(self.position)
    }
}
