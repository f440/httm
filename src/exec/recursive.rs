//       ___           ___           ___           ___
//      /\__\         /\  \         /\  \         /\__\
//     /:/  /         \:\  \        \:\  \       /::|  |
//    /:/__/           \:\  \        \:\  \     /:|:|  |
//   /::\  \ ___       /::\  \       /::\  \   /:/|:|__|__
//  /:/\:\  /\__\     /:/\:\__\     /:/\:\__\ /:/ |::::\__\
//  \/__\:\/:/  /    /:/  \/__/    /:/  \/__/ \/__/~~/:/  /
//       \::/  /    /:/  /        /:/  /            /:/  /
//       /:/  /     \/__/         \/__/            /:/  /
//      /:/  /                                    /:/  /
//      \/__/                                     \/__/
//
// (c) Robert Swinford <robert.swinford<...at...>gmail.com>
//
// For the full copyright and license information, please view the LICENSE file
// that was distributed with this source code.

use std::collections::VecDeque;
use std::{fs::read_dir, path::Path, sync::Arc};

use once_cell::unsync::OnceCell;
use rayon::{prelude::*, Scope, ThreadPool};
use skim::prelude::*;

use crate::config::init::Config;
use crate::config::init::{DeletedMode, ExecMode};
use crate::data::paths::{BasicDirEntryInfo, PathData};
use crate::exec::display_main::display_exec;
use crate::exec::interactive::SelectionCandidate;
use crate::library::results::{HttmError, HttmResult};
use crate::library::utility::{httm_is_dir, print_output_buf, HttmIsDir};
use crate::lookup::deleted::deleted_lookup_exec;
use crate::lookup::versions::versions_lookup_exec;
use crate::{BTRFS_SNAPPER_HIDDEN_DIRECTORY, ZFS_HIDDEN_DIRECTORY};

pub fn display_recursive_wrapper(config: Arc<Config>) -> HttmResult<()> {
    // won't be sending anything anywhere, this just allows us to reuse enumerate_directory
    let (dummy_skim_tx_item, _): (SkimItemSender, SkimItemReceiver) = unbounded();
    let config_clone = config.clone();

    match &config.opt_requested_dir {
        Some(requested_dir) => {
            recursive_exec(config_clone, &requested_dir.path_buf, dummy_skim_tx_item)?;
        }
        None => {
            return Err(HttmError::new(
                "requested_dir should never be None in Display Recursive mode",
            )
            .into())
        }
    }

    Ok(())
}

pub fn recursive_exec(
    config: Arc<Config>,
    requested_dir: &Path,
    skim_tx_item: SkimItemSender,
) -> HttmResult<()> {
    // default stack size for rayon threads spawned to handle enumerate_deleted
    // here set at 1MB (the Linux default is 8MB) to avoid a stack overflow with the Rayon default
    const DEFAULT_STACK_SIZE: usize = 1_048_576;

    // build thread pool with a stack size large enough to avoid a stack overflow
    // this will be our one threadpool for directory enumeration ops
    let pool: ThreadPool = rayon::ThreadPoolBuilder::new()
        .stack_size(DEFAULT_STACK_SIZE)
        .build()
        .expect("Could not initialize rayon threadpool for recursive deleted search");

    pool.in_place_scope(|deleted_scope| {
        iterative_enumeration(config.clone(), requested_dir, deleted_scope, &skim_tx_item)
            .unwrap_or_else(|error| {
                eprintln!("Error: {}", error);
                std::process::exit(1)
            });
    });

    // this would implicitly dropped but want to be clear what we are doing
    // when a threadpool is dropped it signals the remaining threads to shut down
    drop(pool);

    Ok(())
}

