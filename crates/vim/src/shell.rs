//! Helix-style shell verbs (`|` pipe-selection, `Alt-|` pipe-to, `!`
//! insert-output, `Alt-!` append-output, `$` keep-pipe).
//!
//! The vim crate owns the action declarations and the per-selection
//! `$SHELL -c <cmd>` execution engine. The *prompt UX* lives in
//! `codon-command-palette` (the vim crate cannot depend on a codon
//! crate). The no-payload actions (`ShellPipeSelection`, etc.) toggle
//! the workspace command palette pre-filled with a verb mnemonic that
//! `codon-command-palette` registers as a completer; on confirm the
//! completer dispatches `vim::ShellRun { mode, cmd }`, which is the
//! shared worker that captures multi-cursor selections, spawns the
//! shell once per selection (bounded concurrency, configurable
//! timeout), and applies the per-mode result atomically.
//!
//! See `REQ:codon/shell-integration` and the four refining tasks under
//! `.specs/phase-16/shell-*.spec.md`.

use std::{ops::Range, process::Stdio, time::Duration};

use editor::{Anchor, Editor, SelectionEffects};
use futures::AsyncWriteExt as _;
use futures::future::{Either, join_all, select};
use gpui::{Action, Context, Task, Window};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use workspace::{Toast, notifications::NotificationId};

use crate::Vim;

/// How a single `vim::ShellRun` invocation applies its per-selection
/// results to the buffer / selection set.
#[derive(
    Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum ShellMode {
    /// `|` — stdin = selection, replace each selection with stdout.
    #[default]
    PipeReplace,
    /// `Alt-|` — stdin = selection, discard stdout (side-effect commands).
    PipeDiscard,
    /// `!` — no stdin, insert stdout before each selection.
    InsertBefore,
    /// `Alt-!` — no stdin, append stdout after each selection.
    AppendAfter,
    /// `$` — stdin = selection, keep only selections whose command
    /// exited 0. Buffer text unchanged.
    KeepIfZero,
}

gpui::actions!(
    vim,
    [
        /// Pipe each selection through `$SHELL -c <cmd>`; replace each
        /// selection with the command's stdout. Bound to `|`.
        ShellPipeSelection,
        /// Pipe each selection through `$SHELL -c <cmd>`; discard
        /// stdout. Used for side-effect commands (e.g. `pbcopy`).
        /// Bound to `Alt-|`.
        ShellPipeTo,
        /// Run `$SHELL -c <cmd>` with no stdin; insert stdout before
        /// each selection. Bound to `!`.
        ShellInsertOutput,
        /// Run `$SHELL -c <cmd>` with no stdin; append stdout after
        /// each selection. Bound to `Alt-!`.
        ShellAppendOutput,
        /// Pipe each selection through `$SHELL -c <cmd>`; keep only
        /// selections whose command exited 0. Bound to `$`.
        ShellKeepPipe,
    ]
);

/// Payload-bearing action dispatched by the codon-command-palette
/// completers (`:pipe`, `:pipe-to`, `:insert-output`, `:append-output`,
/// `:keep-pipe`) and by the keyboard actions once the user has
/// confirmed a command in the palette prompt.
#[derive(Clone, Debug, PartialEq, Default, Serialize, Deserialize, JsonSchema, Action)]
#[action(namespace = vim)]
pub struct ShellRun {
    pub mode: ShellMode,
    pub cmd: String,
}

/// Default per-process kill timeout. Mirrors
/// `REQ:codon/shell-integration#c-timeout-safety`. Configurable via
/// `codon.toml` is left to a follow-up task — for now this is a hard
/// constant so the kill path is always exercised.
const DEFAULT_TIMEOUT: Duration = Duration::from_millis(5000);

/// Maximum concurrent shell spawns per `ShellRun` invocation. Bounds
/// the explosion when a user selects "every line" and presses `|`.
const MAX_CONCURRENCY: usize = 8;

pub fn register(editor: &mut Editor, cx: &mut Context<Vim>) {
    Vim::action(editor, cx, |vim, _: &ShellPipeSelection, window, cx| {
        toggle_palette_prefill(vim, "pipe ", window, cx);
    });
    Vim::action(editor, cx, |vim, _: &ShellPipeTo, window, cx| {
        toggle_palette_prefill(vim, "pipe-to ", window, cx);
    });
    Vim::action(editor, cx, |vim, _: &ShellInsertOutput, window, cx| {
        toggle_palette_prefill(vim, "insert-output ", window, cx);
    });
    Vim::action(editor, cx, |vim, _: &ShellAppendOutput, window, cx| {
        toggle_palette_prefill(vim, "append-output ", window, cx);
    });
    Vim::action(editor, cx, |vim, _: &ShellKeepPipe, window, cx| {
        toggle_palette_prefill(vim, "keep-pipe ", window, cx);
    });
    Vim::action(editor, cx, |vim, action: &ShellRun, window, cx| {
        run_shell(vim, action.mode, action.cmd.clone(), window, cx);
    });
}

