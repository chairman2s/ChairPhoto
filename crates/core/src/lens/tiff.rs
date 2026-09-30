//! Minimal bounded TIFF directory reading for RAW containers (ARW, the TIFF inside RAF).
//! Reads only the directories and tags it is asked for; every read is capped, so a
//! malformed file is a `None`, never a large allocation.
//!
//! Ported from RAWmakase (<https://github.com/pch/rawmakase>, `src/tiff.rs` at `80b6433`),
//! Copyright (c) 2026 RAWmakase contributors, MIT License — see `MODULE_LICENSING.md`.
//! Only the readers the lens tables need are kept.

use std::{
    collections::BTreeMap,
    fs::File,
    io::{Read, Seek, SeekFrom},
};

pub(crate) struct Entry {
    pub kind: u16,
    pub count: u32,
    pub value: [u8; 4],
}

/// Offsets are relative to `base` (the TIFF's start inside its container).
pub(crate) struct Tiff {
    f: File,
    base: u64,
    little: bool,
    pub first: u64,
}

impl Tiff {
    pub(crate) fn open(mut f: File, base: u64) -> Option<Self> {
        let mut h = [0u8; 8];
        f.seek(SeekFrom::Start(base)).ok()?;
        f.read_exact(&mut h).ok()?;
        let little = match &h[..4] {
            // Olympus ORF and Panasonic RW2 use their own magic numbers.
            b"II*\0" | b"IIRO" | b"IIRS" | b"IIU\0" => true,
            b"MM\0*" => false,
            _ => return None,
        };
        let mut t = Self { f, base, little, first: 0 };
        t.first = t.u32(&h[4..8]) as u64;
        Some(t)
    }

    fn u16(&self, b: &[u8]) -> u16 {
        let a = [b[0], b[1]];
        if self.little {
            u16::from_le_bytes(a)
        } else {
            u16::from_be_bytes(a)
        }
    }

    fn u32(&self, b: &[u8]) -> u32 {
        let a = [b[0], b[1], b[2], b[3]];
        if self.little {
            u32::from_le_bytes(a)
        } else {
            u32::from_be_bytes(a)
        }
    }

    fn bytes(&mut self, offset: u64, len: usize) -> Option<Vec<u8>> {
        if len > 1 << 20 {
            return None;
        }
        let mut b = vec![0; len];
        self.f.seek(SeekFrom::Start(self.base.checked_add(offset)?)).ok()?;
        self.f.read_exact(&mut b).ok()?;
        Some(b)
    }

    pub(crate) fn ifd(&mut self, offset: u64) -> Option<BTreeMap<u16, Entry>> {
        let count = self.bytes(offset, 2)?;
        let n = self.u16(&count) as usize;
        if n == 0 || n > 1000 {
            return None;
        }
        let b = self.bytes(offset + 2, n * 12)?;
        Some(
            b.chunks_exact(12)
                .map(|e| {
                    (
                        self.u16(&e[0..2]),
                        Entry {
                            kind: self.u16(&e[2..4]),
                            count: self.u32(&e[4..8]),
                            value: [e[8], e[9], e[10], e[11]],
                        },
                    )
                })
                .collect(),
        )
    }

    /// The first value of a LONG or IFD entry, used as a directory offset.
    pub(crate) fn offset(&self, e: &Entry) -> Option<u64> {
        matches!(e.kind, 4 | 13).then(|| self.u32(&e.value) as u64)
    }

    /// An entry's values as numbers: (signed) shorts, longs and rationals.
    pub(crate) fn numbers(&mut self, e: &Entry) -> Option<Vec<f32>> {
        let size = match e.kind {
            3 | 8 => 2,
            4 | 9 => 4,
            5 | 10 => 8,
            _ => return None,
        };
        let len = (e.count as usize).checked_mul(size)?;
        let b = if len <= 4 {
            e.value[..len].to_vec()
        } else {
            let at = self.u32(&e.value) as u64;
            self.bytes(at, len)?
        };
        let v: Vec<f32> = b
            .chunks_exact(size)
            .map(|c| match e.kind {
                3 => self.u16(c) as f32,
                8 => self.u16(c) as i16 as f32,
                4 => self.u32(c) as f32,
                9 => self.u32(c) as i32 as f32,
                5 => self.u32(&c[..4]) as f32 / self.u32(&c[4..]) as f32,
                _ => self.u32(&c[..4]) as i32 as f32 / self.u32(&c[4..]) as i32 as f32,
            })
            .collect();
        v.iter().all(|x| x.is_finite()).then_some(v)
    }
}
