//! Global-state handoff protocol.
//!
//! The value produced by `#[global_setup]` is serialized to JSON in the
//! coordinator and handed to each test subprocess through an environment
//! variable. This module owns both ends of that protocol: [`StateHandoff`]
//! bundles the env-var name and the serialized JSON, [`StateHandoff::capture`]
//! builds one on the coordinator side, and [`StateHandoff::load`] reconstructs
//! the state on the subprocess side.
//!
//! No `tokio` dependency lives here — the handoff is pure data plus the two
//! (de)serialization hops through a [`GlobalSetupEntry`].

use std::any::Any;

use crate::registry::GlobalSetupEntry;

/// The serialized global state plus the environment variable it travels in.
///
/// Built once per suite by [`StateHandoff::capture`] and shared (via `Arc`)
/// into every spawned test task, which passes it to the subprocess runner.
pub(crate) struct StateHandoff {
    var: String,
    json: String,
}

impl StateHandoff {
    /// Capture the global state for subprocess handoff.
    ///
    /// `var` is `RIG_STATE_<suffix>` (the naming convention lives here); `json`
    /// is the serialized state when a `#[global_setup]` entry exists, or the
    /// empty string otherwise. `data` is the type-erased global-setup value
    /// (pass `&*global_data`); it is only inspected when `entry` is `Some`.
    pub(crate) fn capture(
        entry: Option<&GlobalSetupEntry>,
        data: &(dyn Any + Send + Sync),
        suffix: u64,
    ) -> Self {
        Self {
            var: format!("RIG_STATE_{suffix:016x}"),
            json: entry.map_or_else(String::new, |e| (e.serialize_fn)(data)),
        }
    }

    /// The environment variable name carrying the serialized state.
    pub(crate) fn var(&self) -> &str {
        &self.var
    }

    /// The serialized state JSON (empty when there is no `#[global_setup]`).
    pub(crate) fn json(&self) -> &str {
        &self.json
    }

    /// Reconstruct the global state inside a test subprocess.
    ///
    /// Reads `var` from the environment, removes it so the test function and
    /// any child processes cannot see it, then deserializes iff a
    /// `#[global_setup]` entry exists. With no entry the state is `()`,
    /// regardless of what the variable held.
    pub(crate) fn load(var: &str, entry: Option<&GlobalSetupEntry>) -> Box<dyn Any + Send + Sync> {
        let json = std::env::var(var).unwrap_or_default();
        // Remove the env var so it is not visible to the test function or any
        // child processes it might spawn.
        //
        // SAFETY: single-threaded at this point — the Tokio runtime has not
        // yet spawned any threads, and no other threads read this variable.
        unsafe { std::env::remove_var(var) };

        entry.map_or_else(|| Box::new(()) as _, |e| (e.deserialize_fn)(&json))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::{Deserialize, Serialize};

    #[derive(Serialize, Deserialize, PartialEq, Debug)]
    struct State {
        base_url: String,
        port: u16,
    }

    /// Fabricate a `GlobalSetupEntry` over the concrete `State` type, mirroring
    /// how the `#[global_setup]` macro wires its serialize/deserialize closures.
    fn state_entry() -> GlobalSetupEntry {
        GlobalSetupEntry::new(
            || Box::pin(async { Box::new(()) as Box<dyn Any + Send + Sync> }),
            |boxed| {
                let concrete = boxed.downcast_ref::<State>().expect("type mismatch");
                serde_json::to_string(concrete).expect("serialize")
            },
            |s| Box::new(serde_json::from_str::<State>(s).expect("deserialize")) as _,
        )
    }

    #[test]
    fn round_trips_state_through_env() {
        let entry = state_entry();
        let data = State {
            base_url: "http://localhost".into(),
            port: 8080,
        };

        // Fixed, test-unique suffix so the env var cannot collide with a
        // concurrently running test.
        let handoff = StateHandoff::capture(Some(&entry), &data, 0x00d1_0001);
        assert!(!handoff.json().is_empty());

        // SAFETY: env mutation is a footgun across threads, but this var name
        // is unique to this test (a fixed, test-only suffix) and is read
        // nowhere else, so no other thread observes it.
        unsafe { std::env::set_var(handoff.var(), handoff.json()) };

        let loaded = StateHandoff::load(handoff.var(), Some(&entry));
        let recovered = loaded.downcast_ref::<State>().expect("downcast");
        assert_eq!(recovered, &data);

        // `load` must have removed the var.
        assert!(std::env::var(handoff.var()).is_err());
    }

    // A distinct fixed suffix keeps this test's var name from colliding with
    // the round-trip test's.
    fn no_setup_suffix() -> u64 {
        0x00d1_0002
    }

    #[test]
    fn no_setup_yields_empty_json_and_unit() {
        let handoff = StateHandoff::capture(None, &(), no_setup_suffix());
        assert_eq!(handoff.json(), "");

        // SAFETY: env mutation is a footgun across threads, but this var name
        // is unique to this test (a fixed, test-only suffix) and is read
        // nowhere else, so no other thread observes it.
        unsafe { std::env::set_var(handoff.var(), "ignored") };

        let loaded = StateHandoff::load(handoff.var(), None);
        assert!(loaded.downcast_ref::<()>().is_some());
        assert!(std::env::var(handoff.var()).is_err());
    }

    #[test]
    fn var_has_rig_state_prefix_and_16_hex() {
        let handoff = StateHandoff::capture(None, &(), 0xdead_beef);
        let var = handoff.var();
        let hex = var.strip_prefix("RIG_STATE_").expect("RIG_STATE_ prefix");
        assert_eq!(hex.len(), 16);
        assert!(hex.chars().all(|c| c.is_ascii_hexdigit()));
    }
}