/// Open the workspace command palette with a verb mnemonic prefilled,
/// so codon-command-palette's matching completer takes over. We use a
/// leading `:` form so this works against both Zed's stock palette and
/// codon's palette (codon-command-palette strips the leading `:` for
/// alias lookup).
fn toggle_palette_prefill(
    vim: &mut Vim,
    prefill: &str,
    window: &mut Window,
    cx: &mut Context<Vim>,
) {
    let Some(workspace) = vim.workspace(window, cx) else {
        return;
    };
    let prefill = prefill.to_string();
    workspace.update(cx, |workspace, cx| {
        command_palette::CommandPalette::toggle(workspace, &prefill, window, cx);
    });
}

/// Public entry point used by codon-command-palette. Captures the
/// editor's selections, spawns `$SHELL -c <cmd>` per selection (bounded
/// concurrency, with per-process timeout), and applies the per-mode
/// result atomically when every spawn has settled.
pub fn run_shell(
    vim: &mut Vim,
    mode: ShellMode,
    cmd: String,
    window: &mut Window,
    cx: &mut Context<Vim>,
) {
    if cmd.trim().is_empty() {
        return;
    }
    let Some(workspace) = vim.workspace(window, cx) else {
        return;
    };
    let project = workspace.read(cx).project().clone();

    // Capture per-selection text and the anchor range each result will
    // apply to. The anchor range is computed *before* any edit so that
    // multi-cursor edits in the same buffer don't shift each other's
    // targets.
    let needs_stdin = matches!(
        mode,
        ShellMode::PipeReplace | ShellMode::PipeDiscard | ShellMode::KeepIfZero
    );
    let mut targets: Vec<ShellTarget> = Vec::new();
    vim.update_editor(cx, |_, editor, cx| {
        let snapshot = editor.buffer().read(cx).snapshot(cx);
        let display = editor.display_snapshot(cx);
        let selections = editor.selections.all_adjusted(&display);
        for selection in selections {
            let start_pt = selection.start;
            let end_pt = selection.end;
            let start = snapshot.anchor_before(start_pt);
            let end = snapshot.anchor_after(end_pt);
            let text: String = if needs_stdin {
                snapshot.text_for_range(start_pt..end_pt).collect()
            } else {
                String::new()
            };
            targets.push(ShellTarget { range: start..end, stdin: text });
        }
    });

    if targets.is_empty() {
        return;
    }

    let workspace_handle = workspace.downgrade();
    let task: Task<()> = cx.spawn_in(window, async move |vim, cx| {
        let mut results: Vec<ShellResult> = Vec::with_capacity(targets.len());
        // Bounded concurrency: chunk the targets into windows of
        // MAX_CONCURRENCY and await each chunk fully before starting
        // the next. Keeps per-process resource use predictable.
        for chunk in targets.chunks(MAX_CONCURRENCY) {
            let mut futs = Vec::with_capacity(chunk.len());
            for target in chunk {
                let project = project.clone();
                let cmd = cmd.clone();
                let stdin = target.stdin.clone();
                let with_stdin = needs_stdin;
                let exec_task = project
                    .update(cx, |project, cx| project.exec_in_shell(cmd, cx));
                futs.push(spawn_one(exec_task, stdin, with_stdin, cx.clone()));
            }
            for outcome in join_all(futs).await {
                results.push(outcome);
            }
        }

        // Apply per-mode semantics in a single editor transaction so
        // undo treats the batch atomically.
        let edits = build_edits(mode, &targets, &results);
        let kept_ranges = if mode == ShellMode::KeepIfZero {
            Some(kept_selection_ranges(&targets, &results))
        } else {
            None
        };
        let failure_count = results
            .iter()
            .filter(|r| !matches!(r, ShellResult::Ok { exit_code: 0, .. }))
            .count();
        let first_stderr = results.iter().find_map(|r| match r {
            ShellResult::Ok { stderr, exit_code, .. } if *exit_code != 0 && !stderr.is_empty() => {
                Some(stderr.clone())
            }
            ShellResult::SpawnError(message) => Some(message.clone()),
            ShellResult::Timeout => Some("timed out".to_string()),
            _ => None,
        });

        let _ = vim.update_in(cx, |vim, window, cx| {
            vim.update_editor(cx, |_, editor, cx| {
                editor.transact(window, cx, |editor, window, cx| {
                    if !edits.is_empty() {
                        editor.edit(edits, cx);
                    }
                    if let Some(ranges) = &kept_ranges {
                        if !ranges.is_empty() {
                            editor.change_selections(
                                SelectionEffects::no_scroll(),
                                window,
                                cx,
                                |s| s.select_anchor_ranges(ranges.clone()),
                            );
                        }
                    }
                });
            });
            // Toast for non-zero exits / spawn errors / timeouts. For
            // KeepIfZero, drops are expected — only toast on spawn
            // errors / timeouts, which we approximate by checking if
            // any result is a SpawnError / Timeout.
            let surface_toast = match mode {
                ShellMode::KeepIfZero => results.iter().any(|r| {
                    matches!(r, ShellResult::SpawnError(_) | ShellResult::Timeout)
                }) || kept_ranges.as_ref().map(|r| r.is_empty()).unwrap_or(false),
                _ => failure_count > 0,
            };
            if surface_toast {
                let _ = workspace_handle.update(cx, |workspace, cx| {
                    let message = match (mode, failure_count, kept_ranges.as_ref(), first_stderr) {
                        (ShellMode::KeepIfZero, _, Some(kept), _) if kept.is_empty() => {
                            "$: no selections matched; primary preserved".to_string()
                        }
                        (_, _, _, Some(err)) => {
                            let trimmed: String = err.chars().take(120).collect();
                            format!(
                                "shell: {failure_count} of {total} selections failed: {trimmed}",
                                total = targets.len()
                            )
                        }
                        _ => format!(
                            "shell: {failure_count} of {total} selections failed",
                            total = targets.len()
                        ),
                    };
                    workspace.show_toast(
                        Toast::new(NotificationId::unique::<ShellToastId>(), message),
                        cx,
                    );
                });
            }
        });
    });
    task.detach();
}

