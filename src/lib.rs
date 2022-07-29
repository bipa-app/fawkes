pub mod client;
pub mod connection;

#[cfg(test)]
mod test {
    use futures::StreamExt;
    use serde::{Deserialize, Serialize};
    use serde_json::json;
    use std::collections::HashSet;
    use url::Url;

    #[derive(Serialize, Deserialize)]
    struct PhoenixChannelMessage {
        topic: String,
        event: String,
        payload: serde_json::Value,
        #[serde(rename = "ref")]
        reference: String,
    }

    async fn handler(ws: axum::extract::WebSocketUpgrade) -> axum::response::Response {
        ws.on_upgrade(handle_socket)
    }

    async fn handle_socket(mut socket: axum::extract::ws::WebSocket) {
        let mut topic_joins = HashSet::new();

        let mut tick = tokio::time::interval(std::time::Duration::from_secs(1));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                _ = tick.tick() => {
                    for topic in topic_joins.iter() {
                        socket.send(axum::extract::ws::Message::Text(serde_json::to_string(&json!({
                            "topic": topic,
                            "event": "event_name",
                            "payload": {
                                "test": true,
                            },
                            "ref": null
                        })).expect("correctly formatted event"))).await.expect("event sent");
                    }
                }

                msg = socket.recv() => {
                    if let Some(Ok(msg)) = msg {
                        if matches!(msg, axum::extract::ws::Message::Close(_)) {
                            break
                        }

                        let msg_text = msg.to_text().expect("text message");
                        let channel_msg: PhoenixChannelMessage =
                            serde_json::from_str(msg_text).expect("correctly formatted JSON");

                        let reply = match channel_msg.event.as_str() {
                            "phx_join" => {
                                topic_joins.insert(channel_msg.topic.clone());

                                Some(json!({
                                    "topic": channel_msg.topic,
                                    "event": "phx_reply",
                                    "payload": {
                                        "status": "ok",
                                        "response": {}
                                    },
                                    "ref": channel_msg.reference
                                }))
                            }
                            "heartbeat" => Some(json!({
                                "topic": "phoenix",
                                "event": "phx_reply",
                                "payload": {},
                                "ref": channel_msg.reference,
                            })),
                            _ => None,
                        };

                        let reply_json = serde_json::to_string(&reply).expect("corretly formatted reply");

                        socket
                            .send(axum::extract::ws::Message::Text(reply_json))
                            .await
                            .expect("reply sent");
                    } else {
                        socket.close().await.unwrap();
                        break
                    }
                }
            }
        }
    }

    async fn server() {
        let app = axum::routing::Router::new()
            .route("/test/socket/websocket", axum::routing::get(handler));

        let addr = std::net::SocketAddr::from(([127, 0, 0, 1], 3000));
        axum::Server::bind(&addr)
            .serve(app.into_make_service())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn it_works() {
        tokio::spawn(server());
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        let (client, connection) = crate::client::Client::connect(
            Url::parse("ws://127.0.0.1:3000/test/socket").expect("valid url"),
        )
        .await
        .expect("client connected");

        let conn_fut_handle = tokio::spawn(connection);

        let mut sub: crate::client::Subscription<serde_json::Value> = client
            .subscribe("a", "event_name")
            .await
            .expect("subscribed successfully");

        let evt_payload = sub
            .next()
            .await
            .expect("event")
            .expect("no error from subscription");

        assert_eq!(
            evt_payload,
            json!({
                "test": true
            })
        );

        client.close();
        conn_fut_handle
            .await
            .expect("connection future did not panic")
            .expect("connection closed gracefully");
    }
}
