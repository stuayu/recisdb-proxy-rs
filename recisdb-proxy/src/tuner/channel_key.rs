//! Channel identification key for tuner sharing.

use std::hash::{Hash, Hasher};

/// A unique key identifying a tuner/channel combination.
///
/// When multiple clients tune to the same channel on the same tuner,
/// they can share the same tuner instance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelKey {
    /// Path to the tuner device.
    pub tuner_path: String,
    /// Channel specification (varies by channel type).
    pub channel: ChannelKeySpec,
}

/// Channel specification for the key.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ChannelKeySpec {
    /// IBonDriver v1 style: single channel number.
    Simple(u8),
    /// IBonDriver v2 style: space and channel.
    SpaceChannel { space: u32, channel: u32 },
}

impl Hash for ChannelKey {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.tuner_path.hash(state);
        self.channel.hash(state);
    }
}

impl ChannelKey {
    /// Create a key from tuner path and simple channel number.
    pub fn simple(tuner_path: impl Into<String>, channel: u8) -> Self {
        Self {
            tuner_path: tuner_path.into(),
            channel: ChannelKeySpec::Simple(channel),
        }
    }

    /// Create a key from tuner path and space/channel.
    pub fn space_channel(tuner_path: impl Into<String>, space: u32, channel: u32) -> Self {
        Self {
            tuner_path: tuner_path.into(),
            channel: ChannelKeySpec::SpaceChannel { space, channel },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn test_channel_key_equality() {
        let k1 = ChannelKey::simple("/dev/pt3video0", 13);
        let k2 = ChannelKey::simple("/dev/pt3video0", 13);
        let k3 = ChannelKey::simple("/dev/pt3video0", 14);
        let k4 = ChannelKey::simple("/dev/pt3video1", 13);

        assert_eq!(k1, k2);
        assert_ne!(k1, k3);
        assert_ne!(k1, k4);
    }

    #[test]
    fn test_channel_key_in_hashmap() {
        let mut map = HashMap::new();
        let key = ChannelKey::space_channel("/dev/pt3video0", 0, 5);
        map.insert(key.clone(), 42);

        assert_eq!(map.get(&key), Some(&42));
    }
}
