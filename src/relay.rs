//! Push-relay seam. The relay client is NOT implemented yet; this module fixes
//! the interface the rest of the app already uses, so adding it later touches
//! only this file. Design and wire protocol: docs/relay-architecture.md.
//!
//! Contract with the Teams page:
//! - `healthy()` true  => relay connected and heartbeating. Polling drops to a
//!   slow safety-net interval and relay events trigger immediate refreshes.
//! - `healthy()` false => relay absent, down or silent. Adaptive polling runs
//!   at full speed (fallback). Flipping back needs no other change.
#![allow(dead_code)] // set_healthy / PushEvent variants are used by the future client

use std::sync::atomic::{AtomicBool, Ordering};

static HEALTHY: AtomicBool = AtomicBool::new(false);

/// Events the relay can push. They carry ids only, never message content.
pub enum PushEvent {
    /// A message was created or changed in this chat.
    Message { chat_id: String },
    /// Notifications may have been missed (reconnect, lifecycle "missed"): refresh everything.
    Resync,
}

/// Relay base URL from `PENGUIN_MS_RELAY_URL`, if configured.
pub fn url() -> Option<String> {
    std::env::var("PENGUIN_MS_RELAY_URL").ok().filter(|u| !u.is_empty())
}

/// True while the relay connection is alive (heartbeat seen recently).
pub fn healthy() -> bool {
    HEALTHY.load(Ordering::Relaxed)
}

/// For the relay client: call with true on connect/heartbeat, false on
/// disconnect or missed heartbeats (~45 s).
pub fn set_healthy(ok: bool) {
    HEALTHY.store(ok, Ordering::Relaxed);
}

/// Start the relay client and return its event stream. Not implemented:
/// returns None, so the app uses polling only.
pub fn start() -> Option<async_channel::Receiver<PushEvent>> {
    if let Some(u) = url() {
        eprintln!("relay configured ({u}) but the client is not implemented yet; using polling");
    }
    None
}
