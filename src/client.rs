use std::{collections::HashSet, marker::PhantomData, pin::Pin, task::Poll};

use futures::Stream;
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::{mpsc, oneshot};
use url::Url;

use crate::connection::{
    Connection, ConnectionInternalRequest, ConnectionInternalSubscription, MessageIn, MessageOut,
    RequestSendError,
};

#[derive(Deserialize)]
struct JoinPayloadResponse {
    reason: Option<String>,
}

#[derive(Deserialize)]
struct JoinPayload {
    status: String,
    response: JoinPayloadResponse,
}

#[derive(Error, Debug)]
pub enum SubscribeError {
    #[error("Connection closed")]
    ConnectionClosed,
    #[error("Request failed: {0}")]
    Request(RequestError),
    #[error("Failed to deserialize payload: {0}")]
    DeserializePayload(serde_json::Error),
    #[error("Failed to join channel: {0}")]
    Join(String),
}

#[derive(Error, Debug)]
pub enum RequestError {
    #[error("Connection closed")]
    ConnectionClosed,
    #[error("Request timed out")]
    Timeout,
    #[error("Failed to send request: {0}")]
    Send(RequestSendError),
    #[error("Failed to serialize payload: {0}")]
    SerializePayload(serde_json::Error),
}

pub struct Client {
    conn_request_tx: mpsc::Sender<ConnectionInternalRequest>,
    conn_subscription_tx: mpsc::Sender<ConnectionInternalSubscription>,
    joined_topics: HashSet<String>,
    reference: u64,
}

impl Client {
    pub async fn connect(
        url: Url,
    ) -> Result<(Self, Connection), tokio_tungstenite::tungstenite::Error> {
        let (websocket, _) = tokio_tungstenite::connect_async(format!("{}/websocket", url)).await?;
        let (conn, conn_request_tx, conn_subscription_tx) = Connection::new(websocket);

        Ok((
            Self {
                conn_request_tx,
                conn_subscription_tx,
                joined_topics: HashSet::new(),
                reference: 0,
            },
            conn,
        ))
    }

    pub async fn subscribe<T>(
        &mut self,
        topic: &str,
        event: &str,
    ) -> Result<Subscription<T>, SubscribeError> {
        let (sub_tx, sub_rx) = mpsc::channel(1);

        self.conn_subscription_tx
            .send((topic.to_string(), event.to_string(), sub_tx))
            .await
            .map_err(|_| SubscribeError::ConnectionClosed)?;

        if !self.joined_topics.contains(topic) {
            let res = self
                .request(
                    topic.to_string(),
                    "phx_join".to_string(),
                    None as Option<()>,
                )
                .await
                .map_err(SubscribeError::Request)?;

            let payload: JoinPayload =
                serde_json::from_value(res.payload).map_err(SubscribeError::DeserializePayload)?;

            if payload.status == "error" {
                return Err(SubscribeError::Join(
                    payload.response.reason.unwrap_or_default(),
                ));
            }

            self.joined_topics.insert(topic.to_string());
        }

        Ok(Subscription {
            events_rx: sub_rx,
            _phantom_data: PhantomData::default(),
        })
    }

    async fn request<T: Serialize>(
        &mut self,
        topic: String,
        event: String,
        payload: T,
    ) -> Result<MessageIn, RequestError> {
        let (tx, rx) = oneshot::channel();

        let msg = MessageOut {
            topic,
            event,
            payload: serde_json::to_value(&payload).map_err(RequestError::SerializePayload)?,
            reference: self.reference.to_string(),
        };

        self.reference += 1;

        self.conn_request_tx
            .send((msg, tx))
            .await
            .map_err(|_| RequestError::ConnectionClosed)?;

        rx.await
            .map_err(|_| RequestError::ConnectionClosed)?
            .map_err(RequestError::Send)
    }
}

pub struct Subscription<T> {
    events_rx: mpsc::Receiver<serde_json::Value>,
    _phantom_data: PhantomData<*const T>,
}

impl<T: DeserializeOwned> Stream for Subscription<T> {
    type Item = Result<T, serde_json::Error>;

    fn poll_next(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Option<Self::Item>> {
        match self.events_rx.poll_recv(cx) {
            Poll::Ready(Some(value)) => Poll::Ready(Some(serde_json::from_value(value))),
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => Poll::Pending,
        }
    }
}
