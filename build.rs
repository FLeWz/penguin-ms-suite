//! Reads PENGUIN_MS_CLIENT_ID / PENGUIN_MS_TENANT from the process env or a
//! local `.env` file and compiles them into the binary, so no IDs live in source.
use std::{collections::HashMap, fs};

fn main() {
    println!("cargo:rerun-if-changed=.env");
    let mut vars: HashMap<String, String> = HashMap::new();
    if let Ok(s) = fs::read_to_string(".env") {
        for l in s.lines() {
            let l = l.trim();
            if l.starts_with('#') {
                continue;
            }
            if let Some((k, v)) = l.split_once('=') {
                vars.insert(k.trim().into(), v.trim().trim_matches('"').into());
            }
        }
    }
    // Optional: relay URL (public) and invite secret (extractable from the binary; prefer the
    // runtime variable or ~/.config/penguin-ms-suite/relay_invite for anything shared).
    for key in ["PENGUIN_MS_RELAY_URL", "PENGUIN_MS_RELAY_INVITE"] {
        println!("cargo:rerun-if-env-changed={key}");
        let val = std::env::var(key).ok().or_else(|| vars.get(key).cloned()).unwrap_or_default();
        println!("cargo:rustc-env=BUILTIN_{key}={val}");
    }
    for key in ["PENGUIN_MS_CLIENT_ID", "PENGUIN_MS_TENANT"] {
        println!("cargo:rerun-if-env-changed={key}");
        let val = std::env::var(key)
            .ok()
            .or_else(|| vars.get(key).cloned())
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| panic!("{key} missing: set it in .env (see .env.example) or the environment"));
        println!("cargo:rustc-env=BUILTIN_{key}={val}");
    }
}
