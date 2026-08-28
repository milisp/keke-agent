//! One-shot, non-interactive turns.

use std::io::Read as _;
use std::io::Write as _;

use anyhow::Context as _;
use anyhow::Result;
use anyhow::bail;
use keke_config::Config;
use keke_core::TurnUpdate;
use keke_protocol::Message;
use keke_protocol::StopReason;

use super::session_builder;
use crate::cli::ExecArgs;
use crate::cli::ExecFormat;
use crate::compose::Composed;
use crate::ui::is_interactive;

pub(super) async fn exec(
    args: ExecArgs,
    config: Config,
    composed: Composed,
    cwd: std::path::PathBuf,
) -> Result<()> {
    let prompt = match args.prompt {
        Some(prompt) => prompt,
        None => {
            let mut buffer = String::new();
            std::io::stdin()
                .read_to_string(&mut buffer)
                .context("reading the prompt from stdin")?;
            buffer
        }
    };
    if prompt.trim().is_empty() {
        bail!("no prompt given; pass one as an argument or on stdin");
    }

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let builder = session_builder(
        &config,
        &composed,
        cwd,
        args.approval.unwrap_or(config.approval_policy),
    )
    .await?
    .updates(tx);

    let mut session = builder.build().await?;
    let log_path = session.log_path().to_path_buf();

    // Ctrl-C cancels the turn rather than killing the process, so the rollout
    // log is closed cleanly and a partially written file is not left behind.
    let canceller = session.canceller();
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            eprintln!("\ncancelling…");
            canceller();
        }
    });

    // JSON mode never streams partial text: it waits for the full reply and
    // emits one object. Text mode streams when connected to a terminal. Tool
    // call progress always goes to stderr so it stays visible in both modes.
    let streaming = args.format == ExecFormat::Text && is_interactive();
    let renderer = tokio::spawn(async move {
        let mut out = std::io::stdout();
        while let Some(update) = rx.recv().await {
            match update {
                TurnUpdate::TextDelta { delta, .. } if streaming => {
                    if write!(out, "{delta}").is_err() {
                        // Broken pipe: stop writing, do not panic.
                        break;
                    }
                    let _ = out.flush();
                }
                TurnUpdate::ToolCallStarted { call } => {
                    eprintln!("· {}", call.name);
                }
                _ => {}
            }
        }
    });

    let outcome = session.run_turn(Message::user(prompt)).await;
    drop(session);
    let _ = renderer.await;

    match args.format {
        ExecFormat::Text => {
            let outcome = outcome?;
            if !streaming {
                // Piped output gets the answer alone, with no interleaved progress.
                if let Some(message) = &outcome.message {
                    println!("{}", message.text());
                }
            } else {
                println!();
            }
            if args.print_log_path {
                eprintln!("log: {}", log_path.display());
            }
            match outcome.stop_reason {
                StopReason::Refusal { message } => bail!("the model refused: {message}"),
                StopReason::Cancelled => bail!("cancelled"),
                _ => Ok(()),
            }
        }
        ExecFormat::Json => {
            let outcome = match outcome {
                Ok(o) => o,
                Err(error) => {
                    // Surface the engine error as a JSON line so a script that
                    // parses stdout sees a consistent shape whether the turn
                    // succeeded or failed.
                    let obj = serde_json::json!({"type": "error", "message": error.to_string()});
                    emit_json(&obj);
                    return Err(error.into());
                }
            };

            let mut obj = serde_json::json!({
                "text": outcome.message.as_ref().map(|m| m.text()).unwrap_or_default(),
                "stopReason": stop_reason_wire(&outcome.stop_reason),
                "usage": {
                    "inputTokens": outcome.usage.input_tokens,
                    "outputTokens": outcome.usage.output_tokens,
                    "cachedInputTokens": outcome.usage.cached_input_tokens,
                    "reasoningTokens": outcome.usage.reasoning_tokens,
                },
            });
            if args.print_log_path {
                obj["log"] = serde_json::Value::String(log_path.display().to_string());
            }
            emit_json(&obj);

            match outcome.stop_reason {
                StopReason::Refusal { message } => bail!("the model refused: {message}"),
                StopReason::Cancelled => bail!("cancelled"),
                _ => Ok(()),
            }
        }
    }
}

/// Map a stop reason to its snake_case wire token.
///
/// Explicit mapping rather than serde so the token is a deliberate contract,
/// not an accident of how the field happens to be serialized right now. An
/// unknown future variant logs a warning and falls back to `end_turn` rather
/// than propagating a deserialization shape that callers have not seen.
fn stop_reason_wire(reason: &StopReason) -> &'static str {
    match reason {
        StopReason::EndTurn => "end_turn",
        StopReason::ToolUse => "tool_use",
        StopReason::MaxTokens => "max_tokens",
        StopReason::Cancelled => "cancelled",
        StopReason::Refusal { .. } => "refusal",
    }
}

/// Write a compact JSON object to stdout followed by a newline.
///
/// Broken pipe is treated as a clean stop: the caller already exited, and
/// panicking here would leave the rollout log open.
fn emit_json(value: &serde_json::Value) {
    use std::io::Write as _;
    let rendered = match serde_json::to_string_pretty(value) {
        Ok(s) => s,
        Err(_) => value.to_string(),
    };
    let mut out = std::io::stdout().lock();
    let _ = out.write_all(rendered.as_bytes());
    let _ = out.write_all(b"\n");
}