fn iterative_enumeration(
    config: Arc<Config>,
    requested_dir: &Path,
    deleted_scope: &Scope,
    skim_tx_item: &SkimItemSender,
) -> HttmResult<()> {
    // runs once for non-recursive but also "primes the pump"
    // for recursive to have items available
    let mut queue: VecDeque<BasicDirEntryInfo> =
        enumerate_live(config.clone(), requested_dir, deleted_scope, skim_tx_item)?.into();

    if config.opt_recursive {
        // condition kills iter when user has made a selection
        // pop_back makes this a LIFO queue which is supposedly better for caches
        while let Some(item) = queue.pop_back() {
            // no errors will be propagated in recursive mode
            // far too likely to run into a dir we don't have permissions to view
            if let Ok(vec_dirs) =
                enumerate_live(config.clone(), &item.path, deleted_scope, skim_tx_item)
            {
                queue.extend(vec_dirs.into_iter())
            }
        }
    }

    Ok(())
}

fn enumerate_live(
    config: Arc<Config>,
    requested_dir: &Path,
    deleted_scope: &Scope,
    skim_tx_item: &SkimItemSender,
) -> HttmResult<Vec<BasicDirEntryInfo>> {
    // combined entries will be sent or printed, but we need the vec_dirs to recurse
    let (vec_dirs, vec_files): (Vec<BasicDirEntryInfo>, Vec<BasicDirEntryInfo>) =
        get_entries_partitioned(config.as_ref(), requested_dir)?;

    combine_and_send_entries(
        config.clone(),
        vec_files,
        &vec_dirs,
        false,
        requested_dir,
        skim_tx_item,
    )?;

    spawn_deleted(config, requested_dir, deleted_scope, skim_tx_item);

    Ok(vec_dirs)
}

fn combine_and_send_entries(
    config: Arc<Config>,
    vec_files: Vec<BasicDirEntryInfo>,
    vec_dirs: &[BasicDirEntryInfo],
    is_phantom: bool,
    requested_dir: &Path,
    skim_tx_item: &SkimItemSender,
) -> HttmResult<()> {
    let mut combined = vec_files;
    combined.extend_from_slice(vec_dirs);

    let entries = if is_phantom {
        get_pseudo_live_versions(combined, requested_dir)
    } else {
        match config.deleted_mode {
            DeletedMode::Only => {
                // spawn_enumerate_deleted will send deleted files back to
                // the main thread for us, so we can skip collecting deleted here
                // and return an empty vec
                Vec::new()
            }
            DeletedMode::DepthOfOne | DeletedMode::Enabled | DeletedMode::Disabled => {
                // DepthOfOne will be handled inside enumerate_deleted
                combined
            }
        }
    };

    // is_phantom is false because these are known live entries
    display_or_transmit(config, entries, is_phantom, skim_tx_item)?;

    Ok(())
}

// "spawn" a lighter weight rayon/greenish thread for enumerate_deleted, if needed
fn spawn_deleted(
    config: Arc<Config>,
    requested_dir: &Path,
    deleted_scope: &Scope,
    skim_tx_item: &SkimItemSender,
) {
    match config.deleted_mode {
        DeletedMode::Only | DeletedMode::DepthOfOne | DeletedMode::Enabled => {
            // spawn_enumerate_deleted will send deleted files back to
            // the main thread for us, so we can skip collecting deleted here
            // and return an empty vec
            let requested_dir_clone = requested_dir.to_path_buf();
            let skim_tx_item_clone = skim_tx_item.clone();

            deleted_scope.spawn(move |_| {
                let _ = enumerate_deleted(config, &requested_dir_clone, &skim_tx_item_clone);
            });
        }
        DeletedMode::Disabled => (),
    }
}

fn get_entries_partitioned(
    config: &Config,
    requested_dir: &Path,
) -> HttmResult<(Vec<BasicDirEntryInfo>, Vec<BasicDirEntryInfo>)> {
    //separates entries into dirs and files
    let (vec_dirs, vec_files) = read_dir(&requested_dir)?
        .flatten()
        // checking file_type on dir entries is always preferable
        // as it is much faster than a metadata call on the path
        .map(|dir_entry| BasicDirEntryInfo::from(&dir_entry))
        .filter(|entry| {
            if config.opt_no_filter {
                return true;
            } else if let Ok(file_type) = entry.get_filetype() {
                if file_type.is_dir() {
                    return !is_filter_dir(config, entry);
                }
            }
            true
        })
        .partition(|entry| {
            // must do is_dir() look up on file type as look up on path will traverse links!
            if config.opt_no_traverse {
                if let Ok(file_type) = entry.get_filetype() {
                    return file_type.is_dir();
                }
            }
            httm_is_dir(entry)
        });

    Ok((vec_dirs, vec_files))
}

