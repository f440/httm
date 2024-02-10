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
// Copyright (c) 2023, Robert Swinford <robert.swinford<...at...>gmail.com>
//
// For the full copyright and license information, please view the LICENSE file
// that was distributed with this source code.

use crate::config::generate::{
    ExecMode, InteractiveMode, PrintMode, RestoreMode, RestoreSnapGuard, SelectMode,
};
use crate::data::paths::{PathData, ZfsSnapPathGuard};
use crate::display_versions::wrapper::VersionsDisplayWrapper;
use crate::exec::preview::PreviewSelection;
use crate::exec::recursive::RecursiveSearch;
use crate::library::file_ops::Copy;
use crate::library::results::{HttmError, HttmResult};
use crate::library::snap_guard::SnapGuard;
use crate::library::utility::{date_string, delimiter, print_output_buf, DateFormat, Never};
use crate::lookup::versions::VersionsMap;
use crate::{Config, GLOBAL_CONFIG};
use crossbeam_channel::unbounded;
use nu_ansi_term::Color::LightYellow;
use skim::prelude::*;
use std::io::{Cursor, Read};
use std::path::{Path, PathBuf};
use std::process::Command as ExecProcess;
use std::thread;
use std::thread::JoinHandle;
use terminal_size::Height;
use terminal_size::Width;

#[derive(Debug)]
pub struct InteractiveBrowse {
    pub selected_pathdata: Vec<PathData>,
    pub opt_background_handle: Option<JoinHandle<()>>,
}

impl InteractiveBrowse {
    pub fn exec(interactive_mode: &InteractiveMode) -> HttmResult<Vec<PathData>> {
        let browse_result = Self::new()?;

        // do we return back to our main exec function to print,
        // or continue down the interactive rabbit hole?
        match interactive_mode {
            InteractiveMode::Restore(_) | InteractiveMode::Select(_) => {
                InteractiveSelect::exec(browse_result, interactive_mode)?;
                unreachable!()
            }
            // InteractiveMode::Browse executes back through fn exec() in main.rs
            InteractiveMode::Browse => Ok(browse_result.selected_pathdata),
        }
    }

    fn new() -> HttmResult<InteractiveBrowse> {
        let browse_result = match &GLOBAL_CONFIG.opt_requested_dir {
            // collect string paths from what we get from lookup_view
            Some(requested_dir) => {
                let view_mode = ViewMode::Browse;
                let browse_result = view_mode.browse(requested_dir)?;
                if browse_result.selected_pathdata.is_empty() {
                    return Err(HttmError::new(
                        "None of the selected strings could be converted to paths.",
                    )
                    .into());
                }

                browse_result
            }
            None => {
                // go to interactive_select early if user has already requested a file
                // and we are in the appropriate mode Select or Restore, see struct Config,
                // and None here is also used for LastSnap to skip browsing for a file/dir
                match GLOBAL_CONFIG.paths.get(0) {
                    Some(first_path) => {
                        let selected_file = first_path.clone();

                        Self {
                            selected_pathdata: vec![selected_file],
                            opt_background_handle: None,
                        }
                    }
                    // Config::from should never allow us to have an instance where we don't
                    // have at least one path to use
                    None => unreachable!(
            "GLOBAL_CONFIG.paths.get(0) should never be a None value in Interactive Mode"
          ),
                }
            }
        };

        Ok(browse_result)
    }
}

struct InteractiveSelect {
    snap_path_strings: Vec<String>,
    opt_live_version: Option<String>,
}

impl InteractiveSelect {
    fn exec(
        browse_result: InteractiveBrowse,
        interactive_mode: &InteractiveMode,
    ) -> HttmResult<()> {
        // continue to interactive_restore or print and exit here?
        let select_result = Self::new(browse_result)?;

        match interactive_mode {
            // one only allow one to select one path string during select
            // but we retain paths_selected_in_browse because we may need
            // it later during restore if opt_overwrite is selected
            InteractiveMode::Restore(_) => {
                let interactive_restore = InteractiveRestore::from(select_result);
                interactive_restore.exec()?;
            }
            InteractiveMode::Select(select_mode) => select_result.print_selections(select_mode)?,
            InteractiveMode::Browse => unreachable!(),
        }

        std::process::exit(0);
    }

