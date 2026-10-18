//! Disk usage of dust directories, with hardlink deduplication (design §9).

use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use dashmap::DashMap;
use rayon::prelude::*;
use walkdir::WalkDir;

/// A file with more than one link, and where its links were found.
struct SharedInode {
    nlink: u64,
    bytes: u64,
    /// `(directory index, links found inside it)`.
    links: Vec<(usize, u64)>,
}

pub struct Measurement {
    /// Per directory: allocated bytes, each inode counted once. Matches `du -s`.
    sizes: Vec<u64>,
    /// Per directory: bytes of files that have a single link.
    unshared: Vec<u64>,
    shared: Vec<SharedInode>,
    /// Entries that could not be read.
    pub unreadable: u64,
}

/// Measures each directory in parallel. `on_measured` is called with a
/// directory's size as soon as it is known.
pub fn measure(dirs: &[&Path], on_measured: &(dyn Fn(u64) + Sync)) -> Measurement {
    let shared: DashMap<(u64, u64), SharedInode> = DashMap::new();
    let unreadable = AtomicU64::new(0);

    let (sizes, unshared) = dirs
        .par_iter()
        .enumerate()
        .map(|(index, dir)| {
            let mut unshared = 0;
            let mut shared_here = 0;
            for entry in WalkDir::new(dir).same_file_system(true) {
                let Ok(meta) = entry.and_then(|e| e.metadata()) else {
                    unreadable.fetch_add(1, Ordering::Relaxed);
                    continue;
                };
                // Allocated size, not length: sparse and tiny files differ a lot.
                let bytes = meta.blocks() * 512;
                if meta.is_dir() || meta.nlink() <= 1 {
                    unshared += bytes;
                    continue;
                }
                let mut inode = shared
                    .entry((meta.dev(), meta.ino()))
                    .or_insert(SharedInode {
                        nlink: meta.nlink(),
                        bytes,
                        links: Vec::new(),
                    });
                match inode.links.iter_mut().find(|(at, _)| *at == index) {
                    Some((_, count)) => *count += 1,
                    None => {
                        inode.links.push((index, 1));
                        shared_here += bytes;
                    }
                }
            }
            on_measured(unshared + shared_here);
            (unshared + shared_here, unshared)
        })
        .unzip();

    Measurement {
        sizes,
        unshared,
        shared: shared.into_iter().map(|(_, inode)| inode).collect(),
        unreadable: unreadable.into_inner(),
    }
}

impl Measurement {
    pub fn size(&self, index: usize) -> u64 {
        self.sizes[index]
    }

    /// Allocated bytes of the chosen directories together, counting each
    /// inode once however many of them link to it.
    pub fn apparent(&self, chosen: &[bool]) -> u64 {
        self.total(chosen, |inode| {
            inode.links.iter().any(|(index, _)| chosen[*index])
        })
    }

    /// Bytes that deleting the chosen directories would free. A hardlinked
    /// file frees space only when every one of its links is deleted.
    pub fn would_free(&self, chosen: &[bool]) -> u64 {
        self.total(chosen, |inode| {
            let deleted: u64 = inode
                .links
                .iter()
                .filter(|(index, _)| chosen[*index])
                .map(|(_, count)| count)
                .sum();
            deleted == inode.nlink
        })
    }

    fn total(&self, chosen: &[bool], counts: impl Fn(&SharedInode) -> bool) -> u64 {
        let unshared: u64 = self
            .unshared
            .iter()
            .zip(chosen)
            .filter(|(_, chosen)| **chosen)
            .map(|(bytes, _)| bytes)
            .sum();
        let shared: u64 = self
            .shared
            .iter()
            .filter(|inode| counts(inode))
            .map(|inode| inode.bytes)
            .sum();
        unshared + shared
    }
}
