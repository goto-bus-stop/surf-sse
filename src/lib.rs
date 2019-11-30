//! An implementation of the [EventSource][] API using [Surf][].
//!
//! [EventSource]: https://developer.mozilla.org/en-US/docs/Web/API/EventSource
//! [Surf]: https://github.com/http-rs/surf

#![deny(future_incompatible)]
#![deny(nonstandard_style)]
#![deny(rust_2018_idioms)]
#![deny(unsafe_code)]
#![warn(missing_docs)]
#![warn(unused)]

use futures_core::stream::Stream;
use futures_timer::Delay;
use futures_util::future::FutureExt;
use futures_util::stream::StreamExt;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;
use surf::url::Url;

/// An event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Event {
    /// The event name, defaults to "message".
    pub event: String,
    /// The event data as a UTF-8 String.
    pub data: String,
}

/// The state of the connection.
///
/// Unlike browser implementations of the EventSource API, this does not have a `Closed` value, because
/// the stream is closed by dropping the struct. At that point, there's no way to inspect the state
/// anyway.
///
/// A `Closed` value may be added in the future when there is more robust error handling in place.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadyState {
    /// The client is connecting. It may have sent a request already, or be waiting for a retry
    /// timer.
    Connecting = 0,
    /// The connection is open and ready to read messages.
    Open = 1,
}

/// Represents an EventSource "error" event.
///
/// Note that you should not always stop reading messages when an error occurs. Many errors are
/// benign. EventSources retry a lot, emitting errors on every failure.
#[derive(Debug)]
pub enum Error {
    /// The connection was closed by the server. EventSource will reopen the connection.
    Retry,
    /// An error occurred while connecting to the endpoint. EventSource will retry the connection.
    ConnectionError(surf::Exception),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Retry => write!(f, "the connection was closed by the server, retrying."),
            Self::ConnectionError(inner) => inner.fmt(f),
        }
    }
}

impl std::error::Error for Error {}

type EventStream = sse_codec::DecodeStream<surf::Response>;
type ConnectionFuture =
    Pin<Box<dyn Future<Output = Result<surf::Response, surf::Exception>> + Unpin>>;

/// Represents the internal state machine.
enum ConnectState {
    /// We're receiving messages.
    Streaming(EventStream),
    /// We're connecting to the endpoint.
    Connecting(ConnectionFuture),
    /// We're waiting to retry.
    WaitingToRetry(Delay),
    /// We're not doing anything. Currently only used as a default value. This can be used for the Closed state later.
    Idle,
}

/// A Server-Sent Events/Event Sourcing client, similar to [`EventSource`][EventSource] in the browser.
///
/// [EventSource]: https://developer.mozilla.org/en-US/docs/Web/API/EventSource
pub struct EventSource {
    url: Url,
    retry_time: Duration,
    last_event_id: Option<String>,
    state: ConnectState,
}

impl EventSource {
    /// Create a new connection.
    pub fn new(url: Url) -> Self {
        let mut client = Self {
            url,
            retry_time: Duration::from_secs(3),
            last_event_id: None,
            state: ConnectState::Idle,
        };
        client.start_connect();
        client
    }

    /// Get the URL that this client connects to.
    pub fn url(&self) -> &Url {
        &self.url
    }

    /// Get the state of the connection. See the documentation for `ReadyState`.
    pub fn ready_state(&self) -> ReadyState {
        match self.state {
            ConnectState::Streaming(_) => ReadyState::Open,
            ConnectState::Connecting(_) | ConnectState::WaitingToRetry(_) => ReadyState::Connecting,
            ConnectState::Idle => unreachable!("ReadyState::Closed"),
        }
    }

    /// Get the current retry timeout. If the connection dies, it is reopened after this time.
    pub fn retry_time(&self) -> Duration {
        self.retry_time
    }

    /// Override the retry timeout. If the connection dies, it is reopened after this time. Note
    /// that the timeout will be reset again if the server sends a new `retry:` message.
    pub fn set_retry_time(&mut self, duration: Duration) {
        self.retry_time = duration;
    }

    fn start_connect(&mut self) {
        let request = surf::get(&self.url);
        // If the EventSource object's last event ID string is not the empty string, set `Last-Event-ID`/last event ID string, encoded as UTF-8, in request's header list.
        let request = match &self.last_event_id {
            Some(id) => request.set_header("Last-Event-ID", id),
            None => request,
        };
        self.state = ConnectState::Connecting(Box::pin(request));
    }

    fn start_retry(&mut self) {
        self.state = ConnectState::WaitingToRetry(Delay::new(self.retry_time));
    }

    fn start_receiving(&mut self, response: surf::Response) {
        self.state = ConnectState::Streaming(sse_codec::decode_stream(response));
    }
}

impl Stream for EventSource {
    type Item = Result<Event, Error>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match &mut self.state {
            ConnectState::Streaming(event_stream) => {
                match event_stream.poll_next_unpin(cx) {
                    Poll::Pending => Poll::Pending,
                    Poll::Ready(Some(Ok(event))) => match event {
                        sse_codec::Event::Message { event, data } => {
                            Poll::Ready(Some(Ok(Event { event, data })))
                        }
                        sse_codec::Event::Retry { retry } => {
                            self.retry_time = Duration::from_millis(retry);
                            Poll::Pending
                        }
                        sse_codec::Event::LastEventId { id } => {
                            self.last_event_id = if id.is_empty() { None } else { Some(id) };
                            Poll::Pending
                        }
                    },
                    Poll::Ready(Some(Err(_))) => {
                        // we care even less about "incorrect" messages than sse_codec does!
                        Poll::Pending
                    }
                    // Clients will reconnect if the connection is closed.
                    Poll::Ready(None) => {
                        self.start_retry();
                        let error = Error::Retry;
                        Poll::Ready(Some(Err(error)))
                    }
                }
            }

            ConnectState::WaitingToRetry(timer) => {
                match timer.poll_unpin(cx) {
                    Poll::Pending => (),
                    Poll::Ready(()) => {
                        self.start_connect();
                    }
                }
                Poll::Pending
            }

            ConnectState::Connecting(connecting) => {
                match connecting.poll_unpin(cx) {
                    Poll::Pending => Poll::Pending,
                    // A client can be told to stop reconnecting using the HTTP 204 No Content response code.
                    Poll::Ready(Ok(response)) if response.status() == 204 => Poll::Ready(None),
                    Poll::Ready(Ok(response)) => {
                        self.start_receiving(response);
                        Poll::Pending
                    }
                    Poll::Ready(Err(error)) => {
                        self.start_retry();
                        let error = Error::ConnectionError(error);
                        Poll::Ready(Some(Err(error)))
                    }
                }
            }

            ConnectState::Idle => unreachable!(),
        }
    }
}
