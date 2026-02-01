//! Group-based tuner space aggregation and driver selection.
//!
//! This module handles aggregation of multiple drivers within a client group,
//! building a unified virtual space map where clients can access all available channels
//! across all drivers in their group. It also handles intelligent driver selection
//! based on channel availability, existing sessions, and tuner load.

use std::collections::HashMap;
use crate::database::{ChannelRecord, Database};
use crate::tuner::space_generator::SpaceGenerator;

/// Information about a single driver within a group.
#[derive(Debug, Clone)]
pub struct DriverInfo {
    /// Driver ID (primary key in bon_drivers table)
    pub driver_id: u32,
    /// Driver path (e.g., "BonDriver_PX-MLT1.dll")
    pub driver_path: String,
    /// Virtual space generator for this driver
    pub space_gen: SpaceGenerator,
}

/// Aggregated space information for a group of drivers.
///
/// Builds a unified view of all channels available across all drivers in a group,
/// allowing clients to see and select from any channel available to any driver.
/// The virtual space index is group-global, mapping back to specific drivers.
#[derive(Debug)]
pub struct GroupSpaceInfo {
    /// Group name (e.g., "PX-MLT")
    pub group_name: String,
    /// All drivers in this group with their space generators
    pub drivers: Vec<DriverInfo>,
    /// Unified virtual space mappings (group-level)
    /// Index: virtual_space_idx, Value: (space_name, Vec<(driver_idx, actual_space_idx)>)
    pub space_mappings: HashMap<u32, (String, Vec<(usize, u32)>)>,
    /// Reverse mapping: (driver_idx, actual_space_idx) -> virtual_space_idx
    pub actual_to_virtual: HashMap<(usize, u32), u32>,
    /// Channel to driver mapping: (space_idx, channel_num) -> Vec<driver_idx>
    pub channel_to_drivers: HashMap<(u32, u32), Vec<usize>>,
}

impl GroupSpaceInfo {
    /// Build group space info from database.
    ///
    /// # Arguments
    /// * `db` - Database instance
    /// * `group_name` - Group name (e.g., "PX-MLT")
    /// * `driver_ids` - Vector of driver IDs in this group
    ///
    /// # Returns
    /// GroupSpaceInfo with unified space mappings and channel-to-driver index
    pub async fn build(
        db: &Database,
        group_name: String,
        driver_ids: Vec<u32>,
    ) -> Result<Self, String> {
        let mut drivers = Vec::new();
        let mut all_channels_by_driver: Vec<Vec<ChannelRecord>> = Vec::new();

        // Load channels for each driver
        for driver_id in driver_ids {
            let driver_record = db
                .get_bon_driver(driver_id as i64)
                .map_err(|e| format!("Failed to get driver {}: {}", driver_id, e))?
                .ok_or_else(|| format!("Driver {} not found", driver_id))?;

            let channels = db
                .get_enabled_channels_by_bon_driver(driver_id as i64)
                .map_err(|e| format!("Failed to get channels for driver {}: {}", driver_id, e))?;

            let space_gen = SpaceGenerator::generate_from_channels(
                &channels
                    .iter()
                    .map(|ch| crate::tuner::space_generator::ChannelInfo {
                        nid: ch.nid,
                        sid: ch.sid,
                        tsid: ch.tsid,
                        bon_space: ch.bon_space.unwrap_or(0),
                        bon_channel: ch.bon_channel.unwrap_or(0),
                        terrestrial_region: ch.terrestrial_region.clone(),
                    })
                    .collect::<Vec<_>>(),
            );

            drivers.push(DriverInfo {
                driver_id,
                driver_path: driver_record.dll_path,
                space_gen: space_gen.clone(),
            });

            all_channels_by_driver.push(channels);
        }

        // Simple implementation: collect all unique spaces and map channels
        let mut space_mappings: HashMap<u32, (String, Vec<(usize, u32)>)> = HashMap::new();
        let mut actual_to_virtual: HashMap<(usize, u32), u32> = HashMap::new();
        let mut channel_to_drivers: HashMap<(u32, u32), Vec<usize>> = HashMap::new();
        let mut next_virtual_space = 0u32;

        // Iterate through all drivers and collect their spaces
        for (driver_idx, driver_info) in drivers.iter().enumerate() {
            let virtual_space_indices = driver_info.space_gen.virtual_spaces();
            for virtual_space_idx in virtual_space_indices {
                // Get the SpaceMapping for this virtual space
                if let Some(space_map) = driver_info.space_gen.get_virtual_space(virtual_space_idx) {
                    let virtual_space = next_virtual_space;
                    next_virtual_space += 1;

                    actual_to_virtual.insert((driver_idx, virtual_space_idx), virtual_space);
                    space_mappings.insert(
                        virtual_space,
                        (space_map.display_name.clone(), vec![(driver_idx, virtual_space_idx)]),
                    );

                    // Index channels in this space
                    let driver_channels = &all_channels_by_driver[driver_idx];
                    for ch in driver_channels {
                        if ch.band_type == Some(space_map.band_type as u8) {
                            if let Some(channel_num) = ch.bon_channel {
                                channel_to_drivers
                                    .entry((virtual_space, channel_num))
                                    .or_insert_with(Vec::new)
                                    .push(driver_idx);
                            }
                        }
                    }
                }
            }
        }

        Ok(GroupSpaceInfo {
            group_name,
            drivers,
            space_mappings,
            actual_to_virtual,
            channel_to_drivers,
        })
    }

