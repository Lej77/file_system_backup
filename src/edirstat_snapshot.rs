//! Convert to and from eDirStat snapshots.

use std::sync::Arc;

use chrono::DateTime;
use edirstat_core::{
    arena::{FileArenaSnapshot, NodeStorage, StringPool, precompute_dir_counts},
    snapshot::PersistentArena,
};

use crate::fs_index::{
    DEFAULT_PATH_SEPARATOR, FsEntryMetadata, FsIndex, FsIndexBuildOptions, PATH_SEPARATORS,
};

pub fn snapshot_from_arena(arena: PersistentArena, string_pool: StringPool) -> FileArenaSnapshot {
    FileArenaSnapshot {
        dir_counts: Arc::new(precompute_dir_counts(arena.nodes())),
        nodes: Arc::new(NodeStorage::Mmapped(arena)),
        string_pool: Arc::new(string_pool),
    }
}

pub fn edirstat_snapshot_to_fs_index(
    snapshot: FileArenaSnapshot,
    options: FsIndexBuildOptions<'_>,
) -> FsIndex {
    if snapshot.nodes.is_empty() {
        return FsIndex::new();
    }
    let mut cursors = Vec::new();
    cursors.push(0);

    let csv_iter = std::iter::from_fn(|| {
        let current_index = cursors.last_mut()?;
        let node_index = *current_index;
        let node = &snapshot.nodes[node_index as usize];

        let is_dir = node.is_directory() || node.first_child_opt().is_some();
        let metadata = FsEntryMetadata {
            size: node.size,
            allocated: node.size,
            modified: DateTime::from_timestamp(i64::from(node.modified_timestamp), 0)
                .expect("invalid or out-of-range modification timestamp")
                .naive_utc(),
            attributes: u64::from({
                let mut attr: u32 = 0;
                if is_dir {
                    attr |= 0x10; // FILE_ATTRIBUTE_DIRECTORY (16)
                } else {
                    attr |= 0x20; // FILE_ATTRIBUTE_ARCHIVE (32)
                }
                attr
            }),
            files: if is_dir {
                u64::from(node.file_count)
            } else {
                0
            },
            folders: if is_dir {
                u64::from(
                    snapshot
                        .dir_counts
                        .get(node_index as usize)
                        .copied()
                        .unwrap_or_default(),
                )
            } else {
                0
            },
            is_dir,
            children: None,
            drive_capacity: None,
            free_space: None,
            used_space: None,
            reserved_space: None,
        };

        let mut record = metadata.to_csv_record_without_filename();
        record.file_name = snapshot.get_full_path(node_index);
        let mut file_name = snapshot.string_pool.get(node.name_id).unwrap_or_default();
        let missing_file_name = file_name.trim().is_empty();
        if missing_file_name {
            file_name = "unknown";
            if !record.file_name.ends_with(PATH_SEPARATORS) {
                record.file_name.push(DEFAULT_PATH_SEPARATOR);
            }
            record.file_name.push_str(file_name);
        }
        if is_dir && !record.file_name.ends_with(PATH_SEPARATORS) {
            record.file_name.push(DEFAULT_PATH_SEPARATOR);
        }
        if node_index != 0 && file_name.contains(PATH_SEPARATORS) {
            panic!(
                "eDirStat snapshot contained file with the filename {file_name:?} which contains a path separator\n\
                \tFull path to problematic file: {}",
                record.file_name
            );
        }

        // Advance cursor:
        {
            // Advance in current directory:
            let mut sibling = node.next_sibling_opt();
            loop {
                if let Some(s_ix) = sibling {
                    let s_node = &snapshot.nodes[s_ix as usize];
                    if s_node.parent != node.parent {
                        // skip incorrect children
                        sibling = s_node.next_sibling_opt();
                    } else {
                        *current_index = s_ix;
                        break;
                    }
                } else {
                    cursors.pop();
                    break;
                }
            }

            // Advance into children:
            if let Some(child) = node.first_child_opt()
                && snapshot.nodes[child as usize].parent == node_index
            {
                cursors.push(child);
            }
        }

        Some(record)
    });

    FsIndex::from_csv_records(csv_iter, options)
}
