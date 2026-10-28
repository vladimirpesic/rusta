//! Steering: user messages injected into a running turn (the TUI's
//! Ctrl-S).
//!
//! The front-end pushes a message into the agent's [`SteerChannel`] while a
//! turn runs. The turn loop appends every pending message as a user message
//! before the next model request, once the current tool batch has finished,
//! and a `wait_for` in progress ends early with the interrupted-by-message
//! result instead of holding the user hostage.

use std::collections::VecDeque;
use std::sync::Mutex;

use tokio::sync::Notify;

/// A queue of steered messages plus the wake source that ends a `wait_for`
/// early. One channel per agent: a sub-agent gets its own, so input steered
/// while a sub-agent runs is not silently rerouted into the sub-agent's
/// context — it waits for the session agent's next request.
pub struct SteerChannel {
    pending: Mutex<VecDeque<String>>,
    /// Signalled on every [`Self::steer`]. It wakes only the registrations
    /// that exist at that moment and stores no permit, so a message the turn
    /// has already drained leaves nothing behind to end a later wait. A
    /// waiter checks [`Self::is_empty`] after registering, which covers a
    /// message steered before the registration.
    notify: Notify,
}

impl SteerChannel {
    #[must_use]
    pub fn new() -> Self {
        Self {
            pending: Mutex::new(VecDeque::new()),
            notify: Notify::new(),
        }
    }

    /// Steer `message` into the running turn: appended as a user message
    /// before the turn's next model request, and a `wait_for` in progress
    /// ends early.
    pub fn steer(&self, message: impl Into<String>) {
        self.pending.lock().unwrap().push_back(message.into());
        self.notify.notify_waiters();
    }

    /// Take every pending message, oldest first.
    pub fn drain(&self) -> Vec<String> {
        self.pending.lock().unwrap().drain(..).collect()
    }

    /// Whether no message is pending.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.pending.lock().unwrap().is_empty()
    }

    /// Register interest in the next steered message. The registration sees
    /// every message steered after it is created, even before it is first
    /// polled, so create it before consulting [`Self::is_empty`]: a message
    /// steered in between is then not missed.
    pub fn notified(&self) -> tokio::sync::futures::Notified<'_> {
        self.notify.notified()
    }
}

impl Default for SteerChannel {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drain_returns_messages_in_the_order_they_arrived() {
        let channel = SteerChannel::new();
        channel.steer("first");
        channel.steer("second");
        assert_eq!(
            channel.drain(),
            vec!["first".to_string(), "second".to_string()]
        );
        assert!(channel.is_empty(), "draining empties the channel");
    }

    /// A message the turn already drained leaves no signal behind: a later
    /// registration does not complete on its account.
    #[tokio::test(start_paused = true)]
    async fn a_drained_message_leaves_no_signal_behind() {
        let channel = SteerChannel::new();
        channel.steer("taken by the turn");
        channel.drain();
        let woken =
            tokio::time::timeout(std::time::Duration::from_secs(1), channel.notified()).await;
        assert!(woken.is_err(), "no message is pending, so nothing wakes");
    }

    /// A registration created before a steer completes, even when it is first
    /// polled after the steer.
    #[tokio::test(start_paused = true)]
    async fn a_registration_sees_a_steer_made_before_its_first_poll() {
        let channel = SteerChannel::new();
        let registered = channel.notified();
        channel.steer("arrived in between");
        tokio::time::timeout(std::time::Duration::from_secs(1), registered)
            .await
            .expect("the registration predates the steer");
        assert!(!channel.is_empty());
    }

    /// One steer wakes every registered waiter, as two `wait_for` calls in
    /// one batch both end on the same message.
    #[tokio::test(start_paused = true)]
    async fn a_steer_wakes_every_registered_waiter() {
        let channel = SteerChannel::new();
        let first = channel.notified();
        let second = channel.notified();
        channel.steer("for both");
        let timeout = std::time::Duration::from_secs(1);
        tokio::time::timeout(timeout, first).await.expect("first");
        tokio::time::timeout(timeout, second).await.expect("second");
    }
}
