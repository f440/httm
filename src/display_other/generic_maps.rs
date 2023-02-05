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

use std::collections::BTreeMap;
use std::ops::Deref;

use crate::config::generate::{Config, MountDisplay, PrintMode};
use crate::display_versions::format::{NOT_SO_PRETTY_FIXED_WIDTH_PADDING, QUOTATION_MARKS_LEN};
use crate::MountsForFiles;
use crate::SnapNameMap;
use crate::VersionsMap;

pub struct PrintAsMap {
    inner: BTreeMap<String, Vec<String>>,
}

impl Deref for PrintAsMap {
    type Target = BTreeMap<String, Vec<String>>;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl From<&MountsForFiles> for PrintAsMap {
    fn from(mounts_for_files: &MountsForFiles) -> Self {
        let inner = mounts_for_files
            .iter()
            .map(|(key, values)| {
                let res = values
                    .iter()
                    .filter_map(|value| match mounts_for_files.mount_display {
                        MountDisplay::Directory => {
                            Some(value.path_buf.to_string_lossy().to_string())
                        }
                        MountDisplay::DeviceDataset => {
                            let opt_md = mounts_for_files
                                .config
                                .dataset_collection
                                .map_of_datasets
                                .datasets
                                .get(&value.path_buf);
                            opt_md.map(|md| md.name.to_owned())
                        }
                    })
                    .collect();
                (key.path_buf.to_string_lossy().to_string(), res)
            })
            .collect();
        Self { inner }
    }
}

impl From<&VersionsMap> for PrintAsMap {
    fn from(map: &VersionsMap) -> Self {
        let inner = map
            .iter()
            .map(|(key, values)| {
                let res = values
                    .iter()
                    .map(|value| value.path_buf.to_string_lossy().to_string())
                    .collect();
                (key.path_buf.to_string_lossy().to_string(), res)
            })
            .collect();
        Self { inner }
    }
}

impl From<&SnapNameMap> for PrintAsMap {
    fn from(map: &SnapNameMap) -> Self {
        let inner = map
            .iter()
            .map(|(key, value)| (key.path_buf.to_string_lossy().to_string(), value.to_owned()))
            .collect();
        Self { inner }
    }
}

impl PrintAsMap {
    pub fn get_map_padding(&self) -> usize {
        self.inner.keys().max_by_key(|key| key.len()).map_or_else(
            || QUOTATION_MARKS_LEN,
            |key| key.len() + QUOTATION_MARKS_LEN,
        )
    }

    pub fn format(&self, config: &Config) -> String {
        let padding = self.get_map_padding();

        let write_out_buffer = self
            .inner
            .iter()
            .filter(|(_key, values)| {
                if config.opt_last_snap.is_some() {
                    !values.is_empty()
                } else {
                    true
                }
            })
            .map(|(key, values)| {
                let display_path = if matches!(config.print_mode, PrintMode::FormattedNotPretty) {
                    key.to_owned()
                } else {
                    format!("\"{}\"", key)
                };

                let values_string: String = values
                    .iter()
                    .enumerate()
                    .map(|(idx, value)| {
                        if matches!(config.print_mode, PrintMode::FormattedNotPretty) {
                            format!("{}{}", NOT_SO_PRETTY_FIXED_WIDTH_PADDING, value)
                        } else if idx == 0 {
                            format!(
                                "{:<width$} : \"{}\"\n",
                                display_path,
                                value,
                                width = padding
                            )
                        } else {
                            format!("{:<width$} : \"{}\"\n", "", value, width = padding)
                        }
                    })
                    .collect::<String>();

                if matches!(config.print_mode, PrintMode::FormattedNotPretty) {
                    format!("{}:{}\n", display_path, values_string)
                } else {
                    values_string
                }
            })
            .collect();

        write_out_buffer
    }
}
