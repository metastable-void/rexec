use std::fs::{File, OpenOptions, read_dir};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::protocol::TranscriptEntry;

pub fn dir() -> std::io::Result<PathBuf> {
    let home = std::env::var_os("HOME").ok_or_else(|| std::io::Error::other("HOME not set"))?;
    let path = PathBuf::from(home).join(".rexec");
    std::fs::create_dir_all(&path)?;
    Ok(path)
}

pub fn path_for(name: &str) -> std::io::Result<PathBuf> {
    let mut p = dir()?;
    p.push(format!("{name}.jsonl"));
    Ok(p)
}

pub struct TranscriptWriter {
    file: Mutex<BufWriter<File>>,
}

impl TranscriptWriter {
    pub fn create(name: &str) -> std::io::Result<Self> {
        let path = path_for(name)?;
        Self::create_at(&path)
    }

    pub(crate) fn create_at(path: &Path) -> std::io::Result<Self> {
        let file = OpenOptions::new()
            .create_new(true)
            .append(true)
            .open(path)?;
        Ok(Self {
            file: Mutex::new(BufWriter::new(file)),
        })
    }

    pub fn append(&self, entry: &TranscriptEntry) -> std::io::Result<()> {
        let line = serde_json::to_string(entry)
            .map_err(|e| std::io::Error::other(format!("serialize transcript: {e}")))?;
        let mut g = self.file.lock().unwrap();
        g.write_all(line.as_bytes())?;
        g.write_all(b"\n")?;
        g.flush()?;
        Ok(())
    }
}

pub fn read_entries(path: &Path) -> std::io::Result<Vec<TranscriptEntry>> {
    let f = File::open(path)?;
    let reader = BufReader::new(f);
    let mut out = Vec::new();
    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<TranscriptEntry>(&line) {
            Ok(e) => out.push(e),
            Err(_) => continue,
        }
    }
    Ok(out)
}

pub struct TranscriptListing {
    pub name: String,
    pub command_count: usize,
}

pub fn list_recent(limit: usize) -> std::io::Result<Vec<TranscriptListing>> {
    let d = dir()?;
    let mut names: Vec<String> = Vec::new();
    for entry in read_dir(&d)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
            names.push(stem.to_string());
        }
    }
    names.sort();
    names.reverse();
    names.truncate(limit);

    let mut out = Vec::with_capacity(names.len());
    for name in names {
        let path = d.join(format!("{name}.jsonl"));
        let count = count_lines(&path).unwrap_or(0);
        out.push(TranscriptListing {
            name,
            command_count: count,
        });
    }
    Ok(out)
}

fn count_lines(path: &Path) -> std::io::Result<usize> {
    let f = File::open(path)?;
    let reader = BufReader::new(f);
    let mut count = 0;
    for line in reader.lines() {
        let line = line?;
        if !line.trim().is_empty() {
            count += 1;
        }
    }
    Ok(count)
}
