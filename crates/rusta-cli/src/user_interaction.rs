use std::io::Write;

use agent_core::UserInteraction;
use async_trait::async_trait;

/// Ask the user a question via stdin/stdout.
pub struct StdinUserInteraction {
    /// Ask with a `question` event rather than a text prompt, for
    /// `--stream-json`, whose stdout carries nothing but JSON events.
    json: bool,
}

impl StdinUserInteraction {
    pub fn new() -> Self {
        Self { json: false }
    }

    /// Ask with a `question` event on stdout; see
    /// [`crate::stream::emit_question`].
    pub fn json() -> Self {
        Self { json: true }
    }
}

impl Default for StdinUserInteraction {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl UserInteraction for StdinUserInteraction {
    async fn ask(&self, question: &str) -> Option<String> {
        let _asking = ASKING.lock().await;
        if self.json {
            crate::stream::emit_question(question);
        } else {
            let question = crate::terminal_sanitize::for_stdout(question);
            println!("\n[ask_user] {question}");
            print!("answer> ");
            let _ = std::io::stdout().flush();
        }

        match read_answer().await {
            Ok(Some(line)) => {
                let line = line.trim();
                if line.is_empty() {
                    None
                } else {
                    Some(line.to_string())
                }
            }
            Ok(None) | Err(_) => None,
        }
    }
}

/// Held while a question or an approval is put and answered. Sub-agents ask
/// through the session's hooks, and several swarm members can ask at once: one
/// at a time keeps each answer with its question.
pub(crate) static ASKING: std::sync::LazyLock<tokio::sync::Mutex<()>> =
    std::sync::LazyLock::new(|| tokio::sync::Mutex::new(()));

/// One line of standard input: the answer to a question or an approval, or
/// `None` at the end of input.
///
/// Every answer is read through one shared buffered reader. A reader of its
/// own buffers ahead — a whole pipe's worth — and drops what it read past its
/// line along with itself, so answers piped in for later questions vanished
/// and the next question read the end of input, which cancels the turn.
pub(crate) async fn read_answer() -> std::io::Result<Option<String>> {
    use tokio::io::AsyncBufReadExt;
    type SharedStdin = tokio::sync::Mutex<tokio::io::BufReader<tokio::io::Stdin>>;
    static STDIN: std::sync::LazyLock<SharedStdin> = std::sync::LazyLock::new(|| {
        tokio::sync::Mutex::new(tokio::io::BufReader::new(tokio::io::stdin()))
    });
    let mut reader = STDIN.lock().await;
    let mut line = String::new();
    Ok(match reader.read_line(&mut line).await? {
        0 => None,
        _ => Some(line),
    })
}