    fn new(browse_result: InteractiveBrowse) -> HttmResult<Self> {
        let versions_map = VersionsMap::new(&GLOBAL_CONFIG, &browse_result.selected_pathdata)?;

        // snap and live set has no snaps
        if versions_map.is_empty() {
            let paths: Vec<String> = browse_result
                .selected_pathdata
                .iter()
                .map(|path| path.path_buf.to_string_lossy().to_string())
                .collect();
            let msg = format!(
                "{}{:?}",
                "Cannot select or restore from the following paths as they have no snapshots:\n",
                paths
            );
            return Err(HttmError::new(&msg).into());
        }

        let opt_live_version: Option<String> = if browse_result.selected_pathdata.len() > 1 {
            None
        } else {
            browse_result
                .selected_pathdata
                .get(0)
                .map(|pathdata| pathdata.path_buf.to_string_lossy().into_owned())
        };

        let snap_path_strings = if GLOBAL_CONFIG.opt_last_snap.is_some() {
            Self::last_snap(&versions_map)
        } else {
            // same stuff we do at fn exec, snooze...
            let display_config = Config::from(browse_result.selected_pathdata.clone());

            let display_map = VersionsDisplayWrapper::from(&display_config, versions_map);

            let selection_buffer = display_map.to_string();

            display_map.map.iter().for_each(|(live, snaps)| {
                if snaps.is_empty() {
                    eprintln!("WARN: Path {:?} has no snapshots available.", live.path_buf)
                }
            });

            let view_mode = ViewMode::Select(opt_live_version.clone());

            // loop until user selects a valid snapshot version
            loop {
                // get the file name
                let selected_line = view_mode.select(&selection_buffer, MultiSelect::On)?;

                let requested_file_names = selected_line
                    .iter()
                    .filter_map(|selection| {
                        // ... we want everything between the quotes
                        selection
                            .split_once("\"")
                            .and_then(|(_lhs, rhs)| rhs.rsplit_once("\""))
                            .map(|(lhs, _rhs)| lhs)
                    })
                    .filter(|selection_buffer| {
                        // and cannot select a 'live' version or other invalid value.
                        display_map
                            .keys()
                            .all(|key| key.path_buf.as_path() != Path::new(selection_buffer))
                    })
                    .map(|selection_buffer| selection_buffer.to_string())
                    .collect::<Vec<String>>();

                if requested_file_names.is_empty() {
                    continue;
                }

                break requested_file_names;
            }
        };

        if let Some(handle) = browse_result.opt_background_handle {
            let _ = handle.join();
        }

        Ok(Self {
            snap_path_strings,
            opt_live_version,
        })
    }

    fn last_snap(map: &VersionsMap) -> Vec<String> {
        map.iter()
            .filter_map(|(key, values)| {
                if values.is_empty() {
                    eprintln!(
                        "WARN: No last snap of {:?} is available for selection.  Perhaps you omitted identical files.",
                        key.path_buf
                    );
                    None
                } else {
                    Some(values)
                }
            })
            .flatten()
            .map(|pathdata| pathdata.path_buf.to_string_lossy().to_string())
            .collect()
    }

    fn print_selections(&self, select_mode: &SelectMode) -> HttmResult<()> {
        self.snap_path_strings
            .iter()
            .map(Path::new)
            .try_for_each(|snap_path| self.print_snap_path(snap_path, select_mode))?;

        Ok(())
    }

