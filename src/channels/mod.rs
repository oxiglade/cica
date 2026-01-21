pub mod telegram;


/// Information about a channel for display purposes
pub struct ChannelInfo {
    pub name: &'static str,
    pub display_name: &'static str,
}

/// List of all supported channels
pub const SUPPORTED_CHANNELS: &[ChannelInfo] = &[
    ChannelInfo {
        name: "telegram",
        display_name: "Telegram",
    },
    // Future: discord, slack, etc.
];

/// Get channel info by name
pub fn get_channel_info(name: &str) -> Option<&'static ChannelInfo> {
    SUPPORTED_CHANNELS.iter().find(|c| c.name == name)
}