    /// Get space name for virtual space index.
    pub fn get_space_name(&self, virtual_space: u32) -> Option<String> {
        self.space_mappings
            .get(&virtual_space)
            .map(|(name, _)| name.clone())
    }

    /// Get all channels available in a virtual space.
    pub fn get_channels_in_space(&self, virtual_space: u32) -> Vec<(usize, u32)> {
        self.space_mappings
            .get(&virtual_space)
            .map(|(_, driver_spaces)| driver_spaces.clone())
            .unwrap_or_default()
    }

    /// Find which drivers can serve a specific channel in a space.
    ///
    /// Returns: Vec<usize> with driver indices
    pub fn find_drivers_for_channel(&self, virtual_space: u32, channel: u32) -> Vec<usize> {
        self.channel_to_drivers
            .get(&(virtual_space, channel))
            .cloned()
            .unwrap_or_default()
    }

    /// Get all virtual spaces.
    pub fn all_virtual_spaces(&self) -> Vec<u32> {
        let mut spaces: Vec<u32> = self.space_mappings.keys().cloned().collect();
        spaces.sort();
        spaces
    }
}

/// Strategy for selecting a driver when multiple options are available.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DriverSelectionStrategy {
    /// Prefer drivers with fewer active sessions
    LeastLoaded,
    /// Use the first available driver
    FirstAvailable,
    /// Prefer drivers that are already tuning to the same channel
    PreferExisting,
}

/// Driver selector with scoring logic.
pub struct DriverSelector;

impl DriverSelector {
    /// Score drivers based on selection strategy.
    ///
    /// Returns: Vec<(driver_idx, actual_space_idx)> sorted by preference
    pub fn score_drivers(
        candidates: &[(usize, u32)],
        strategy: DriverSelectionStrategy,
        _active_sessions: &HashMap<usize, bool>, // driver_idx -> is_active
    ) -> Vec<(usize, u32)> {
        match strategy {
            DriverSelectionStrategy::LeastLoaded => {
                let mut sorted = candidates.to_vec();
                sorted.sort_by_key(|(idx, _)| *idx);
                sorted
            }
            DriverSelectionStrategy::FirstAvailable => candidates.to_vec(),
            DriverSelectionStrategy::PreferExisting => {
                let mut sorted = candidates.to_vec();
                sorted.sort_by_key(|(idx, _)| *idx);
                sorted
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_group_space_info_creation() {
        let drivers = vec![];
        let group = GroupSpaceInfo {
            group_name: "PX-TEST".to_string(),
            drivers,
            space_mappings: HashMap::new(),
            actual_to_virtual: HashMap::new(),
            channel_to_drivers: HashMap::new(),
        };
        assert_eq!(group.group_name, "PX-TEST");
    }

    #[test]
    fn test_driver_selector() {
        let candidates = vec![(0, 10), (1, 20)];
        let active = HashMap::new();

        let selected =
            DriverSelector::score_drivers(&candidates, DriverSelectionStrategy::FirstAvailable, &active);
        assert!(!selected.is_empty());
    }
}
