use std::sync::Arc;

use rustyline::DefaultEditor;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use agent_core::errors::AgentError;

use crate::{commands, stream, terminal_sanitize};

pub async fn run_repl(
    ctx: &mut commands::CommandContext,
    cancel: CancellationToken,
    stream_json: bool,
) -> anyhow::Result<()> {
    println!("rusta interactive REPL. Type /exit to quit.");

    let mut rl = DefaultEditor::new()?;
    let history_path = agent_core::user_dirs::data_dir().map(|d| d.join("rusta").join("history"));
    if let Some(path) = &history_path {
        let _ = rl.load_history(path);
    }
    let rl = Arc::new(Mutex::new(rl));

    loop {
        if cancel.is_cancelled() {
            break;
        }

        let readline = tokio::task::spawn_blocking({
            let rl = rl.clone();
            move || {
                let mut rl = rl.blocking_lock();
                rl.readline("> ")
            }
        })
        .await?;

        let line = match readline {
            Ok(line) => line,
            Err(rustyline::error::ReadlineError::Interrupted)
            | Err(rustyline::error::ReadlineError::Eof) => break,
            Err(e) => {
                eprintln!("Readline error: {e}");
                break;
            }
        };

        {
            let mut rl = rl.lock().await;
            rl.add_history_entry(line.as_str())?;
        }

        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        if line == "/exit" || line == "/quit" {
            println!("Goodbye!");
            break;
        }

        // Scheduled prompts that came due while the REPL waited, queued for
        // the next turn (unlike the TUI, the REPL never runs a turn on its
        // own). They are polled here rather than announced as they come due:
        // rustyline's printer for that leaves keys that arrive together — a
        // paste, typeahead, an input method's word — unread after the first
        // until the next key.
        let due = ctx.agent.lock().await.poll_cron();

        // A slash command either is the whole turn or leads into one:
        // `/swarm <task>` sends its task.
        let prompt = match handle_slash_command(line, ctx).await {
            Slash::Handled => {
                announce_due(&due, "with your next message");
                continue;
            }
            Slash::Prompt(prompt) => prompt,
        };
        announce_due(&due, "ahead of this message");
        let line = prompt.as_str();

        let llm = ctx.llm.clone();
        {
            let mut agent = ctx.agent.lock().await;
            // A session another process is writing is offered back: retry,
            // and once a retry is refused too, take it over. A script driving
            // the REPL gets the error.
            let mut retried = false;
            loop {
                let (callbacks, tool_event_sender) = if stream_json {
                    stream::json_callbacks()
                } else {
                    stream::repl_callbacks()
                };
                agent.set_tool_event_sender(tool_event_sender);
                match agent
                    .prompt_with_callbacks(llm.as_ref(), line, callbacks, &cancel)
                    .await
                {
                    Ok(result) => {
                        if stream_json {
                            stream::emit_usage(&result.usage);
                        }
                        if let Some(reason) = result.stop_reason {
                            stream::report_stop(reason, stream_json);
                        }
                        crate::record_session_activity(ctx, None).await;
                        crate::session_title::after_turn(ctx, agent.first_exchange());
                    }
                    Err(AgentError::SessionOccupied(occupied)) if !stream_json => {
                        println!("{occupied}");
                        let question = if retried {
                            "[R]etry, [t]ake over, or [c]ancel? "
                        } else {
                            "[R]etry or [c]ancel? "
                        };
                        let answer = ask(&rl, question).await;
                        match occupancy_choice(&answer, retried) {
                            Occupancy::Retry => {
                                retried = true;
                                continue;
                            }
                            Occupancy::TakeOver => match agent.take_over_session() {
                                Ok(_) => {
                                    println!("Took the session over.");
                                    continue;
                                }
                                Err(e) => println!("Could not take the session over: {e}"),
                            },
                            Occupancy::Cancel => println!("Cancelled; your message was not sent."),
                        }
                    }
                    Err(e) => stream::report_turn_error(&e, stream_json),
                }
                break;
            }
            // Let go of this turn's printer, which ends its last line as it
            // goes. Kept until the next turn replaced it, it put the `> `
            // prompt right after the answer, and the missing line break after
            // the next question.
            agent.set_tool_event_sender(stream::silent_tool_events());
        }
    }

    if let Some(path) = &history_path {
        if let Some(parent) = path.parent() {
            let _ = tokio::fs::create_dir_all(parent).await;
        }
        let _ = rl.lock().await.save_history(path);
    }

    Ok(())
}

/// What to do with a message the session's lock refused.
#[derive(Debug, PartialEq, Eq)]
enum Occupancy {
    Retry,
    TakeOver,
    Cancel,
}

/// The choice `answer` makes: retry by default, take over only once a
/// retry has been refused too.
fn occupancy_choice(answer: &str, retried: bool) -> Occupancy {
    match answer.trim().to_ascii_lowercase().as_str() {
        "c" | "cancel" | "n" | "no" => Occupancy::Cancel,
        "t" | "take over" | "takeover" if retried => Occupancy::TakeOver,
        _ => Occupancy::Retry,
    }
}

/// Ask `question` on the REPL's line; an interrupted read is a cancel.
async fn ask(rl: &Arc<Mutex<DefaultEditor>>, question: &'static str) -> String {
    let rl = rl.clone();
    tokio::task::spawn_blocking(move || rl.blocking_lock().readline(question))
        .await
        .ok()
        .and_then(Result::ok)
        .unwrap_or_else(|| "cancel".to_string())
}

/// Say which scheduled prompts came due, and when they reach the model.
fn announce_due(jobs: &[String], when: &str) {
    for job in jobs {
        println!("Scheduled prompt {job} came due; it is delivered {when}.");
    }
}

/// What a line of input comes to once any slash command in it has run.
enum Slash {
    /// The command was the whole of it.
    Handled,
    /// Send this as the prompt: the line itself, or a command's follow-up.
    Prompt(String),
}

/// Run `line` if it is a slash command.
async fn handle_slash_command(line: &str, ctx: &mut commands::CommandContext) -> Slash {
    if !line.starts_with('/') {
        return Slash::Prompt(line.to_string());
    }

    let cmd = commands::Command::parse(line);
    let follow_up = cmd.follow_up_prompt();
    match commands::execute(ctx, cmd).await {
        // Command output can carry what the model wrote, such as todo items.
        Ok(msg) => println!("{}", terminal_sanitize::for_stdout(&msg)),
        Err(e) => {
            eprintln!("{}", terminal_sanitize::for_stderr(&e.to_string()));
            return Slash::Handled;
        }
    }
    follow_up.map_or(Slash::Handled, Slash::Prompt)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Retry is the default; take over is offered once a retry was
    /// refused too; cancel is always there.
    #[test]
    fn an_occupied_session_offers_retry_then_take_over() {
        assert_eq!(occupancy_choice("", false), Occupancy::Retry);
        assert_eq!(occupancy_choice(" R ", false), Occupancy::Retry);
        assert_eq!(occupancy_choice("t", false), Occupancy::Retry);
        assert_eq!(occupancy_choice("t", true), Occupancy::TakeOver);
        assert_eq!(occupancy_choice("Take over", true), Occupancy::TakeOver);
        assert_eq!(occupancy_choice("c", false), Occupancy::Cancel);
        assert_eq!(occupancy_choice("cancel", true), Occupancy::Cancel);
    }
}