struct ShellToastId;

#[derive(Clone, Debug)]
struct ShellTarget {
    range: Range<Anchor>,
    stdin: String,
}

#[derive(Clone, Debug)]
enum ShellResult {
    Ok { exit_code: i32, stdout: String, stderr: String },
    SpawnError(String),
    Timeout,
}

async fn spawn_one(
    exec_task: Task<anyhow::Result<smol::process::Command>>,
    stdin_text: String,
    with_stdin: bool,
    cx: gpui::AsyncWindowContext,
) -> ShellResult {
    let mut process = match exec_task.await {
        Ok(p) => p,
        Err(e) => return ShellResult::SpawnError(e.to_string()),
    };
    process.stdout(Stdio::piped());
    process.stderr(Stdio::piped());
    if with_stdin {
        process.stdin(Stdio::piped());
    } else {
        process.stdin(Stdio::null());
    }
    let mut running = match process.spawn() {
        Ok(child) => child,
        Err(e) => return ShellResult::SpawnError(e.to_string()),
    };
    if with_stdin
        && let Some(mut stdin) = running.stdin.take()
    {
        // Write stdin in a side-task; if it fails (e.g. broken pipe
        // because the process exited early) we proceed to collect what
        // we have.
        let bytes = stdin_text.into_bytes();
        cx.background_executor()
            .spawn(async move {
                let _ = stdin.write_all(&bytes).await;
                let _ = stdin.flush().await;
            })
            .detach();
    }
    let timeout = cx.background_executor().timer(DEFAULT_TIMEOUT);
    let output_fut = Box::pin(running.output());
    let timeout_fut = Box::pin(timeout);
    match select(output_fut, timeout_fut).await {
        Either::Left((output, _)) => match output {
            Ok(out) => ShellResult::Ok {
                exit_code: out.status.code().unwrap_or(-1),
                stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
                stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
            },
            Err(e) => ShellResult::SpawnError(e.to_string()),
        },
        Either::Right(_) => ShellResult::Timeout,
    }
}

