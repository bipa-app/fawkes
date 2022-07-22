use std::{collections::HashMap, pin::Pin, task::Poll};

use futures::{Future, Sink, Stream};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::{
    net::TcpStream,
    sync::{mpsc, oneshot},
};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

#[derive(Serialize)]
pub(crate) struct MessageOut {
    pub topic: String,
    pub event: String,
    pub payload: serde_json::Value,
    #[serde(rename = "ref")]
    pub reference: String,
}

#[derive(Deserialize)]
pub(crate) struct MessageIn {
    pub topic: String,
    pub event: String,
    pub payload: serde_json::Value,
    #[serde(rename = "ref")]
    pub reference: Option<String>,
}

#[derive(Error, Debug)]
pub enum RequestSendError {
    #[error("Invalid JSON: {0}")]
    InvalidMessageOutJson(serde_json::Error),
    #[error("Failed to send request: {0}")]
    Send(tokio_tungstenite::tungstenite::Error),
}

#[derive(Debug)]
pub struct ConnectionError;

pub(crate) type ConnectionInternalRequest = (
    MessageOut,
    oneshot::Sender<Result<MessageIn, RequestSendError>>,
);
pub(crate) type ConnectionInternalSubscription = (String, String, mpsc::Sender<serde_json::Value>);

enum ConnectionState {
    Unknown,
    Ready,
}

pub struct Connection {
    websocket: WebSocketStream<MaybeTlsStream<TcpStream>>,
    request_rx: mpsc::Receiver<ConnectionInternalRequest>,
    subscription_rx: mpsc::Receiver<ConnectionInternalSubscription>,
    state: ConnectionState,
    pending_requests: HashMap<String, oneshot::Sender<Result<MessageIn, RequestSendError>>>,
    subscriptions: HashMap<(String, String), Vec<mpsc::Sender<serde_json::Value>>>,
}

impl Connection {
    pub(crate) fn new(
        websocket: WebSocketStream<MaybeTlsStream<TcpStream>>,
    ) -> (
        Self,
        mpsc::Sender<ConnectionInternalRequest>,
        mpsc::Sender<ConnectionInternalSubscription>,
    ) {
        let (request_tx, request_rx) = mpsc::channel(1);
        let (subscription_tx, subscription_rx) = mpsc::channel(1);

        (
            Self {
                websocket,
                request_rx,
                subscription_rx,
                state: ConnectionState::Unknown,
                pending_requests: HashMap::new(),
                subscriptions: HashMap::new(),
            },
            request_tx,
            subscription_tx,
        )
    }

    fn poll_send(&mut self, cx: &mut std::task::Context<'_>) -> Poll<<Self as Future>::Output> {
        let next_state = match self.state {
            ConnectionState::Unknown => {
                if let Poll::Ready(Ok(())) = Pin::new(&mut self.websocket).poll_ready(cx) {
                    ConnectionState::Ready
                } else {
                    ConnectionState::Unknown
                }
            }

            ConnectionState::Ready => match self.request_rx.poll_recv(cx) {
                Poll::Ready(Some((msg, sender))) => {
                    cx.waker().wake_by_ref();

                    match serde_json::to_string(&msg) {
                        Ok(json_msg) => {
                            match Pin::new(&mut self.websocket)
                                .start_send(tokio_tungstenite::tungstenite::Message::text(json_msg))
                            {
                                Ok(_) => {
                                    assert!(matches!(
                                        self.pending_requests.insert(msg.reference, sender),
                                        None
                                    ));
                                }
                                Err(e) => {
                                    let _ = sender.send(Err(RequestSendError::Send(e)));
                                }
                            }
                        }
                        Err(e) => {
                            let _ = sender.send(Err(RequestSendError::InvalidMessageOutJson(e)));
                        }
                    }

                    ConnectionState::Unknown
                }
                Poll::Ready(None) => return Poll::Ready(Ok(())),
                Poll::Pending => ConnectionState::Ready,
            },
        };

        self.state = next_state;

        Poll::Pending
    }

    fn poll_subscription_receiver(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<<Self as Future>::Output> {
        if let Poll::Ready(Some((topic, event, sender))) = self.subscription_rx.poll_recv(cx) {
            cx.waker().wake_by_ref();

            let subs_senders = self.subscriptions.entry((topic, event)).or_default();
            subs_senders.push(sender);
        }

        Poll::Pending
    }

    fn poll_request_receiver(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<<Self as Future>::Output> {
        if let Poll::Ready(res) = Pin::new(&mut self.websocket).poll_next(cx) {
            cx.waker().wake_by_ref();

            match res {
                Some(res) => {
                    if let Ok(ws_msg) = res {
                        match ws_msg {
                            tokio_tungstenite::tungstenite::Message::Text(msg_json) => {
                                let res: Result<MessageIn, _> =
                                    serde_json::from_str(msg_json.as_str());

                                match res {
                                    Ok(msg) => match &msg.reference {
                                        Some(reference) => {
                                            match self.pending_requests.remove(reference) {
                                                Some(sender) => {
                                                    let _ = sender.send(Ok(msg));
                                                }
                                                None => todo!(),
                                            }
                                        }
                                        None => {
                                            if let Some(senders) =
                                                self.subscriptions.get(&(msg.topic, msg.event))
                                            {
                                                for sender in senders {
                                                    sender.try_send(msg.payload.clone()).unwrap();
                                                }
                                            }
                                        }
                                    },
                                    Err(_) => todo!(),
                                }
                            }
                            tokio_tungstenite::tungstenite::Message::Close(_) => todo!(),
                            _ => {}
                        }
                    }
                }

                None => return Poll::Ready(Ok(())),
            }
        }

        Poll::Pending
    }
}

impl Future for Connection {
    type Output = Result<(), ConnectionError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> Poll<Self::Output> {
        if self.poll_subscription_receiver(cx).is_ready()
            || self.poll_send(cx).is_ready()
            || self.poll_request_receiver(cx).is_ready()
        {
            return Poll::Ready(Ok(()));
        }

        Poll::Pending
    }
}
