// SPDX-FileCopyrightText: 2026 Guilherme Pedroza
// SPDX-License-Identifier: Apache-2.0

//! Bounded delivery for one app connection; slow clients must reconnect.

use tokio::sync::{mpsc, watch};

const QUEUE_CAPACITY: usize = 32;
const MAX_MESSAGE_BYTES: usize = 64 * 1024;

#[derive(Clone, Debug)]
pub(crate) struct PrivateSession {
    sender: mpsc::Sender<String>,
    closed: watch::Sender<bool>,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum PrivateSendError {
    Full,
    TooLarge,
    Closed,
}

impl std::fmt::Display for PrivateSendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Private delivery failed: {self:?}")
    }
}

impl std::error::Error for PrivateSendError {}

pub(crate) fn channel() -> (
    PrivateSession,
    mpsc::Receiver<String>,
    watch::Receiver<bool>,
) {
    let (sender, messages) = mpsc::channel(QUEUE_CAPACITY);
    let (closed, notification) = watch::channel(false);
    (PrivateSession { sender, closed }, messages, notification)
}

impl PrivateSession {
    pub(crate) fn try_send(&self, message: String) -> Result<(), PrivateSendError> {
        if *self.closed.borrow() {
            return Err(PrivateSendError::Closed);
        }
        let error = if message.len() > MAX_MESSAGE_BYTES {
            PrivateSendError::TooLarge
        } else {
            match self.sender.try_send(message) {
                Ok(()) => return Ok(()),
                Err(mpsc::error::TrySendError::Full(_)) => PrivateSendError::Full,
                Err(mpsc::error::TrySendError::Closed(_)) => PrivateSendError::Closed,
            }
        };
        if !self.closed.send_replace(true) {
            tracing::warn!(reason = ?error, "Closing app connection after private delivery failure");
        }
        Err(error)
    }

    pub(crate) fn same_channel(&self, other: &Self) -> bool {
        self.sender.same_channel(&other.sender)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_session_matches_only_its_own_connection() {
        // Closing a socket removes the user's private channel only when it is
        // still that socket's: a newer connection of the same user must stay.
        let (session, _messages, _closed) = channel();
        let (newer, _newer_messages, _newer_closed) = channel();
        assert!(session.same_channel(&session.clone()));
        assert!(!session.same_channel(&newer));
    }

    #[tokio::test]
    async fn saturated_queue_closes_only_its_connection_without_waiting() {
        let (session, mut messages, mut closed) = channel();
        let (other, mut other_messages, other_closed) = channel();
        for _ in 0..32 {
            session.try_send("queued".into()).unwrap();
        }
        assert_eq!(
            session.try_send("overflow".into()),
            Err(PrivateSendError::Full)
        );
        tokio::time::timeout(
            std::time::Duration::from_millis(100),
            closed.wait_for(|value| *value),
        )
        .await
        .unwrap()
        .unwrap();
        // Draining a slot must not resurrect a terminal connection.
        assert_eq!(messages.recv().await.as_deref(), Some("queued"));
        assert_eq!(
            session.try_send("late".into()),
            Err(PrivateSendError::Closed)
        );
        other.try_send("healthy".into()).unwrap();
        assert_eq!(other_messages.recv().await.as_deref(), Some("healthy"));
        assert!(!*other_closed.borrow());
    }

    #[tokio::test]
    async fn private_message_limit_counts_bytes_and_rejects_before_enqueueing() {
        let (session, mut messages, closed) = channel();
        session.try_send("é".repeat(32768)).unwrap(); // Exactly 64 KiB UTF-8.
        assert_eq!(messages.recv().await.unwrap().len(), 65536);
        assert_eq!(
            session.try_send("é".repeat(32769)),
            Err(PrivateSendError::TooLarge)
        );
        assert!(*closed.borrow());
        assert!(messages.try_recv().is_err());
    }

    #[tokio::test]
    async fn closed_receiver_rejects_delivery_and_signals_shutdown() {
        let (session, messages, closed) = channel();
        drop(messages);
        assert_eq!(
            session.try_send("gone".into()),
            Err(PrivateSendError::Closed)
        );
        assert!(*closed.borrow());
    }
}
