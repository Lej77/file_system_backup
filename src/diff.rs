use std::{
    fs::File,
    io::{self, Read, Write},
    path::{Path, PathBuf},
};

use clap::{Parser, ValueEnum};
use color_eyre::eyre::{Context as _, bail, eyre};
use flate2::{Compression, write::GzEncoder};

use crate::{
    BackupFileType, CancelSignal, CommonOpt, Result, RsyncableOpts, WizTreeCsvRecord,
    fs_index::{
        DEFAULT_PATH_SEPARATOR, FsCursor, FsEntry, FsEntryId, FsIndex, FsIndexBuildOptions,
    },
    utils::{Rsyncable, create_file},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, ValueEnum)]
pub enum DiffFilter {
    #[value(name = "new-or-larger")]
    NewOrLarger,
    #[value(name = "new-or-changed")]
    NewOrChanged,
}

#[derive(Debug, Parser, Clone)]
pub struct DiffOpts {
    #[clap(flatten)]
    pub common: CommonOpt,

    /// Don't compress the output file.
    #[clap(short, long, conflicts_with = "rsyncable", help_heading = "PROCESSING")]
    pub uncompressed: bool,
    #[clap(flatten)]
    pub rsyncable: RsyncableOpts,
    /// The compression level to use for the output file.
    ///
    /// The integer here is typically on a scale of 0-9 where 0 means "no
    /// compression" and 9 means "take as long as you'd like".
    ///
    /// Defaults to the best possible compression.
    #[clap(
        short,
        long,
        conflicts_with = "uncompressed",
        help_heading = "PROCESSING"
    )]
    pub compression: Option<u32>,

    /// How to compare file entries from the old and new backups.
    ///
    /// "new-or-larger": the size of files in the diff reflects the increase in
    /// disk usage, i.e. the sizes of modified files is set to the difference in
    /// their size from the old to the new backup.
    ///
    /// "new-or-changed": keep entries unmodified from the new backup file but
    /// remove entries that exactly match existing entries in the old backup
    /// file.
    #[clap(
        short,
        long,
        value_enum,
        default_value_t = DiffFilter::NewOrLarger,
        help_heading = "PROCESSING"
    )]
    pub filter: DiffFilter,

    /// Overwrite the output file if it already exists.
    #[clap(
        long,
        requires = "output",
        visible_alias = "ow",
        help_heading = "OUTPUT"
    )]
    pub overwrite: bool,
    /// Don't try to add a file extension to the output path.
    ///
    /// Normally a file extension is only added to the output path if it doesn't
    /// already specify a one so this is only useful if you want an output file
    /// without any file extension at all.
    #[clap(long, requires = "output", help_heading = "OUTPUT")]
    pub no_file_extension: bool,
    /// Where to write the created backup file.
    #[clap(
        short,
        long,
        help_heading = "OUTPUT",
        required_unless_present = "stdout",
        conflicts_with = "stdout"
    )]
    pub output: Option<PathBuf>,

    /// Write the backup output to stdout.
    #[clap(long, help_heading = "OUTPUT")]
    pub stdout: bool,

    /// The type of the "old" backup file. Normally this can be guessed from the
    /// file extension.
    #[clap(long, value_enum, default_value_t = BackupFileType::Auto, help_heading = "INPUT")]
    pub old_file_type: BackupFileType,

    /// The type of the "new" backup file. Normally this can be guessed from the
    /// file extension.
    #[clap(long, value_enum, default_value_t = BackupFileType::Auto, help_heading = "INPUT")]
    pub new_file_type: BackupFileType,

    /// The file path to the older "filesystem" backup.
    #[clap(long, help_heading = "INPUT")]
    pub old: PathBuf,

    /// The file path to the newer "filesystem" backup.
    #[clap(long, help_heading = "INPUT")]
    pub new: PathBuf,
    // TODO: support reading input from stdin (start with a `u32` indicating the length of the first file)
}
impl DiffOpts {
    fn load_backup_file(input: &Path, mut file_type: BackupFileType) -> Result<FsIndex> {
        if BackupFileType::Auto == file_type
            && let Some(ext_file_type) = BackupFileType::from_file_path_ext(input)
        {
            file_type = ext_file_type;
        }
        if let BackupFileType::Auto = file_type {
            bail!(
                "Failed to determine the type of the backup file, \
                please specify it manually via the `--file-type` option"
            )
        }

        let mut file = File::open(input)
            .wrap_err_with(|| format!(r#"failed to open input file at: "{}""#, input.display()))?;

        let fs_index: FsIndex = match file_type {
            BackupFileType::Auto => unreachable!("checked for this previously"),
            BackupFileType::CompressedCsv => {
                let mut data = Vec::new();
                file.read_to_end(&mut data)
                    .wrap_err("Failed to read input data")?;
                FsIndex::try_from_csv_records(
                    WizTreeCsvRecord::parse_compressed_csv(&data),
                    FsIndexBuildOptions::default(),
                )?
            }
            BackupFileType::UncompressedCsv => FsIndex::try_from_csv_records(
                WizTreeCsvRecord::parse_uncompressed_csv(&mut file),
                FsIndexBuildOptions::default(),
            )?,
        };

        Ok(fs_index)
    }

    pub fn run(self, cancel_signal: &CancelSignal) -> Result<()> {
        let old = Self::load_backup_file(&self.old, self.old_file_type).wrap_err_with(|| {
            format!("Failed to load old backup file at {}", self.old.display())
        })?;
        let mut new =
            Self::load_backup_file(&self.new, self.new_file_type).wrap_err_with(|| {
                format!("Failed to load new backup file at {}", self.new.display())
            })?;

        if !self.uncompressed
            && let Some(level) = self.compression
            && level > 9
        {
            log::warn!(
                "Compression level should be a number between 0 and 9 but {} was specified",
                level
            );
        }

        // Create output file (exit early if we can't overwrite or create it):
        let mut output_path = self.output.clone();
        let stdout_handle;
        let mut stdout_guard;
        let mut output_file;
        let output: &mut dyn Write = if let Some(output_path) = &mut output_path {
            if output_path.extension().is_none() {
                if self.no_file_extension {
                    log::trace!(
                        "Output file doesn't have a file extension and we aren't adding one automatically because the `--no-file-extension` flag is specified"
                    );
                } else if self.uncompressed {
                    output_path.set_extension("csv");
                } else {
                    output_path.set_extension("csv.gz");
                }
            }
            output_file = Some(create_file(self.overwrite, &output_path).wrap_err_with(|| {
                format!(
                    r#"Failed to create output file at: "{}""#,
                    output_path.display()
                )
            })?);
            output_file.as_mut().unwrap()
        } else {
            stdout_handle = io::stdout();
            stdout_guard = stdout_handle.lock();
            &mut stdout_guard
        };

        filter_fs_index(&old, &mut new, self.filter);

        let output_msg = if let Some(path) = &output_path {
            format!(r#"a file at "{}""#, path.display())
        } else {
            "stdout".to_string()
        };
        log::info!("Finished filtering operation, writing difference information to {output_msg}");

        // Enable compression:
        let mut gz_encoder;
        let mut rsyncable;
        let output: &mut dyn Write = if self.uncompressed {
            output
        } else {
            let compression_level = self
                .compression
                .map(Compression::new)
                .unwrap_or_else(Compression::best);

            gz_encoder = GzEncoder::new(output, compression_level);

            if self.rsyncable.rsyncable {
                rsyncable = Rsyncable::new(gz_encoder);
                &mut rsyncable
            } else {
                &mut gz_encoder
            }
        };

        let output = cancel_signal.wrap_io(output);

        // Create CSV records from result of filesystem scan:
        WizTreeCsvRecord::write_csv_to(new.csv_iter(None, '\\', false), output)
            .map_err(|e| eyre!(e.to_string()))
            .wrap_err("failed to write CSV with filesystem info")?;

        Ok(())
    }
}

pub fn filter_fs_index(old: &FsIndex, new: &mut FsIndex, filter: DiffFilter) {
    let mut parents = Vec::with_capacity(100);
    struct ParentInfo {
        new_id: FsEntryId,
        new_info: FsEntry,
        old_info: Option<(FsEntryId, FsEntry)>,
    }
    fn update_parents(
        parents: &mut Vec<ParentInfo>,
        current_parents: usize,
        new_info: Option<(FsEntryId, FsEntry)>,
        old_info: Option<(FsEntryId, FsEntry)>,
        _old: &FsIndex,
        new: &mut FsIndex,
        filter: DiffFilter,
    ) {
        while parents.len() > current_parents
            // Latest parent can change if we move between sibling folders:
            || (parents.len() == current_parents
                && matches!((parents.last(), &new_info), (Some(last_parent), Some((new_id, _))) if last_parent.new_id != *new_id))
        {
            let parent = parents.pop().unwrap();

            let children = parent
                .new_info
                .children()
                .expect("tracked parent should be a folder");
            let mut meta = parent
                .new_info
                .metadata()
                .clone()
                .into_dir_without_size()
                .into_entry_with_0_files_and_folders();

            let mut child_count = 0;
            for child in children.iter(new) {
                let Some(child_info) = child.load_metadata(new) else {
                    continue; // deleted child
                };
                child_count += 1;
                let child_meta = child_info.metadata();
                meta.size += child_meta.size;
                meta.allocated += child_meta.allocated;
                if child_meta.is_dir {
                    meta.folders += 1 + child_meta.folders;
                    meta.files += child_meta.files;
                } else {
                    meta.files += 1;
                }
            }
            if child_count == 0 && !parents.is_empty() {
                // Consider deleting empty non-root folders:
                let should_delete = match filter {
                    DiffFilter::NewOrLarger => parent.old_info.is_some(), // if not new
                    DiffFilter::NewOrChanged => {
                        if let Some((_, old_info)) = parent.old_info {
                            meta == *old_info.metadata()
                                && old_info
                                    .children()
                                    .is_none_or(|children| children.is_empty())
                        } else {
                            // Folder didn't exist previously
                            false
                        }
                    }
                };
                if should_delete {
                    parent.new_id.set_metadata_id(None, new);
                    continue;
                }
            }
            // If the previous values was lower then the actual folder content indicated
            // then limit the update to the previous values:
            meta.files = parent.new_info.metadata().files.min(meta.files);
            meta.folders = parent.new_info.metadata().folders.min(meta.folders);
            meta.size = parent.new_info.metadata().size.min(meta.size);
            meta.allocated = parent.new_info.metadata().allocated.min(meta.allocated);

            if let Err(e) = parent.new_info.update_metadata(meta.clone(), new) {
                panic!(
                    "Failed to update metadata for folder:\
                            \n\tnew metadata: {meta:?}\
                            \n\told metadata: {:?}\
                            \nError: {e}",
                    parent.new_info.metadata()
                );
            }
        }
        if parents.len() < current_parents {
            assert_eq!(
                parents.len() + 1,
                current_parents,
                "can only add a single parent at a time"
            );
            let Some((new_id, new_info)) = new_info else {
                panic!("if parent count increased then we should know the folder id");
            };
            parents.push(ParentInfo {
                new_id,
                new_info,
                old_info,
            });
        }
    }

    let mut old_cursor = FsCursor::new(DEFAULT_PATH_SEPARATOR);
    if let Some(root) = old.root() {
        old_cursor.set_root(root, Some("/"), old);
    }

    let mut new_cursor = FsCursor::new(DEFAULT_PATH_SEPARATOR);
    if let Some(root) = new.root()
        && let Some(new_info) = root.load_metadata(new)
    {
        new_cursor.set_root(root, Some("/"), new);

        parents.push(ParentInfo {
            new_id: root,
            new_info,
            old_info: old
                .root()
                .and_then(|id| id.load_metadata(old).map(|info| (id, info))),
        });
    }

    new_cursor.advance(new); // skip root (i.e. always keep it)

    while let Some(new_id) = new_cursor.current_id() {
        let full_path = new_cursor.current_path();

        let Some(new_info) = new_id.load_metadata(new) else {
            // This child doesn't have any metadata associated with it, i.e.
            // already deleted from file index.
            new_cursor.advance(new);
            continue;
        };

        let old_info = old_cursor
            .go_to_path(full_path, old) // true if path found
            .then(|| old_cursor.current_id()) // should always be Some if path was found
            .flatten()
            .and_then(|old_id| old_id.load_metadata(old).map(|info| (old_id, info))); // None if found path was removed from old index

        let is_dir = full_path.ends_with(DEFAULT_PATH_SEPARATOR);
        if is_dir {
            // Folders are handled by update_parents function
            update_parents(
                &mut parents,
                new_cursor.parent_count(),
                Some((new_id, new_info.clone())),
                old_info.clone(),
                old,
                new,
                filter,
            );
            new_cursor.advance(new);
            continue;
        }

        let delete = if let Some((_old_id, old_info)) = old_info {
            let new_meta = new_info.metadata();
            let old_meta = old_info.metadata();
            match filter {
                DiffFilter::NewOrLarger if new_meta.size > old_meta.size => {
                    let mut new_meta = new_meta.clone();
                    new_meta.size = new_meta.size.saturating_sub(old_meta.size);
                    new_meta.allocated = new_meta.allocated.saturating_sub(old_meta.allocated);
                    if let Err(e) = new_info.update_metadata(new_meta, new) {
                        panic!("Failed to update file size for file \"{full_path}\": {e}")
                    }
                    false
                }
                DiffFilter::NewOrLarger => true,
                DiffFilter::NewOrChanged => new_meta == old_meta,
            }
        } else {
            // Not found, i.e. represents a new file
            match filter {
                DiffFilter::NewOrLarger => false,
                DiffFilter::NewOrChanged => false,
            }
        };

        if delete {
            new_id.set_metadata_id(None, new);
        }
        new_cursor.advance(new);
    }
    update_parents(&mut parents, 0, None, None, old, new, filter);
}