    fn print_snap_path(&self, snap_path: &Path, select_mode: &SelectMode) -> HttmResult<()> {
        match select_mode {
            SelectMode::Path => {
                let delimiter = delimiter();
                let output_buf = match GLOBAL_CONFIG.print_mode {
                    PrintMode::RawNewline | PrintMode::RawZero => {
                        format!("{}{delimiter}", snap_path.to_string_lossy())
                    }
                    PrintMode::FormattedDefault | PrintMode::FormattedNotPretty => {
                        format!("\"{}\"{delimiter}", snap_path.to_string_lossy())
                    }
                };

                print_output_buf(&output_buf)?;

                Ok(())
            }
            SelectMode::Contents => {
                if !snap_path.is_file() {
                    let msg = format!("Path is not a file: {:?}", snap_path);
                    return Err(HttmError::new(&msg).into());
                }
                let mut f = std::fs::File::open(snap_path)?;
                let mut contents = Vec::new();
                f.read_to_end(&mut contents)?;

                // SAFETY: Panic here is not the end of the world as we are just printing the bytes.
                // This is the same as simply `cat`-ing the file.
                let output_buf = unsafe { std::str::from_utf8_unchecked(&contents) };

                print_output_buf(output_buf)?;

                Ok(())
            }
            SelectMode::Preview => {
                let view_mode = ViewMode::Select(self.opt_live_version.clone());

                let preview_selection = PreviewSelection::new(&view_mode)?;

                let cmd = if let Some(command) = preview_selection.opt_preview_command {
                    command.replace("$snap_file", &format!("{:?}", snap_path))
                } else {
                    return Err(HttmError::new("Could not parse preview command").into());
                };

                let env_command =
                    which::which("env").unwrap_or_else(|_| PathBuf::from("/usr/bin/env"));

                let spawned = ExecProcess::new(env_command)
                    .arg("bash")
                    .arg("-c")
                    .arg(cmd)
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::piped())
                    .spawn()?;

                match spawned.stdout {
                    Some(mut stdout) => {
                        let mut output_buf = String::new();
                        stdout.read_to_string(&mut output_buf)?;
                        print_output_buf(&output_buf)
                    }
                    None => match spawned.stderr {
                        Some(mut stderr) => {
                            let mut output_buf = String::new();
                            stderr.read_to_string(&mut output_buf)?;
                            if !output_buf.is_empty() {
                                eprintln!("{}", &output_buf)
                            }
                            Ok(())
                        }
                        None => {
                            let msg = format!(
                                "Preview command output was empty for path: {:?}",
                                snap_path
                            );
                            Err(HttmError::new(&msg).into())
                        }
                    },
                }
            }
        }
    }

    pub fn opt_live_version(&self, snap_pathdata: &PathData) -> HttmResult<PathBuf> {
        match &self.opt_live_version {
            Some(live_version) => Some(PathBuf::from(live_version)),
            None => ZfsSnapPathGuard::new(snap_pathdata)
                .and_then(|snap_guard| snap_guard.live_path())
                .map(|pathdata| pathdata.path_buf),
        }
        .ok_or_else(|| HttmError::new("Could not determine a possible live version.").into())
    }
}

struct InteractiveRestore {
    select_result: InteractiveSelect,
}

impl From<InteractiveSelect> for InteractiveRestore {
    fn from(value: InteractiveSelect) -> Self {
        Self {
            select_result: value,
        }
    }
}

impl InteractiveRestore {
    fn exec(&self) -> HttmResult<()> {
        self.select_result
            .snap_path_strings
            .iter()
            .try_for_each(|snap_path_string| self.restore(snap_path_string))?;

        std::process::exit(0)
    }

    fn restore(&self, snap_path_string: &str) -> HttmResult<()> {
        // build pathdata from selection buffer parsed string
        //
        // request is also sanity check for snap path exists below when we check
        // if snap_pathdata is_phantom below
        let snap_pathdata = PathData::from(Path::new(snap_path_string));

        // build new place to send file
        let new_file_path_buf = self.build_new_file_path(&snap_pathdata)?;

        let should_preserve = Self::should_preserve_attributes();

        // tell the user what we're up to, and get consent
        let preview_buffer = format!(
            "httm will copy a file from a snapshot:\n\n\
            \tfrom: {:?}\n\
            \tto:   {new_file_path_buf:?}\n\n\
            Before httm restores this file, it would like your consent. Continue? (YES/NO)\n\
            ──────────────────────────────────────────────────────────────────────────────\n\
            YES\n\
            NO",
            snap_pathdata.path_buf
        );

        // loop until user consents or doesn't
        loop {
            let view_mode = &ViewMode::Restore;

            let selection = view_mode.select(&preview_buffer, MultiSelect::Off)?;

            let user_consent = selection
                .get(0)
                .ok_or_else(|| HttmError::new("Could not obtain the first match selected."))?;

            match user_consent.to_ascii_uppercase().as_ref() {
                "YES" | "Y" => {
                    if matches!(
                        GLOBAL_CONFIG.exec_mode,
                        ExecMode::Interactive(InteractiveMode::Restore(RestoreMode::Overwrite(
                            RestoreSnapGuard::Guarded
                        )))
                    ) {
                        let snap_guard: SnapGuard =
                            SnapGuard::try_from(new_file_path_buf.as_path())?;

                        if let Err(err) = Copy::recursive(
                            &snap_pathdata.path_buf,
                            &new_file_path_buf,
                            should_preserve,
                        ) {
                            let msg = format!(
                                "httm restore failed for the following reason: {}.\n\
                            Attempting roll back to precautionary pre-execution snapshot.",
                                err
                            );

                            eprintln!("{}", msg);

                            snap_guard
                                .rollback()
                                .map(|_| println!("Rollback succeeded."))?;

                            std::process::exit(1);
                        }
                    } else {
                        if let Err(err) = Copy::recursive(
                            &snap_pathdata.path_buf,
                            &new_file_path_buf,
                            should_preserve,
                        ) {
                            let msg =
                                format!("httm restore failed for the following reason: {}.", err);
                            return Err(HttmError::new(&msg).into());
                        }
                    }

                    let result_buffer = format!(
                        "httm copied from snapshot:\n\n\
                            \tsource:\t{:?}\n\
                            \ttarget:\t{new_file_path_buf:?}\n\n\
                            Restore completed successfully.",
                        snap_pathdata.path_buf
                    );

                    let summary_string = LightYellow.paint(Self::summary_string());

                    break println!("{summary_string}{result_buffer}");
                }
                "NO" | "N" => {
                    break println!("User declined restore of: {:?}", snap_pathdata.path_buf)
                }
                // if not yes or no, then noop and continue to the next iter of loop
                _ => {}
            }
        }

        Ok(())
    }