fn is_filter_dir(config: &Config, entry: &BasicDirEntryInfo) -> bool {
    // FYI path is always a relative path, but no need to canonicalize as
    // partial eq for paths is comparison of components iter
    let path = entry.path.as_path();

    // never check the hidden snapshot directory for live files (duh)
    // didn't think this was possible until I saw a SMB share return
    // a .zfs dir entry
    if path.ends_with(ZFS_HIDDEN_DIRECTORY) || path.ends_with(BTRFS_SNAPPER_HIDDEN_DIRECTORY) {
        return true;
    }

    // is 1) a common snapshot path for btrfs, or 2) is a non-supported (non-ZFS, non-btrfs) dataset?

    // is a common btrfs snapshot dir?
    if let Some(common_snap_dir) = &config.dataset_collection.opt_common_snap_dir {
        if path == *common_snap_dir {
            return true;
        }
    }

    let user_requested_dir = config
        .opt_requested_dir
        .as_ref()
        .expect("opt_requested_dir must always be Some in any recursive mode")
        .path_buf
        .as_path();

    // check whether user requested this dir specifically, then we will show
    if path == user_requested_dir {
        false
    // else: is a non-supported dataset?
    } else {
        config
            .dataset_collection
            .vec_of_filter_dirs
            .par_iter()
            .any(|filter_dir| path == filter_dir)
    }
}

// deleted file search for all modes
fn enumerate_deleted(
    config: Arc<Config>,
    requested_dir: &Path,
    skim_tx_item: &SkimItemSender,
) -> HttmResult<()> {
    // obtain all unique deleted, policy is one version for each file, latest in time
    let deleted = deleted_lookup_exec(config.as_ref(), requested_dir)?;

    // combined entries will be sent or printed, but we need the vec_dirs to recurse
    let (vec_dirs, vec_files): (Vec<BasicDirEntryInfo>, Vec<BasicDirEntryInfo>) =
        deleted.into_iter().partition(|entry| {
            // no need to traverse symlinks in deleted search
            if let Some(file_type) = entry.file_type {
                file_type.is_dir()
            } else {
                false
            }
        });

    combine_and_send_entries(
        config.clone(),
        vec_files,
        &vec_dirs,
        true,
        requested_dir,
        skim_tx_item,
    )?;

    // disable behind deleted dirs with DepthOfOne,
    // otherwise recurse and find all those deleted files
    //
    // don't propagate errors, errors we are most concerned about
    // are transmission errors, which are handled elsewhere
    if config.deleted_mode != DeletedMode::DepthOfOne && config.opt_recursive {
        vec_dirs
            .into_iter()
            .map(|basic_dir_entry_info| basic_dir_entry_info.path)
            .try_for_each(|deleted_dir| {
                let config_clone = config.clone();
                let requested_dir_clone = requested_dir.to_path_buf();

                get_entries_behind_deleted_dir(
                    config_clone,
                    &deleted_dir,
                    &requested_dir_clone,
                    skim_tx_item,
                )
            })
    } else {
        Ok(())
    }
}

