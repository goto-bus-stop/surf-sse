//! An implementation of the [EventSource][] API using [Surf][].
//!
//! # Logging
//!
//! [`surf-sse`][surf-sse] uses the [`log`][log] crate for some rudimentary connection logging. If you need to debug
//! an EventSource connection, enable trace logging for the `surf-sse` target. For example, with
//! [`env_logger`][env_logger]:
//! ```bash
//! RUST_LOG=surf-sse=trace \
//! cargo run
//! ```
//!
//! # Examples
//! ```rust,no_run
//! # async_std::task::block_on(async move {
//! #
//! use futures_util::stream::TryStreamExt; // for try_next()
//! use surf_sse::EventSource;
//!
//! let mut events = EventSource::new("https://announce.u-wave.net/events".parse().unwrap());
//!
//! while let Some(message) = events.try_next().await.unwrap() {
//!     dbg!(message);
//! }
//! #
//! # });
//! ```
//!
//! [EventSource]: https://developer.mozilla.org/en-US/docs/Web/API/EventSource
//! [Surf]: https://github.com/http-rs/surf
//! [surf-sse]: https://github.com/goto-bus-stop/surf-sse
//! [log]: https://docs.rs/log
//! [env_logger]: https://docs.rs/env_logger

#![deny(future_incompatible)]
#![deny(nonstandard_style)]
#![deny(rust_2018_idioms)]
#![deny(unsafe_code)]
#![warn(missing_docs)]
#![warn(unused)]

use futures_core::stream::Stream;
use futures_timer::Delay;
use log::trace;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;
pub use surf::Url;

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
    ConnectionError(surf::Error),
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

/// Wrapper for a dynamic Future type that adds an opaque Debug implementation.
struct DynDebugFuture<T>(Pin<Box<dyn Future<Output = T>>>);
impl<T> Future for DynDebugFuture<T> {
    type Output = T;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        Pin::new(&mut self.0).poll(cx)
    }
}

impl<T> fmt::Debug for DynDebugFuture<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[opaque Future type]")
    }
}

type EventStream = sse_codec::DecodeStream<surf::Response>;
type ConnectionFuture = DynDebugFuture<Result<surf::Response, surf::Error>>;

/// Represents the internal state machine.
#[derive(Debug)]
#[allow(clippy::large_enum_variant)] // `EventStream` is large but it's the active one most of the time
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
#[derive(Debug)]
pub struct EventSource {
    client: surf::Client,
    url: Url,
    retry_time: Duration,
    last_event_id: Option<String>,
    state: ConnectState,
}

impl EventSource {
    /// Create a new connection.
    ///
    /// This constructor creates a new [`surf::Client`][] for the event sourcing connection. If you
    /// already have a [`surf::Client`][], consider using [`EventSource::with_client`][].
    ///
    /// [`surf::Client`]: https://docs.rs/surf/2.x/surf/struct.Client.html
    /// [`EventSource::with_client`]: #method.with_client
    pub fn new(url: Url) -> Self {
        Self::with_client(surf::client(), url)
    }

    /// Create a new connection.
    pub fn with_client(client: surf::Client, url: Url) -> Self {
        let mut event_source = Self {
            client,
            url,
            retry_time: Duration::from_secs(3),
            last_event_id: None,
            state: ConnectState::Idle,
        };
        event_source.start_connect();
        event_source
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
        trace!(target: "surf-sse", "connecting to {}", self.url);
        let mut request = surf::get(self.url.clone()).header("Accept", "text/event-stream");
        // If the EventSource object's last event ID string is not the empty string, set `Last-Event-ID`/last event ID string, encoded as UTF-8, in request's header list.
        if let Some(id) = &self.last_event_id {
            request = request.header("Last-Event-ID", id.as_str());
        }

        let client = self.client.clone();
        let request_future = Box::pin(async move { client.send(request).await });

        self.state = ConnectState::Connecting(DynDebugFuture(request_future));
    }

    fn start_retry(&mut self) {
        trace!(target: "surf-sse", "connection to {}, retrying in {:?}", self.url, self.retry_time);
        self.state = ConnectState::WaitingToRetry(Delay::new(self.retry_time));
    }

    fn start_receiving(&mut self, response: surf::Response) {
        trace!(target: "surf-sse", "connected to {}, now waiting for events", self.url);
        self.state = ConnectState::Streaming(sse_codec::decode_stream(response));
    }
}

impl Stream for EventSource {
    type Item = Result<Event, Error>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match &mut self.state {
            ConnectState::Streaming(event_stream) => {
                match Pin::new(event_stream).poll_next(cx) {
                    Poll::Pending => Poll::Pending,
                    Poll::Ready(Some(Ok(event))) => match event {
                        sse_codec::Event::Message { id, event, data } => {
                            self.last_event_id = id;
                            Poll::Ready(Some(Ok(Event { event, data })))
                        }
                        sse_codec::Event::Retry { retry } => {
                            self.retry_time = Duration::from_millis(retry);
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
                match Pin::new(timer).poll(cx) {
                    Poll::Pending => (),
                    Poll::Ready(()) => self.start_connect(),
                }
                Poll::Pending
            }

            ConnectState::Connecting(connecting) => {
                match Pin::new(connecting).poll(cx) {
                    Poll::Pending => Poll::Pending,
                    // A client can be told to stop reconnecting using the HTTP 204 No Content response code.
                    Poll::Ready(Ok(response)) if response.status() == 204 => Poll::Ready(None),
                    Poll::Ready(Ok(response)) => {
                        self.start_receiving(response);
                        self.poll_next(cx)
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

/// Extension trait with event sourcing methods for Surf clients.
///
/// ```rust,no_run
/// use surf_sse::ClientExt;
/// use futures_util::stream::StreamExt; // for `.next`
///
/// let client = surf::client();
/// let mut events = client.connect_event_source("https://announce.u-wave.net".parse().unwrap());
/// async_std::task::block_on(async move {
///     while let Some(event) = events.next().await {
///         dbg!(event.unwrap());
///     }
/// });
/// ```
pub trait ClientExt {
    /// Connect to an event sourcing / server-sent events endpoint.
    fn connect_event_source(&self, url: Url) -> EventSource;
}

impl ClientExt for surf::Client {
    fn connect_event_source(&self, url: Url) -> EventSource {
        EventSource::with_client(self.clone(), url)
    }
}