/// Build the per-selection edits for the given mode + outcomes. Empty
/// for modes that don't edit text (`PipeDiscard`, `KeepIfZero`) or
/// when every spawn failed.
fn build_edits(
    mode: ShellMode,
    targets: &[ShellTarget],
    results: &[ShellResult],
) -> Vec<(Range<Anchor>, String)> {
    if matches!(mode, ShellMode::PipeDiscard | ShellMode::KeepIfZero) {
        return Vec::new();
    }
    let mut edits = Vec::new();
    for (target, result) in targets.iter().zip(results.iter()) {
        let ShellResult::Ok { exit_code, stdout, .. } = result else {
            continue; // skip selections whose command failed
        };
        if *exit_code != 0 {
            continue;
        }
        let payload = strip_one_trailing_newline(stdout);
        match mode {
            ShellMode::PipeReplace => {
                edits.push((target.range.clone(), payload));
            }
            ShellMode::InsertBefore => {
                let zero = target.range.start..target.range.start;
                edits.push((zero, payload));
            }
            ShellMode::AppendAfter => {
                let zero = target.range.end..target.range.end;
                edits.push((zero, payload));
            }
            ShellMode::PipeDiscard | ShellMode::KeepIfZero => unreachable!(),
        }
    }
    edits
}

/// For `KeepIfZero`, return the anchor ranges of selections whose
/// command exited 0. Empty result is the caller's signal that no
/// selection survived (codon's "keep primary + toast" policy applies).
fn kept_selection_ranges(
    targets: &[ShellTarget],
    results: &[ShellResult],
) -> Vec<Range<Anchor>> {
    targets
        .iter()
        .zip(results.iter())
        .filter_map(|(t, r)| match r {
            ShellResult::Ok { exit_code: 0, .. } => Some(t.range.clone()),
            _ => None,
        })
        .collect()
}

/// Mirror Helix: strip one trailing `\n` (or `\r\n`) from the command's
/// stdout before inserting. Trailing newlines from `echo`-style
/// commands would otherwise blow up the buffer by one line per
/// invocation.
fn strip_one_trailing_newline(text: &str) -> String {
    if let Some(rest) = text.strip_suffix("\r\n") {
        rest.to_string()
    } else if let Some(rest) = text.strip_suffix('\n') {
        rest.to_string()
    } else {
        text.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_one_trailing_newline_unix() {
        assert_eq!(strip_one_trailing_newline("hello\n"), "hello");
        assert_eq!(strip_one_trailing_newline("hello\n\n"), "hello\n");
        assert_eq!(strip_one_trailing_newline("hello"), "hello");
    }

    #[test]
    fn strip_one_trailing_newline_windows() {
        assert_eq!(strip_one_trailing_newline("hello\r\n"), "hello");
    }

    #[test]
    fn build_edits_skips_failed_results() {
        // PipeReplace with one OK and one non-zero result yields one
        // edit (the OK selection) only.
        let targets = vec![
            ShellTarget { range: Anchor::Min..Anchor::Min, stdin: "a".to_string() },
            ShellTarget { range: Anchor::Min..Anchor::Min, stdin: "b".to_string() },
        ];
        let results = vec![
            ShellResult::Ok {
                exit_code: 0,
                stdout: "ok\n".to_string(),
                stderr: String::new(),
            },
            ShellResult::Ok {
                exit_code: 1,
                stdout: String::new(),
                stderr: "boom".to_string(),
            },
        ];
        let edits = build_edits(ShellMode::PipeReplace, &targets, &results);
        assert_eq!(edits.len(), 1);
        assert_eq!(edits[0].1, "ok");
    }

    #[test]
    fn build_edits_pipe_discard_and_keep_if_zero_return_no_edits() {
        let targets = vec![ShellTarget {
            range: Anchor::Min..Anchor::Min,
            stdin: String::new(),
        }];
        let results = vec![ShellResult::Ok {
            exit_code: 0,
            stdout: "ok\n".to_string(),
            stderr: String::new(),
        }];
        assert!(build_edits(ShellMode::PipeDiscard, &targets, &results).is_empty());
        assert!(build_edits(ShellMode::KeepIfZero, &targets, &results).is_empty());
    }

    #[test]
    fn kept_ranges_drop_non_zero_exits() {
        let targets = vec![
            ShellTarget { range: Anchor::Min..Anchor::Min, stdin: String::new() },
            ShellTarget { range: Anchor::Min..Anchor::Min, stdin: String::new() },
        ];
        let results = vec![
            ShellResult::Ok { exit_code: 0, stdout: String::new(), stderr: String::new() },
            ShellResult::Ok { exit_code: 1, stdout: String::new(), stderr: String::new() },
        ];
        assert_eq!(kept_selection_ranges(&targets, &results).len(), 1);
    }
}
