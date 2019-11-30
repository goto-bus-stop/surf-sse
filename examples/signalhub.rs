use futures_util::stream::StreamExt;
use surf_sse::{EventSource, Error};

async fn amain() {
    let mut client = EventSource::new(
        "https://signalhub-jccqtwhdwc.now.sh/v1/sse-codec/example"
            .parse()
            .unwrap(),
    );

    while let Some(message) = client.next().await {
        match message {
            Ok(message) => println!("received: {:?}", message),
            Err(Error::Retry) => println!("connection lost, retrying"),
            Err(err) => panic!("connection failed: {}", err),
        }
    }
}

fn main() {
    async_std::task::block_on(amain());
}
