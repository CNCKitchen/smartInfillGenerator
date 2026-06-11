// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Stefan Hermann (CNC Kitchen) <stefan@cnckitchen.com>

//! Minimal ZIP container support for 3MF: stored-entry writer (3MF readers
//! accept uncompressed archives) and a reader that handles stored + deflate.

use miniz_oxide::inflate::decompress_to_vec_with_limit;

const MAX_ENTRY: usize = 512 * 1024 * 1024;

pub struct ZipWriter {
    data: Vec<u8>,
    entries: Vec<(String, u32, u32, u32)>, // name, crc, size, local offset
}

impl Default for ZipWriter {
    fn default() -> Self {
        Self::new()
    }
}

impl ZipWriter {
    pub fn new() -> Self {
        Self { data: Vec::new(), entries: Vec::new() }
    }

    pub fn add(&mut self, name: &str, bytes: &[u8]) {
        let offset = self.data.len() as u32;
        let crc = crc32(bytes);
        let size = bytes.len() as u32;
        // Local file header.
        self.data.extend_from_slice(&[0x50, 0x4b, 0x03, 0x04]);
        self.data.extend_from_slice(&20u16.to_le_bytes()); // version needed
        self.data.extend_from_slice(&0u16.to_le_bytes()); // flags
        self.data.extend_from_slice(&0u16.to_le_bytes()); // method: stored
        self.data.extend_from_slice(&0u16.to_le_bytes()); // mod time
        self.data.extend_from_slice(&0x21u16.to_le_bytes()); // mod date (1980-01-01)
        self.data.extend_from_slice(&crc.to_le_bytes());
        self.data.extend_from_slice(&size.to_le_bytes()); // compressed
        self.data.extend_from_slice(&size.to_le_bytes()); // uncompressed
        self.data.extend_from_slice(&(name.len() as u16).to_le_bytes());
        self.data.extend_from_slice(&0u16.to_le_bytes()); // extra len
        self.data.extend_from_slice(name.as_bytes());
        self.data.extend_from_slice(bytes);
        self.entries.push((name.to_string(), crc, size, offset));
    }

    pub fn finish(mut self) -> Vec<u8> {
        let cd_start = self.data.len() as u32;
        for (name, crc, size, offset) in &self.entries {
            self.data.extend_from_slice(&[0x50, 0x4b, 0x01, 0x02]);
            self.data.extend_from_slice(&20u16.to_le_bytes()); // version made by
            self.data.extend_from_slice(&20u16.to_le_bytes()); // version needed
            self.data.extend_from_slice(&0u16.to_le_bytes()); // flags
            self.data.extend_from_slice(&0u16.to_le_bytes()); // method
            self.data.extend_from_slice(&0u16.to_le_bytes()); // time
            self.data.extend_from_slice(&0x21u16.to_le_bytes()); // date
            self.data.extend_from_slice(&crc.to_le_bytes());
            self.data.extend_from_slice(&size.to_le_bytes());
            self.data.extend_from_slice(&size.to_le_bytes());
            self.data.extend_from_slice(&(name.len() as u16).to_le_bytes());
            self.data.extend_from_slice(&0u16.to_le_bytes()); // extra
            self.data.extend_from_slice(&0u16.to_le_bytes()); // comment
            self.data.extend_from_slice(&0u16.to_le_bytes()); // disk
            self.data.extend_from_slice(&0u16.to_le_bytes()); // internal attrs
            self.data.extend_from_slice(&0u32.to_le_bytes()); // external attrs
            self.data.extend_from_slice(&offset.to_le_bytes());
            self.data.extend_from_slice(name.as_bytes());
        }
        let cd_size = self.data.len() as u32 - cd_start;
        let n = self.entries.len() as u16;
        self.data.extend_from_slice(&[0x50, 0x4b, 0x05, 0x06]);
        self.data.extend_from_slice(&0u16.to_le_bytes()); // disk
        self.data.extend_from_slice(&0u16.to_le_bytes()); // cd disk
        self.data.extend_from_slice(&n.to_le_bytes());
        self.data.extend_from_slice(&n.to_le_bytes());
        self.data.extend_from_slice(&cd_size.to_le_bytes());
        self.data.extend_from_slice(&cd_start.to_le_bytes());
        self.data.extend_from_slice(&0u16.to_le_bytes()); // comment len
        self.data
    }
}