    fn summary_string() -> String {
        let width = match terminal_size::terminal_size() {
            Some((Width(width), Height(_height))) => width as usize,
            None => 80usize,
        };

        format!("{:^width$}\n", "====> [ httm recovery summary ] <====")
    }

    fn should_preserve_attributes() -> bool {
        matches!(
            GLOBAL_CONFIG.exec_mode,
            ExecMode::Interactive(InteractiveMode::Restore(
                RestoreMode::CopyAndPreserve | RestoreMode::Overwrite(_)
            ))
        )
    }

    fn build_new_file_path(&self, snap_pathdata: &PathData) -> HttmResult<PathBuf> {
        // build new place to send file
        if matches!(
            GLOBAL_CONFIG.exec_mode,
            ExecMode::Interactive(InteractiveMode::Restore(RestoreMode::Overwrite(_)))
        ) {
            // instead of just not naming the new file with extra info (date plus "httm_restored") and shoving that new file
            // into the pwd, here, we actually look for the original location of the file to make sure we overwrite it.
            // so, if you were in /etc and wanted to restore /etc/samba/smb.conf, httm will make certain to overwrite
            // at /etc/samba/smb.conf

            return self.select_result.opt_live_version(snap_pathdata);
        }

        let snap_filename = snap_pathdata
            .path_buf
            .file_name()
            .expect("Could not obtain a file name for the snap file version of path given")
            .to_string_lossy()
            .into_owned();

        let Some(snap_metadata) = snap_pathdata.metadata else {
            let msg = format!(
                "Source location: {:?} does not exist on disk Quitting.",
                snap_pathdata.path_buf
            );
            return Err(HttmError::new(&msg).into());
        };

        // remove leading dots
        let new_filename = snap_filename
            .strip_prefix(".")
            .unwrap_or(&snap_filename)
            .to_string()
            + ".httm_restored."
            + &date_string(
                GLOBAL_CONFIG.requested_utc_offset,
                &snap_metadata.modify_time,
                DateFormat::Timestamp,
            );
        let new_file_dir = GLOBAL_CONFIG.pwd.as_path();
        let new_file_path_buf: PathBuf = new_file_dir.join(new_filename);

        // don't let the user rewrite one restore over another in non-overwrite mode
        if new_file_path_buf.exists() {
            Err(
                    HttmError::new("httm will not restore to that file, as a file with the same path name already exists. Quitting.").into(),
                )
        } else {
            Ok(new_file_path_buf)
        }
    }
}

pub enum ViewMode {
    Browse,
    Select(Option<String>),
    Restore,
    Prune,
}

pub enum MultiSelect {
    On,
    Off,
}

impl ViewMode {
    fn print_header(&self) -> String {
        format!(
            "PREVIEW UP: shift+up | PREVIEW DOWN: shift+down | {}\n\
        PAGE UP:    page up  | PAGE DOWN:    page down \n\
        EXIT:       esc      | SELECT:       enter      | SELECT, MULTIPLE: shift+tab\n\
        ──────────────────────────────────────────────────────────────────────────────",
            self.print_mode()
        )
    }

    fn print_mode(&self) -> &str {
        match self {
            ViewMode::Browse => "====> [ Browse Mode ] <====",
            ViewMode::Select(_) => "====> [ Select Mode ] <====",
            ViewMode::Restore => "====> [ Restore Mode ] <====",
            ViewMode::Prune => "====> [ Prune Mode ] <====",
        }
    }

