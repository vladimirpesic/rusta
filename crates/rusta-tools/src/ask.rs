//! The `ask` tool — §6.4: pause the turn for a user reply.
//!
//! The reply text returns as the observation and the turn continues. At
//! most one pending `ask` per turn — that bound is enforced by the agent
//! loop (M8), which owns turn boundaries; this handler only performs the
//! question→reply exchange through the injected [`Responder`].

use std::sync::Mutex;

use serde_json::Value;

use crate::exec::{ToolOutcome, lock, req_nonempty};

/// Supplies the user's reply to an `ask`. The CLI (M8) implements the
/// terminal prompt; tests script it.
pub trait Responder: Send {
    /// Answer `question`; the text becomes the next observation.
    fn reply(&mut self, question: &str) -> String;
}

/// Headless responder for non-interactive runs (`rusta -c`): no user is
/// attached, and the model is told so explicitly.
pub struct Headless;

impl Responder for Headless {
    fn reply(&mut self, _question: &str) -> String {
        "(no user available in this run — proceed with your best judgment and state your assumptions)"
            .to_owned()
    }
}

/// Execute `ask(question)`.
pub(crate) fn ask(responder: &Mutex<Box<dyn Responder>>, input: &Value) -> ToolOutcome {
    let question = match req_nonempty(input, "question") {
        Ok(question) => question,
        Err(outcome) => return outcome,
    };
    let reply = lock(responder).reply(question);
    ToolOutcome::ok(format!("USER REPLY:\n{reply}"))
}
