/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under both the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree and the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree.
 */

use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use anyhow::Context;
use buck2_client_ctx::client_ctx::ClientCommandContext;
use buck2_client_ctx::common::CommonCommandOptions;
use buck2_client_ctx::daemon::client::BuckdLifecycleLock;
use buck2_client_ctx::exit_result::ExitResult;
use buck2_client_ctx::final_console::FinalConsole;
use buck2_client_ctx::startup_deadline::StartupDeadline;
use buck2_client_ctx::streaming::BuckSubcommand;
use buck2_common::argv::Argv;
use buck2_common::argv::SanitizedArgv;
use buck2_common::daemon_dir::DaemonDir;
use buck2_core::fs::fs_util;
use buck2_core::fs::paths::abs_norm_path::AbsNormPathBuf;
use buck2_core::fs::paths::abs_path::AbsPath;
use buck2_core::fs::paths::abs_path::AbsPathBuf;
use dupe::Dupe;
use gazebo::prelude::SliceExt;
use humantime;
use threadpool::ThreadPool;
use walkdir::WalkDir;

use crate::commands::clean_stale::parse_clean_stale_args;
use crate::commands::clean_stale::CleanStaleCommand;
use crate::commands::kill::kill_command_impl;

/// Delete generated files and caches.
///
/// The command also kills the buck2 daemon.
#[derive(Debug, clap::Parser)]
pub struct CleanCommand {
    #[clap(flatten)]
    common_opts: CommonCommandOptions,

    #[clap(
        long = "dry-run",
        help = "Performs a dry-run and prints the paths that would be removed."
    )]
    dry_run: bool,

    #[clap(
        long = "stale",
        help = "Delete artifacts from buck-out older than 1 week or older than
the specified duration, without killing the daemon",
        value_name = "DURATION"
    )]
    stale: Option<Option<humantime::Duration>>,

    // Like stale but since a specific timestamp, for testing
    #[clap(long = "keep-since-time", conflicts_with = "stale", hidden = true)]
    keep_since_time: Option<i64>,

    /// Only considers tracked artifacts for cleanup.
    ///
    /// `buck-out` can contain untracked artifacts for different reasons:
    ///  - Outputs from aborted actions
    ///  - State getting deleted (e.g., new buckversion that changes the on-disk state format)
    ///  - Writing to `buck-out` without being expected by Buck
    #[clap(long = "tracked-only", requires = "stale")]
    tracked_only: bool,
}

impl CleanCommand {
    pub fn exec(self, matches: &clap::ArgMatches, ctx: ClientCommandContext<'_>) -> ExitResult {
        if let Some(keep_since_arg) = parse_clean_stale_args(self.stale, self.keep_since_time)? {
            let cmd = CleanStaleCommand {
                common_opts: self.common_opts,
                keep_since_arg,
                dry_run: self.dry_run,
                tracked_only: self.tracked_only,
            };
            return cmd.exec(matches, ctx);
        }

        ctx.instant_command("clean", async move |ctx| {
            let buck_out_dir = ctx.paths()?.buck_out_path();
            let daemon_dir = ctx.paths()?.daemon_dir()?;
            let console = &self.common_opts.console_opts.final_console();

            if self.dry_run {
                return clean(buck_out_dir, daemon_dir, console, None).await;
            }

            // Kill the daemon and make sure a new daemon does not spin up while we're performing clean up operations
            // This will ensure we have exclusive access to the directories in question
            let lifecycle_lock = BuckdLifecycleLock::lock_with_timeout(
                daemon_dir.clone(),
                StartupDeadline::duration_from_now(Duration::from_secs(10))?,
            )
            .await
            .with_context(|| "Error locking buckd lifecycle.lock")?;

            kill_command_impl(&lifecycle_lock, "`buck2 clean` was invoked").await?;

            clean(buck_out_dir, daemon_dir, console, Some(&lifecycle_lock)).await
        })
    }

    pub fn sanitize_argv(&self, argv: Argv) -> SanitizedArgv {
        argv.no_need_to_sanitize()
    }
}

async fn clean(
    buck_out_dir: AbsNormPathBuf,
    daemon_dir: DaemonDir,
    console: &FinalConsole,
    // None means "dry run".
    lifecycle_lock: Option<&BuckdLifecycleLock>,
) -> anyhow::Result<()> {
    let mut paths_to_clean = Vec::new();
    // Try to clean EdenFS based buck-out first. For EdenFS based buck-out, "eden rm"
    // is efficient. Notice eden rm will remove the buck-out root directory,
    // but for the native fs, the buck-out root directory is kept.
    if let Some(paths) = try_clean_eden_buck_out(&buck_out_dir, lifecycle_lock.is_none()).await? {
        paths_to_clean = paths;
    } else if buck_out_dir.exists() {
        paths_to_clean =
            collect_paths_to_clean(&buck_out_dir)?.map(|path| path.display().to_string());
        if lifecycle_lock.is_some() {
            tokio::task::spawn_blocking(move || clean_buck_out_with_retry(&buck_out_dir))
                .await?
                .context("Failed to spawn clean")?;
        }
    }

    if daemon_dir.path.exists() {
        paths_to_clean.push(daemon_dir.to_string());
        if let Some(lifecycle_lock) = lifecycle_lock {
            lifecycle_lock.clean_daemon_dir()?;
        }
    }

    for path in paths_to_clean {
        console.print_stderr(&path)?;
    }
    Ok(())
}

