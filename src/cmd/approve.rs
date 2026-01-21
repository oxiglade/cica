use anyhow::Result;
use tracing::info;

use crate::channels;
use crate::pairing::PairingStore;

/// Run the approve command
pub fn run(code: &str) -> Result<()> {
    let mut store = PairingStore::load()?;

    let request = store.approve(code)?;

    let channel_display = channels::get_channel_info(&request.channel)
        .map(|c| c.display_name)
        .unwrap_or(&request.channel);

    let user_display = request
        .display_name
        .as_ref()
        .or(request.username.as_ref())
        .map(|s| s.as_str())
        .unwrap_or(&request.user_id);

    println!("Approved {} user: {}", channel_display, user_display);

    info!(
        "Approved {} user {} ({})",
        request.channel, request.user_id, user_display
    );

    Ok(())
}
