/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under both the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree and the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree.
 */

use std::collections::HashSet;
use std::sync::Mutex;

use buck2_core::env_helper::EnvHelper;
use buck2_events::trace::TraceId;
use gazebo::dupe::Dupe;
use itertools::Itertools;
use once_cell::sync::Lazy;

static ACTIVE_COMMANDS: Lazy<Mutex<HashSet<TraceId>>> = Lazy::new(|| Mutex::new(HashSet::new()));

/// Return the active commands, if you know what they are
pub fn active_commands() -> Option<HashSet<TraceId>> {
    // Note that this function is accessed during panic, so have to be super careful
    Some(ACTIVE_COMMANDS.try_lock().ok()?.clone())
}

pub struct ActiveCommandDropGuard {
    trace_id: TraceId,
}

impl ActiveCommandDropGuard {
    pub fn new(trace_id: TraceId) -> Self {
        let result = {
            // Scope the guard so it's locked as little as possible
            let mut active_commands = ACTIVE_COMMANDS.lock().unwrap();
            active_commands.insert(trace_id.dupe());

            if active_commands.len() > 1 {
                Some(active_commands.clone())
            } else {
                None
            }
        };

        if let Some(commands) = result {
            static ERROR_CONCURRENT: EnvHelper<bool> = EnvHelper::new("BUCK2_ERROR_CONCURRENT");

            let message = format!(
                "Warning! Concurrent commands detected! Concurrent commands are not supported and likely results in crashes and incorrect builds.\n    Currently running commands are `{}`",
                commands
                    .iter()
                    .map(|id| format!("https://www.internalfb.com/buck2/{}", id))
                    .join(" ")
            );

            if ERROR_CONCURRENT.get_copied().ok() == Some(Some(true)) {
                panic!("{}", message);
            } else {
                // we use eprintln here on purpose so that this message goes to ALL commands, since
                // concurrent commands can affect correctness of ALL commands.
                eprintln!("{}", message);
            }
        }
        Self { trace_id }
    }
}

impl Drop for ActiveCommandDropGuard {
    fn drop(&mut self) {
        ACTIVE_COMMANDS.lock().unwrap().remove(&self.trace_id);
    }
}
