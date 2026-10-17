//! One count/byte budget shared by all producers of a connection's messages.
use crate::{api::figura::SessionMessage, service::event::CloudEvent};
use prost::Message;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use tokio::sync::{mpsc, watch};

const MAX_BYTES: usize = 262_144;

struct Item {
    message: Option<SessionMessage>,
    cost: usize,
    budget: Arc<AtomicUsize>,
}

impl Drop for Item {
    fn drop(&mut self) {
        self.budget.fetch_sub(self.cost, Ordering::AcqRel);
    }
}

#[derive(Clone, Debug)]
pub struct Sender {
    sender: mpsc::Sender<Item>,
    budget: Arc<AtomicUsize>,
    close: watch::Sender<bool>,
}

pub struct Receiver(mpsc::Receiver<Item>);

pub fn channel(capacity: usize) -> (Sender, Receiver) {
    let (sender, receiver) = mpsc::channel(capacity);
    (
        Sender {
            sender,
            budget: Arc::new(AtomicUsize::new(0)),
            close: watch::channel(false).0,
        },
        Receiver(receiver),
    )
}

fn cost(message: &SessionMessage) -> usize {
    match message {
        SessionMessage::Event(CloudEvent::Ysm { body, .. }) => {
            // Includes the largest authenticated v1 envelope header used here.
            crate::protocol::v1::Envelope {
                major: 1,
                body: Some(body.clone()),
                ..Default::default()
            }
            .encoded_len()
                + 128
        }
        SessionMessage::Event(CloudEvent::FiguraPing { arguments, .. }) => arguments.len() + 128,
        SessionMessage::Event(CloudEvent::AvatarChanged { .. }) => 4096,
        SessionMessage::Event(CloudEvent::LegacyRaw(bytes)) | SessionMessage::Ping(bytes) => {
            bytes.len() + 128
        }
        SessionMessage::Banned | SessionMessage::ResyncRequired => 128,
    }
}

impl Sender {
    pub fn close_signal(&self) -> watch::Sender<bool> {
        self.close.clone()
    }

    pub fn try_send(
        &self,
        message: SessionMessage,
    ) -> Result<(), mpsc::error::TrySendError<SessionMessage>> {
        let cost = cost(&message);
        if self
            .budget
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |used| {
                used.checked_add(cost).filter(|total| *total <= MAX_BYTES)
            })
            .is_err()
        {
            self.close.send_replace(true);
            return Err(mpsc::error::TrySendError::Full(message));
        }
        let item = Item {
            message: Some(message),
            cost,
            budget: self.budget.clone(),
        };
        match self.sender.try_send(item) {
            Ok(()) => Ok(()),
            Err(error) => {
                let full = matches!(&error, mpsc::error::TrySendError::Full(_));
                let mut item = error.into_inner();
                let message = item.message.take().expect("queued message");
                if full {
                    self.close.send_replace(true);
                    Err(mpsc::error::TrySendError::Full(message))
                } else {
                    Err(mpsc::error::TrySendError::Closed(message))
                }
            }
        }
    }

    /// Producers never wait indefinitely for a slow network consumer.
    pub async fn send(
        &self,
        message: SessionMessage,
    ) -> Result<(), mpsc::error::SendError<SessionMessage>> {
        self.try_send(message)
            .map_err(|error| mpsc::error::SendError(error.into_inner()))
    }
}

impl Receiver {
    pub async fn recv(&mut self) -> Option<SessionMessage> {
        let mut item = self.0.recv().await?;
        if matches!(item.message.as_ref(), Some(SessionMessage::Event(CloudEvent::Ysm {
            body: crate::protocol::v1::envelope::Body::YsmEvent(_), expires_at: Some(deadline), ..
        })) if *deadline <= crate::storage::now_seconds())
        {
            return Some(SessionMessage::ResyncRequired);
        }
        item.message.take()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn expired_instantaneous_events_require_recovery_instead_of_replay() {
        let (sender, mut receiver) = channel(32);
        sender
            .try_send(SessionMessage::Event(CloudEvent::Ysm {
                owner: uuid::Uuid::nil(),
                body: crate::protocol::v1::envelope::Body::YsmEvent(Default::default()),
                expires_at: Some(0),
            }))
            .unwrap();
        assert!(matches!(
            receiver.recv().await,
            Some(SessionMessage::ResyncRequired)
        ));
        assert_eq!(sender.budget.load(Ordering::Acquire), 0);
    }

    #[tokio::test]
    async fn bytes_bound_all_producers_and_are_released_on_receive_or_drop() {
        let (sender, mut receiver) = channel(32);
        let other_producer = sender.clone();
        let signal = sender.close_signal();
        sender
            .try_send(SessionMessage::Ping(vec![0; MAX_BYTES / 2]))
            .unwrap();
        assert!(other_producer
            .try_send(SessionMessage::Ping(vec![0; MAX_BYTES / 2]))
            .is_err());
        assert!(*signal.borrow());
        receiver.recv().await.unwrap();
        assert_eq!(sender.budget.load(Ordering::Acquire), 0);
        sender
            .try_send(SessionMessage::Ping(vec![0; MAX_BYTES / 2]))
            .unwrap();
        drop(receiver);
        assert_eq!(sender.budget.load(Ordering::Acquire), 0);
        assert!(sender.try_send(SessionMessage::Banned).is_err());
        assert_eq!(sender.budget.load(Ordering::Acquire), 0);
    }
}