fn collect_paths_to_clean(buck_out_path: &AbsNormPathBuf) -> anyhow::Result<Vec<AbsNormPathBuf>> {
    let mut paths_to_clean = vec![];
    let dir = fs_util::read_dir(buck_out_path)?;
    for entry in dir {
        let entry = entry?;
        let path = entry.path();
        paths_to_clean.push(path);
    }

    Ok(paths_to_clean)
}

/// In Windows, we've observed the buck-out clean immediately after killing
/// the daemon can fail with this error: `The process cannot access the
/// file because it is being used by another process.`. To get around this,
/// add a single retry.
fn clean_buck_out_with_retry(path: &AbsNormPathBuf) -> anyhow::Result<()> {
    let mut result = clean_buck_out(path);
    match result {
        Ok(_) => {
            return result;
        }
        Err(e) => {
            tracing::info!(
                "Retrying buck-out clean, first attempted failed with: {:#}",
                e
            );
            result = clean_buck_out(path);
        }
    }
    result
}

fn clean_buck_out(path: &AbsNormPathBuf) -> anyhow::Result<()> {
    let walk = WalkDir::new(path);
    let thread_pool = ThreadPool::new(num_cpus::get());
    let error = Arc::new(Mutex::new(None));
    // collect dir paths to delete them after deleting files in them
    // we need reverse order to make sure the dir is already empty when
    // we delete it, otherwise remove would fail with DirNotEmpty exception
    let mut reverse_dir_paths = Vec::new();
    for dir_entry in walk.into_iter().flatten() {
        if dir_entry.file_type().is_dir() {
            // The walk gives us back absolute paths since we give it absolute paths.
            reverse_dir_paths.push(AbsPathBuf::new(dir_entry.into_path()).unwrap());
        } else {
            let error = error.dupe();
            thread_pool.execute(move || {
                // The wlak gives us back absolute paths since we give it absolute paths.
                let res = AbsPath::new(dir_entry.path())
                    .and_then(|p| fs_util::remove_file(p).map_err(Into::into));

                match res {
                    Ok(_) => {}
                    Err(e) => {
                        let mut error = error.lock().unwrap();
                        if error.is_none() {
                            *error = Some(e);
                        }
                    }
                }
            })
        }
    }

    thread_pool.join();
    if let Some(e) = error.lock().unwrap().take() {
        return Err(e);
    }

    // first entry is buck-out root dir and we don't want to remove it
    for path in reverse_dir_paths.iter().skip(1).rev() {
        fs_util::remove_dir(path)?;
    }

    Ok(())
}

#[cfg(fbcode_build)]
async fn try_clean_eden_buck_out(
    buck_out: &AbsNormPathBuf,
    dryrun: bool,
) -> anyhow::Result<Option<Vec<String>>> {
    use std::process::Stdio;

    use buck2_execute::materialize::eden_api::is_recas_eden_mount;
    use buck2_util::process::async_background_command;

    if !cfg!(unix) {
        return Ok(None);
    }

    if !is_recas_eden_mount(buck_out)? {
        return Ok(None);
    }

    // Run eden rm <buck-out> to rm a mount
    let mut eden_rm_cmd = async_background_command("eden");
    eden_rm_cmd
        .arg("rm")
        .arg("-y") // No promot
        .arg(buck_out.as_os_str())
        .current_dir("/")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    buck2_client_ctx::eprintln!(
        "The following command will be executed: `eden rm -y {}`",
        buck_out
    )?;

    if !dryrun {
        eden_rm_cmd
            .spawn()
            .context("Failed to start to remove EdenFS buck-out mount.")?
            .wait()
            .await
            .context("Failed to remove EdenFS buck-out mount.")?;

        // eden rm might not delete the buck-out completed.
        if buck_out.exists() {
            fs_util::remove_dir(buck_out)?;
        }
    }

    Ok(Some(vec![buck_out.display().to_string()]))
}

#[cfg(not(fbcode_build))]
async fn try_clean_eden_buck_out(
    _buck_out: &AbsNormPathBuf,
    _dryrun: bool,
) -> anyhow::Result<Option<Vec<String>>> {
    Ok(None)
}