#[derive(Debug)]
pub enum ZipError {
    NotAZip,
    Corrupt(&'static str),
    Unsupported(&'static str),
}

impl std::fmt::Display for ZipError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ZipError::NotAZip => write!(f, "not a zip archive"),
            ZipError::Corrupt(s) => write!(f, "corrupt zip: {s}"),
            ZipError::Unsupported(s) => write!(f, "unsupported zip feature: {s}"),
        }
    }
}

impl std::error::Error for ZipError {}

/// Read all entries (directories skipped). Handles stored and deflate.
pub fn read_zip(bytes: &[u8]) -> Result<Vec<(String, Vec<u8>)>, ZipError> {
    if bytes.len() < 22 {
        return Err(ZipError::NotAZip);
    }
    // Find EOCD.
    let scan_start = bytes.len().saturating_sub(22 + 65536);
    let mut eocd = None;
    let mut i = bytes.len() - 22;
    loop {
        if &bytes[i..i + 4] == b"PK\x05\x06" {
            eocd = Some(i);
            break;
        }
        if i == scan_start {
            break;
        }
        i -= 1;
    }
    let eocd = eocd.ok_or(ZipError::NotAZip)?;
    let count = u16::from_le_bytes([bytes[eocd + 10], bytes[eocd + 11]]) as usize;
    let cd_offset = u32::from_le_bytes([
        bytes[eocd + 16],
        bytes[eocd + 17],
        bytes[eocd + 18],
        bytes[eocd + 19],
    ]) as usize;

    let mut out = Vec::with_capacity(count);
    let mut p = cd_offset;
    for _ in 0..count {
        if p + 46 > bytes.len() || &bytes[p..p + 4] != b"PK\x01\x02" {
            return Err(ZipError::Corrupt("central directory"));
        }
        let method = u16::from_le_bytes([bytes[p + 10], bytes[p + 11]]);
        let csize = u32::from_le_bytes([bytes[p + 20], bytes[p + 21], bytes[p + 22], bytes[p + 23]])
            as usize;
        let usize_ = u32::from_le_bytes([bytes[p + 24], bytes[p + 25], bytes[p + 26], bytes[p + 27]])
            as usize;
        let name_len = u16::from_le_bytes([bytes[p + 28], bytes[p + 29]]) as usize;
        let extra_len = u16::from_le_bytes([bytes[p + 30], bytes[p + 31]]) as usize;
        let comment_len = u16::from_le_bytes([bytes[p + 32], bytes[p + 33]]) as usize;
        let local_off = u32::from_le_bytes([bytes[p + 42], bytes[p + 43], bytes[p + 44], bytes[p + 45]])
            as usize;
        let name = String::from_utf8_lossy(&bytes[p + 46..p + 46 + name_len]).into_owned();
        p += 46 + name_len + extra_len + comment_len;

        if name.ends_with('/') {
            continue; // directory
        }
        // Local header: re-read name/extra lengths (extra often differs).
        if local_off + 30 > bytes.len() || &bytes[local_off..local_off + 4] != b"PK\x03\x04" {
            return Err(ZipError::Corrupt("local header"));
        }
        let lname = u16::from_le_bytes([bytes[local_off + 26], bytes[local_off + 27]]) as usize;
        let lextra = u16::from_le_bytes([bytes[local_off + 28], bytes[local_off + 29]]) as usize;
        let data_start = local_off + 30 + lname + lextra;
        if data_start + csize > bytes.len() {
            return Err(ZipError::Corrupt("entry data"));
        }
        let raw = &bytes[data_start..data_start + csize];
        let data = match method {
            0 => raw.to_vec(),
            8 => decompress_to_vec_with_limit(raw, MAX_ENTRY.min(usize_.max(1024)))
                .map_err(|_| ZipError::Corrupt("deflate stream"))?,
            _ => return Err(ZipError::Unsupported("compression method")),
        };
        out.push((name, data));
    }
    Ok(out)
}

pub fn crc32(data: &[u8]) -> u32 {
    let mut table = [0u32; 256];
    for (i, t) in table.iter_mut().enumerate() {
        let mut c = i as u32;
        for _ in 0..8 {
            c = if c & 1 != 0 { 0xEDB88320 ^ (c >> 1) } else { c >> 1 };
        }
        *t = c;
    }
    let mut crc = 0xFFFFFFFFu32;
    for &b in data {
        crc = table[((crc ^ b as u32) & 0xFF) as usize] ^ (crc >> 8);
    }
    crc ^ 0xFFFFFFFF
}
