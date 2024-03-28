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

use crate::background::recursive::RecursiveSearch;
use crate::data::paths::PathData;
use crate::interactive::view_mode::ViewMode;
use crate::library::results::{HttmError, HttmResult};
use crate::library::utility::Never;
use crate::GLOBAL_CONFIG;
use crossbeam_channel::unbounded;
use skim::prelude::*;
use std::path::Path;
use std::thread;

#[derive(Debug)]
pub struct InteractiveBrowse {
    pub selected_pathdata: Vec<PathData>,
}

impl InteractiveBrowse {
    pub fn new() -> HttmResult<Self> {
        let browse_result = match &GLOBAL_CONFIG.opt_requested_dir {
            // collect string paths from what we get from lookup_view
            Some(requested_dir) => {
                let selected_pathdata = Self::view(requested_dir)?;

                if selected_pathdata.is_empty() {
                    return Err(HttmError::new(
                        "None of the selected strings could be converted to paths.",
                    )
                    .into());
                }

                selected_pathdata
            }
            None => {
                // go to interactive_select early if user has already requested a file
                // and we are in the appropriate mode Select or Restore, see struct Config,
                // and None here is also used for LastSnap to skip browsing for a file/dir
                match GLOBAL_CONFIG.paths.get(0) {
                    Some(first_path) => {
                        let selected_file = first_path.clone();

                        vec![selected_file]
                    }
                    // Config::from should never allow us to have an instance where we don't
                    // have at least one path to use
                    None => unreachable!(
                        "GLOBAL_CONFIG.paths.get(0) should never be a None value in Interactive Mode"
                    ),
                }
            }
        };

        Ok(Self {
            selected_pathdata: browse_result,
        })
    }

    #[allow(dead_code)]
    #[cfg(feature = "malloc_trim")]
    #[cfg(target_os = "linux")]
    #[cfg(target_env = "gnu")]
    fn malloc_trim() {
        unsafe {
            let _ = libc::malloc_trim(0usize);
        }
    }

    fn view(requested_dir: &Path) -> HttmResult<Vec<PathData>> {
        // prep thread spawn
        let requested_dir_clone = requested_dir.to_path_buf();
        let (tx_item, rx_item): (SkimItemSender, SkimItemReceiver) = unbounded();
        let (hangup_tx, hangup_rx): (Sender<Never>, Receiver<Never>) = bounded(0);

        // thread spawn fn enumerate_directory - permits recursion into dirs without blocking
        let background_handle = std::thread::spawn(move || {
            // no way to propagate error from closure so exit and explain error here
            RecursiveSearch::exec(&requested_dir_clone, tx_item.clone(), hangup_rx.clone());
        });

        let header: String = ViewMode::Browse.print_header();

        let opt_multi = GLOBAL_CONFIG.opt_preview.is_none();

        let display_thread = thread::spawn(move || {
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

            skim::Skim::run_with(&skim_opts, Some(rx_item))
        });

        // run_with() reads and shows items from the thread stream created above
        match display_thread.join().ok().flatten() {
            Some(output) if output.is_abort => {
                eprintln!("httm interactive file browse session was aborted.  Quitting.");
                std::process::exit(0)
            }
            Some(output) => {
                // hangup the channel so the background recursive search can gracefully cleanup and exit
                drop(hangup_tx);
                let _ = background_handle.join();

                #[cfg(feature = "malloc_trim")]
                #[cfg(target_os = "linux")]
                #[cfg(target_env = "gnu")]
                Self::malloc_trim();

                let selected_pathdata: Vec<PathData> = output
                    .selected_items
                    .iter()
                    .map(|item| PathData::from(Path::new(item.output().as_ref())))
                    .collect();

                Ok(selected_pathdata)
            }
            None => Err(HttmError::new("httm interactive file browse session failed.").into()),
        }
    }
}
