use fawkes::client::{Client, Subscription};
use futures::StreamExt;
use serde::Deserialize;
use url::Url;

#[derive(Deserialize)]
struct Ticker {
    buy: f64,
    sell: f64,
}

#[derive(Deserialize)]
struct Tickers {
    #[serde(rename = "BTC-BRL")]
    btc_brl: Ticker,
}

#[tokio::main]
async fn main() {
    let (mut client, conn) = Client::connect(
        Url::parse("wss://bp-channels.gigalixirapp.com/orderbook/socket").expect("valid url"),
    )
    .await
    .expect("client");

    tokio::spawn(conn);

    let mut sub: Subscription<Tickers> = client.subscribe("ticker:ALL-BRL", "price").await.unwrap();

    while let Some(payload) = sub.next().await {
        match payload {
            Ok(payload) => println!(
                "buy = {}, sell = {}",
                payload.btc_brl.buy, payload.btc_brl.sell
            ),
            Err(e) => eprintln!("{}", e),
        }
    }
}
