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

use crate::display::display_exec;
use crate::lookup::get_dataset;
use std::io::Write;

use crate::{get_pathdata, Config, HttmError, PathData};
use rayon::prelude::*;

use fxhash::FxHashMap as HashMap;
use std::fs::DirEntry;
use std::io::Stdout;
use std::{
    ffi::OsString,
    path::{Path, PathBuf},
    time::SystemTime,
};

pub fn deleted_exec(
    config: &Config,
    out: &mut Stdout,
) -> Result<Vec<Vec<PathData>>, Box<dyn std::error::Error + Send + Sync + 'static>> {
    if config.opt_recursive {
        let path = PathBuf::from(config.raw_paths.get(0).unwrap());
        let pathdata = PathData::new(config, &path);
        recursive_del_search(config, &pathdata, out)?;
        
        std::process::exit(0)
    } else {
        let path = PathBuf::from(&config.raw_paths.get(0).unwrap());
        let pathdata_set = get_deleted(&config, &path)?;
        
        Ok(vec![pathdata_set, Vec::new()])
    }
}

fn recursive_del_search(
    config: &Config,
    pathdata: &PathData,
    out: &mut Stdout,
) -> Result<(), Box<dyn std::error::Error + Send + Sync + 'static>> {
    let read_dir = std::fs::read_dir(&pathdata.path_buf)?;

    // convert to paths, and split into dirs and files
    let (vec_dirs, _): (Vec<PathBuf>, Vec<PathBuf>) = read_dir
        .filter_map(|i| i.ok())
        .map(|dir_entry| dir_entry.path())
        .partition(|path| path.is_dir());

    let vec_deleted: Vec<PathData> = get_deleted(config, &pathdata.path_buf)?;

    let output_buf = display_exec(config, vec![vec_deleted, Vec::new()])?;

    write!(out, "{}", output_buf)?;
    out.flush()?;

    // now recurse into those dirs, if requested
    vec_dirs
        // don't want to a par_iter here because it will block and wait for all results, instead of
        // printing and recursing into the subsequent dirs
        .iter()
        .for_each(|requested_dir| {
            let path = PathData::new(config, requested_dir);
            let _ = recursive_del_search(config, &path, out);
        });
    Ok(())
}

pub fn get_deleted(
    config: &Config,
    path: &Path,
) -> Result<Vec<PathData>, Box<dyn std::error::Error + Send + Sync + 'static>> {
    let pathdata = PathData::new(config, path);

    let dataset = if let Some(ref snap_point) = config.opt_snap_point {
        snap_point.to_owned()
    } else {
        get_dataset(&pathdata)?
    };

    // building the snapshot path
    let snapshot_dir: PathBuf = [&dataset.to_string_lossy(), ".zfs", "snapshot"]
        .iter()
        .collect();

    // building our local relative path by removing parent
    // directories below the remote/snap mount point
    let local_path = if config.opt_snap_point.is_some() {
        pathdata.path_buf
        .strip_prefix(&config.opt_local_dir).map_err(|_| HttmError::new("Are you sure you're in the correct working directory?  Perhaps you need to set the LOCAL_DIR value."))
    } else {
        pathdata.path_buf
        .strip_prefix(&dataset).map_err(|_| HttmError::new("Are you sure you're in the correct working directory?  Perhaps you need to set the SNAP_DIR and LOCAL_DIR values."))
    }?;

    let local_dir_entries: Vec<DirEntry> = std::fs::read_dir(&pathdata.path_buf)?
        .into_iter()
        .par_bridge()
        .flatten()
        .collect();

    let mut local_unique_filenames: HashMap<OsString, PathBuf> = HashMap::default();

    let _ = local_dir_entries.iter().for_each(|dir_entry| {
        let stripped = dir_entry.file_name();
        let _ = local_unique_filenames.insert(stripped, dir_entry.path());
    });

    // Now we have to find all file names in the snap_dirs and compare against the local_dir
    let snap_files: Vec<(OsString, PathBuf)> = std::fs::read_dir(&snapshot_dir)?
        .into_iter()
        .par_bridge()
        .flatten()
        .map(|entry| entry.path())
        .map(|path| path.join(local_path))
        .map(|path| std::fs::read_dir(&path))
        .flatten_iter()
        .flatten_iter()
        .flatten_iter()
        .map(|de| (de.file_name(), de.path()))
        .collect();

    let mut unique_snap_filenames: HashMap<OsString, PathBuf> = HashMap::default();
    let _ = snap_files.into_iter().for_each(|(file_name, path)| {
        let _ = unique_snap_filenames.insert(file_name, path);
    });

    let deleted_file_strings: Vec<String> = unique_snap_filenames
        .par_iter()
        .filter(|(file_name, _)| local_unique_filenames.get(file_name.to_owned()).is_none())
        .map(|(_, path)| path.to_string_lossy().to_string())
        .collect();

    let deleted_pathdata = get_pathdata(config, &deleted_file_strings);

    let mut unique_deleted_versions: HashMap<(SystemTime, u64), PathData> = HashMap::default();
    let _ = deleted_pathdata.into_iter().for_each(|pathdata| {
        let _ = unique_deleted_versions.insert((pathdata.system_time, pathdata.size), pathdata);
    });

    let mut sorted: Vec<_> = unique_deleted_versions.into_iter().collect();

    sorted.par_sort_by_key(|&(k, _)| k);

    Ok(sorted.into_iter().map(|(_, v)| v).collect())
}