    fn browse(&self, requested_dir: &Path) -> HttmResult<InteractiveBrowse> {
        // prep thread spawn
        let requested_dir_clone = requested_dir.to_path_buf();
        let (tx_item, rx_item): (SkimItemSender, SkimItemReceiver) = unbounded();
        let (hangup_tx, hangup_rx): (Sender<Never>, Receiver<Never>) = bounded(0);

        // thread spawn fn enumerate_directory - permits recursion into dirs without blocking
        let background_handle = thread::spawn(move || {
            // no way to propagate error from closure so exit and explain error here
            RecursiveSearch::exec(&requested_dir_clone, tx_item.clone(), hangup_rx.clone());
        });

        let header: String = self.print_header();

        let display_handle = thread::spawn(move || {
            #[cfg(feature = "setpriority")]
            #[cfg(target_os = "linux")]
            #[cfg(target_env = "gnu")]
            {
                use crate::library::utility::ThreadPriorityType;
                let tid = std::process::id();
                let _ = ThreadPriorityType::Process.nice_thread(Some(tid), -3i32);
            }

            let opt_multi = GLOBAL_CONFIG.opt_preview.is_none();

            // create the skim component for previews
            let skim_opts = SkimOptionsBuilder::default()
                .preview_window(Some("up:50%"))
                .preview(Some(""))
                .nosort(true)
                .exact(GLOBAL_CONFIG.opt_exact)
                .header(Some(&header))
                .multi(opt_multi)
                .regex(false)
                .build()
                .expect("Could not initialized skim options for browse_view");

            // run_with() reads and shows items from the thread stream created above
            let res = match skim::Skim::run_with(&skim_opts, Some(rx_item)) {
                Some(output) if output.is_abort => {
                    eprintln!("httm interactive file browse session was aborted.  Quitting.");
                    std::process::exit(0)
                }
                Some(output) => {
                    // hangup the channel so the background recursive search can gracefully cleanup and exit
                    drop(hangup_tx);

                    output
                        .selected_items
                        .iter()
                        .map(|i| PathData::from(Path::new(&i.output().to_string())))
                        .collect()
                }
                None => {
                    return Err(HttmError::new(
                        "httm interactive file browse session failed.",
                    ));
                }
            };

            #[cfg(feature = "malloc_trim")]
            #[cfg(target_os = "linux")]
            #[cfg(target_env = "gnu")]
            {
                use crate::library::utility::malloc_trim;
                malloc_trim();
            }

            Ok(res)
        });

        match display_handle.join() {
            Ok(selected_pathdata) => {
                let res = InteractiveBrowse {
                    selected_pathdata: selected_pathdata?,
                    opt_background_handle: Some(background_handle),
                };
                Ok(res)
            }
            Err(_) => Err(HttmError::new("Interactive browse thread panicked.").into()),
        }
    }

    pub fn select(&self, preview_buffer: &str, opt_multi: MultiSelect) -> HttmResult<Vec<String>> {
        let preview_selection = PreviewSelection::new(self)?;

        let header = self.print_header();

        let opt_multi = match opt_multi {
            MultiSelect::On => true,
            MultiSelect::Off => false,
        };

        // build our browse view - less to do than before - no previews, looking through one 'lil buffer
        let skim_opts = SkimOptionsBuilder::default()
            .preview_window(preview_selection.opt_preview_window.as_deref())
            .preview(preview_selection.opt_preview_command.as_deref())
            .disabled(true)
            .tac(true)
            .nosort(true)
            .tabstop(Some("4"))
            .exact(true)
            .multi(opt_multi)
            .regex(false)
            .tiebreak(Some("length,index".to_string()))
            .header(Some(&header))
            .build()
            .expect("Could not initialized skim options for select_restore_view");

        let item_reader_opts = SkimItemReaderOption::default().ansi(true);
        let item_reader = SkimItemReader::new(item_reader_opts);

        let (items, opt_ingest_handle) =
            item_reader.of_bufread(Box::new(Cursor::new(preview_buffer.trim().to_owned())));

        // run_with() reads and shows items from the thread stream created above
        let res = match skim::Skim::run_with(&skim_opts, Some(items)) {
            Some(output) if output.is_abort => {
                eprintln!("httm select/restore/prune session was aborted.  Quitting.");
                std::process::exit(0);
            }
            Some(output) => output
                .selected_items
                .iter()
                .map(|i| i.output().into_owned())
                .collect(),
            None => {
                return Err(HttmError::new("httm select/restore/prune session failed.").into());
            }
        };

        if let Some(handle) = opt_ingest_handle {
            let _ = handle.join();
        };

        if GLOBAL_CONFIG.opt_debug {
            if let Some(preview_command) = preview_selection.opt_preview_command.as_deref() {
                eprintln!("DEBUG: Preview command executed: {}", preview_command)
            }
        }

        Ok(res)
    }
}
