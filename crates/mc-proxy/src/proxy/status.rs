//! Server-list ping response construction.

use crate::protocol::status::StatusResponse;
use crate::proxy::Shared;

/// Build the JSON document answered to a server-list ping.
pub fn build_status(shared: &Shared, protocol: i32) -> String {
    let (sample, online) = {
        let players = shared.players.lock().unwrap_or_else(|e| e.into_inner());
        (players.clone(), players.len())
    };
    StatusResponse {
        version_name: shared.config.version_name.clone(),
        protocol,
        max_players: shared.config.max_players,
        online_players: online,
        sample,
        motd: shared.config.motd.clone(),
        favicon: shared.favicon.clone(),
        enforces_secure_chat: shared.config.online_mode,
    }
    .to_json()
}
