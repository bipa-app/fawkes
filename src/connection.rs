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
pub enum ConnectionError {
    Heartbeat,
    Websocket(tokio_tungstenite::tungstenite::Error),
}

pub(crate) struct ConnectionInternalRequest {
    pub topic: String,
    pub event: String,
    pub payload: serde_json::Value,
}

pub(crate) type ConnectionInternalSubscription = (String, String, mpsc::Sender<serde_json::Value>);

enum ConnectionState {
    Unknown,
    Ready,
    Closing,
    Closed,
}

pub struct Connection {
    websocket: WebSocketStream<MaybeTlsStream<TcpStream>>,
    request_rx: mpsc::Receiver<(
        ConnectionInternalRequest,
        oneshot::Sender<Result<serde_json::Value, RequestSendError>>,
    )>,
    subscription_rx: mpsc::Receiver<ConnectionInternalSubscription>,
    state: ConnectionState,
    pending_requests: HashMap<String, oneshot::Sender<Result<serde_json::Value, RequestSendError>>>,
    subscriptions: HashMap<(String, String), Vec<mpsc::Sender<serde_json::Value>>>,
    heartbeat_timer: tokio::time::Interval,
    heartbeat_ref: Option<String>,
    reference: u64,
}

impl Connection {
    pub(crate) fn new(
        websocket: WebSocketStream<MaybeTlsStream<TcpStream>>,
    ) -> (
        Self,
        mpsc::Sender<(
            ConnectionInternalRequest,
            oneshot::Sender<Result<serde_json::Value, RequestSendError>>,
        )>,
        mpsc::Sender<ConnectionInternalSubscription>,
    ) {
        let (request_tx, request_rx) = mpsc::channel(1);
        let (subscription_tx, subscription_rx) = mpsc::channel(1);

        let mut heartbeat_timer = tokio::time::interval(std::time::Duration::from_secs(30));
        heartbeat_timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        (
            Self {
                websocket,
                request_rx,
                subscription_rx,
                heartbeat_timer,
                state: ConnectionState::Unknown,
                pending_requests: HashMap::new(),
                subscriptions: HashMap::new(),
                heartbeat_ref: None,
                reference: 0,
            },
            request_tx,
            subscription_tx,
        )
    }

    fn send_request(
        &mut self,
        req: ConnectionInternalRequest,
        response_tx: oneshot::Sender<Result<serde_json::Value, RequestSendError>>,
    ) {
        log::trace!(
            "Sending request topic={} event={} reference={}",
            req.topic,
            req.event,
            self.reference
        );

        let msg = MessageOut {
            topic: req.topic,
            event: req.event,
            payload: req.payload,
            reference: self.reference.to_string(),
        };

        match serde_json::to_string(&msg) {
            Ok(json_msg) => {
                match Pin::new(&mut self.websocket)
                    .start_send(tokio_tungstenite::tungstenite::Message::text(json_msg))
                {
                    Ok(_) => {
                        if self
                            .pending_requests
                            .insert(msg.reference, response_tx)
                            .is_some()
                        {
                            log::error!("Replaced request with same ref");
                        }

                        self.reference += 1;
                    }
                    Err(e) => {
                        if response_tx.send(Err(RequestSendError::Send(e))).is_err() {
                            log::debug!("Internal request channel receiver dropped");
                        }
                    }
                }
            }
            Err(e) => {
                if response_tx
                    .send(Err(RequestSendError::InvalidMessageOutJson(e)))
                    .is_err()
                {
                    log::debug!("Internal request channel receiver dropped");
                }
            }
        }
    }

    fn send_request_sync(
        &mut self,
        req: ConnectionInternalRequest,
    ) -> Result<(), RequestSendError> {
        log::trace!(
            "Sending request topic={} event={} reference={}",
            req.topic,
            req.event,
            self.reference
        );

        let msg = MessageOut {
            topic: req.topic,
            event: req.event,
            payload: req.payload,
            reference: self.reference.to_string(),
        };

        let json_msg =
            serde_json::to_string(&msg).map_err(RequestSendError::InvalidMessageOutJson)?;

        Pin::new(&mut self.websocket)
            .start_send(tokio_tungstenite::tungstenite::Message::text(json_msg))
            .map_err(RequestSendError::Send)?;

        self.reference += 1;

        Ok(())
    }

