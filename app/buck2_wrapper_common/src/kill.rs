/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under both the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree and the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree.
 */

//! Cross-platform process killing.

use std::time::Duration;

use sysinfo::Process;

use crate::pid::Pid;
#[cfg(unix)]
use crate::unix::kill as imp;
#[cfg(windows)]
use crate::win::kill as imp;

pub fn process_creation_time(process: &Process) -> Option<Duration> {
    imp::process_creation_time(process)
}

pub fn process_exists(pid: Pid) -> anyhow::Result<bool> {
    imp::process_exists(pid)
}

/// Send `KILL` or call `TerminateProcess` on the given process.
///
/// Returns a KilledProcessHandle that can be used to observe the termination of the killed process.
pub fn kill(pid: Pid) -> anyhow::Result<Option<KilledProcessHandle>> {
    match imp::kill(pid)? {
        Some(handle) => Ok(Some(KilledProcessHandle { handle })),
        None => Ok(None),
    }
}

pub struct KilledProcessHandle {
    handle: imp::KilledProcessHandleImpl,
}

impl KilledProcessHandle {
    pub fn has_exited(&self) -> anyhow::Result<bool> {
        self.handle.has_exited()
    }

    pub fn status(&self) -> Option<String> {
        self.handle.status()
    }
}

/// Get the status of a given process according to sysinfo.
pub fn get_sysinfo_status(pid: Pid) -> Option<String> {
    use sysinfo::PidExt;
    use sysinfo::ProcessExt;
    use sysinfo::ProcessRefreshKind;
    use sysinfo::System;
    use sysinfo::SystemExt;

    let pid = sysinfo::Pid::from_u32(pid.to_u32());

    let mut system = System::new();
    system.refresh_process_specifics(pid, ProcessRefreshKind::new());

    let proc = system.process(pid)?;
    Some(proc.status().to_string())
}

#[cfg(test)]
mod tests {
    use std::time::Duration;
    use std::time::Instant;

    use buck2_util::process::background_command;

    use crate::kill::kill;
    use crate::kill::process_exists;
    use crate::pid::Pid;

    #[test]
    fn test_process_exists_kill() {
        let mut command = if !cfg!(windows) {
            let mut command = background_command("sh");
            command.args(["-c", "sleep 10000"]);
            command
        } else {
            let mut command = background_command("powershell");
            command.args(["-c", "Start-Sleep -Seconds 10000"]);
            command
        };
        let mut child = command.spawn().unwrap();
        let pid = Pid::from_u32(child.id()).unwrap();
        for _ in 0..5 {
            assert!(process_exists(pid).unwrap());
        }

        let handle = kill(pid).unwrap().unwrap();

        child.wait().unwrap();
        // Drop child to ensure the Windows handle is closed.
        drop(child);

        if !cfg!(windows) {
            assert!(handle.has_exited().unwrap());
        } else {
            let start = Instant::now();
            loop {
                if handle.has_exited().unwrap() {
                    break;
                }
                assert!(
                    start.elapsed() < Duration::from_secs(20),
                    "Timed out waiting for process to die"
                );
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    }
}
