//! Convert to and from eDirStat snapshots.

use chrono::DateTime;
use edirstat::arena::FileArenaSnapshot;

use crate::fs_index::{
    DEFAULT_PATH_SEPARATOR, FsEntryMetadata, FsIndex, FsIndexBuildOptions, PATH_SEPARATORS,
};

pub fn from_edirstat_snapshot(
    snapshot: FileArenaSnapshot,
    options: FsIndexBuildOptions<'_>,
) -> FsIndex {
    if snapshot.nodes.is_empty() {
        return FsIndex::new();
    }
    let mut cursors = Vec::new();
    cursors.push(0);

    let csv_iter = std::iter::from_fn(|| {
        let node_index = cursors.last().copied()?;

        // Advance cursor:
        if let Some(current_index) = cursors.last_mut() {
            let current = &snapshot.nodes[*current_index as usize];

            // Advance in current directory:
            if let Some(sibling) = current.next_sibling_opt() {
                *current_index = sibling;
            } else {
                cursors.pop();
            }

            // Advance into children:
            if let Some(child) = current.first_child_opt() {
                cursors.push(child);
            }
        }

        // Convert metadata:
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
            folders: 0,
            is_dir,
            children: None,
            drive_capacity: None,
            free_space: None,
            used_space: None,
            reserved_space: None,
        };

        let mut record = metadata.to_csv_record_without_filename();
        record.file_name = snapshot.get_full_path(node_index);
        if is_dir && !record.file_name.ends_with(PATH_SEPARATORS) {
            record.file_name.push(DEFAULT_PATH_SEPARATOR);
        }
        Some(record)
    });

    FsIndex::from_csv_records(csv_iter, options)
}