    fn poll_send(&mut self, cx: &mut std::task::Context<'_>) -> Poll<<Self as Future>::Output> {
        let next_state = match self.state {
            ConnectionState::Unknown => match Pin::new(&mut self.websocket).poll_ready(cx) {
                Poll::Ready(Ok(())) => ConnectionState::Ready,
                Poll::Ready(Err(e)) => return Poll::Ready(Err(ConnectionError::Websocket(e))),
                _ => ConnectionState::Unknown,
            },

            ConnectionState::Ready => {
                let reference = self.reference;

                if let Poll::Ready(_) = self.heartbeat_timer.poll_tick(cx) {
                    cx.waker().wake_by_ref();

                    if let Err(e) = self.send_request_sync(ConnectionInternalRequest {
                        topic: "phoenix".to_string(),
                        event: "heartbeat".to_string(),
                        payload: serde_json::Value::Null,
                    }) {
                        log::error!("Failed to send heartbeat: {}", e);

                        return Poll::Ready(Err(ConnectionError::Heartbeat));
                    }

                    self.heartbeat_ref = Some(reference.to_string());

                    ConnectionState::Unknown
                } else {
                    match self.request_rx.poll_recv(cx) {
                        Poll::Ready(Some((req, response_tx))) => {
                            cx.waker().wake_by_ref();

                            self.send_request(req, response_tx);

                            ConnectionState::Unknown
                        }
                        Poll::Ready(None) => ConnectionState::Closing,
                        Poll::Pending => ConnectionState::Ready,
                    }
                }
            }
            ConnectionState::Closing => {
                if let Poll::Ready(_) = Pin::new(&mut self.websocket).poll_close(cx) {
                    ConnectionState::Closed
                } else {
                    ConnectionState::Closing
                }
            }
            ConnectionState::Closed => return Poll::Ready(Ok(())),
        };

        self.state = next_state;

        Poll::Pending
    }

    fn poll_subscription_receiver(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<<Self as Future>::Output> {
        match self.subscription_rx.poll_recv(cx) {
            Poll::Ready(Some((topic, event, sender))) => {
                cx.waker().wake_by_ref();

                let subs_senders = self.subscriptions.entry((topic, event)).or_default();
                subs_senders.push(sender);
            }
            Poll::Ready(None) => self.state = ConnectionState::Closing,
            _ => {}
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
                Some(res) => match res {
                    Ok(ws_msg) => match ws_msg {
                        tokio_tungstenite::tungstenite::Message::Text(msg_json) => {
                            let res: Result<MessageIn, _> = serde_json::from_str(msg_json.as_str());

                            match res {
                                Ok(msg) => match &msg.reference {
                                    Some(reference)
                                        if self.heartbeat_ref.as_ref() == Some(reference) =>
                                    {
                                        self.heartbeat_ref = None;
                                        log::trace!("Received heartbeat response");
                                    }
                                    Some(reference) => {
                                        match self.pending_requests.remove(reference) {
                                            Some(sender) => {
                                                if sender.send(Ok(msg.payload)).is_err() {
                                                    log::debug!(
                                                        "Internal request channel receiver dropped"
                                                    );
                                                }
                                            }
                                            None => {
                                                log::debug!("Received response for a request that was not pending");
                                            }
                                        }
                                    }
                                    None => {
                                        if let Some(senders) =
                                            self.subscriptions.get_mut(&(msg.topic, msg.event))
                                        {
                                            senders.retain(|sender| {
                                                match sender.try_send(msg.payload.clone()) {
                                                    Ok(_) => true,
                                                    Err(e) => {
                                                        match e {
                                                            mpsc::error::TrySendError::Full(_) => {
                                                                log::warn!("Dropping received event because subscription channel full");
                                                                true
                                                            }
                                                            mpsc::error::TrySendError::Closed(_) => {
                                                                log::trace!("Removing subscription sender");
                                                                false
                                                            }
                                                        }
                                                    }
                                                }
                                            });
                                        }
                                    }
                                },

                                Err(e) => {
                                    log::debug!("Failed to deserialize response: {}", e);
                                }
                            }
                        }

                        tokio_tungstenite::tungstenite::Message::Close(_) => {
                            self.state = ConnectionState::Closing;
                        }

                        _ => {}
                    },
                    Err(e) => return Poll::Ready(Err(ConnectionError::Websocket(e))),
                },

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
