// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Stopping a run part way.
//!
//! `dukovo-lab stop` leaves a request in the store (see store.rs); the
//! run notices it within a fifth of a second and sets its [`Stop`], which
//! every long wait checks: waiting for serial text, for SSH, for a
//! command, for a machine to power off, for a person. Each of them then
//! ends early, the test it was in is skipped as stopped, and so is every
//! test after it.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// Set once a run is asked to stop. Clones share the one flag.
#[derive(Clone, Debug, Default)]
pub struct Stop(Arc<AtomicBool>);

impl Stop {
    /// Asks every wait that checks this to end.
    pub fn request(&self) {
        self.0.store(true, Ordering::Relaxed);
    }

    /// Whether the run has been asked to stop.
    pub fn requested(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
}