// searches for all files behind the dirs that have been deleted
// recurses over all dir entries and creates pseudo live versions
// for them all, policy is to use the latest snapshot version before
// deletion
fn get_entries_behind_deleted_dir(
    config: Arc<Config>,
    deleted_dir: &Path,
    requested_dir: &Path,
    skim_tx_item: &SkimItemSender,
) -> HttmResult<()> {
    fn recurse_behind_deleted_dir(
        config: Arc<Config>,
        dir_name: &Path,
        from_deleted_dir: &Path,
        from_requested_dir: &Path,
        skim_tx_item: &SkimItemSender,
    ) -> HttmResult<()> {
        // deleted_dir_on_snap is the path from the deleted dir on the snapshot
        // pseudo_live_dir is the path from the fake, deleted directory that once was
        let deleted_dir_on_snap = &from_deleted_dir.to_path_buf().join(&dir_name);
        let pseudo_live_dir = &from_requested_dir.to_path_buf().join(&dir_name);

        let (vec_dirs, vec_files): (Vec<BasicDirEntryInfo>, Vec<BasicDirEntryInfo>) =
            get_entries_partitioned(config.as_ref(), deleted_dir_on_snap)?;

        combine_and_send_entries(
            config.clone(),
            vec_files,
            &vec_dirs,
            true,
            pseudo_live_dir,
            skim_tx_item,
        )?;

        // now recurse!
        // don't propagate errors, errors we are most concerned about
        // are transmission errors, which are handled elsewhere
        vec_dirs.into_iter().try_for_each(|basic_dir_entry_info| {
            recurse_behind_deleted_dir(
                config.clone(),
                Path::new(&basic_dir_entry_info.file_name),
                deleted_dir_on_snap,
                pseudo_live_dir,
                skim_tx_item,
            )
        })
    }

    match &deleted_dir.file_name() {
        Some(dir_name) => recurse_behind_deleted_dir(
            config,
            Path::new(dir_name),
            deleted_dir.parent().unwrap_or_else(|| Path::new("/")),
            requested_dir,
            skim_tx_item,
        )?,
        None => return Err(HttmError::new("Not a valid file name!").into()),
    }

    Ok(())
}

// this function creates dummy "live versions" values to match deleted files
// which have been found on snapshots, we return to the user "the path that
// once was" in their browse panel
fn get_pseudo_live_versions(
    entries: Vec<BasicDirEntryInfo>,
    pseudo_live_dir: &Path,
) -> Vec<BasicDirEntryInfo> {
    entries
        .into_iter()
        .map(|basic_dir_entry_info| BasicDirEntryInfo {
            path: pseudo_live_dir.join(&basic_dir_entry_info.file_name),
            file_name: basic_dir_entry_info.file_name,
            file_type: basic_dir_entry_info.file_type,
            modify_time: OnceCell::new(),
        })
        .collect()
}

fn display_or_transmit(
    config: Arc<Config>,
    entries: Vec<BasicDirEntryInfo>,
    is_phantom: bool,
    skim_tx_item: &SkimItemSender,
) -> HttmResult<()> {
    // send to the interactive view, or print directly, never return back
    match &config.exec_mode {
        ExecMode::Interactive(_) => {
            transmit_entries(config.clone(), entries, is_phantom, skim_tx_item)?
        }
        ExecMode::DisplayRecursive(progress_bar) => {
            if entries.is_empty() {
                progress_bar.tick();
            } else {
                print_display_recursive(config.as_ref(), entries)?;
                // keeps spinner from squashing last line of output
                eprintln!();
            }
        }
        _ => unreachable!(),
    }

    Ok(())
}

fn transmit_entries(
    config: Arc<Config>,
    entries: Vec<BasicDirEntryInfo>,
    is_phantom: bool,
    skim_tx_item: &SkimItemSender,
) -> HttmResult<()> {
    // don't want a par_iter here because it will block and wait for all
    // results, instead of printing and recursing into the subsequent dirs
    entries
        .into_iter()
        .try_for_each(|basic_dir_entry_info| {
            skim_tx_item.try_send(Arc::new(SelectionCandidate::new(
                config.clone(),
                basic_dir_entry_info,
                is_phantom,
            )))
        })
        .map_err(|err| err.into())
}

fn print_display_recursive(config: &Config, entries: Vec<BasicDirEntryInfo>) -> HttmResult<()> {
    let pseudo_live_set: Vec<PathData> = entries
        .iter()
        .map(|basic_dir_entry_info| PathData::from(basic_dir_entry_info.path.as_path()))
        .collect();

    let map_live_to_snaps = versions_lookup_exec(config, &pseudo_live_set)?;

    let output_buf = display_exec(config, &map_live_to_snaps)?;

    print_output_buf(output_buf)?;

    Ok(())
}
