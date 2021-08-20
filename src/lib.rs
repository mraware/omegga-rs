use std::{
    future::Future,
    pin::Pin,
    sync::{
        atomic::{AtomicI32, Ordering},
        Arc,
    },
    task::{Context, Poll},
};

use dashmap::{mapref::entry::Entry, DashMap};
use serde_json::Value;
use tokio::{
    io::{stdin, AsyncBufReadExt, BufReader},
    sync::{
        mpsc::{self, UnboundedReceiver},
        oneshot,
    },
};

pub mod rpc;

pub type RpcEventReceiver = UnboundedReceiver<rpc::Message>;

pub struct Omegga {
    pub awaiter_txs: Arc<DashMap<rpc::RequestId, oneshot::Sender<rpc::Response>>>,
    request_id: Arc<AtomicI32>,
}

impl Omegga {
    /// Create a new Omegga instance.
    pub fn new() -> Self {
        Self {
            awaiter_txs: Arc::new(DashMap::new()),
            request_id: Arc::new(AtomicI32::new(-1)),
        }
    }

    /// Spawn the listener.
    pub fn spawn(&self) -> RpcEventReceiver {
        let (tx, rx) = mpsc::unbounded_channel::<rpc::Message>();
        let awaiter_txs = Arc::clone(&self.awaiter_txs);
        tokio::spawn(async move {
            let reader = BufReader::new(stdin());
            let mut lines = reader.lines();
            while let Some(line) = lines.next_line().await.unwrap() {
                let message: rpc::Message = match serde_json::from_str(&line) {
                    Ok(v) => v,
                    Err(_) => continue,
                };

                match message {
                    // Handle responses
                    rpc::Message::Response {
                        id, result, error, ..
                    } => {
                        if let Entry::Occupied(entry) = awaiter_txs.entry(id) {
                            let (id, sender) = entry.remove_entry();
                            let _ = sender.send(rpc::Response { id, result, error });
                        }
                    }
                    // Otherwise, send everything else
                    _ => {
                        let _ = tx.send(message);
                    }
                };
            }
        });
        rx
    }

    /// Write out an RPC message.
    pub fn write(&self, message: rpc::Message) {
        println!("{}", serde_json::to_string(&message).unwrap());
    }

    /// Write out an RPC notification.
    pub fn write_notification(&self, method: impl Into<String>, params: Option<Value>) {
        self.write(rpc::Message::notification(method.into(), params));
    }

    /// Write out an RPC response.
    pub fn write_response(
        &self,
        id: rpc::RequestId,
        params: Option<Value>,
        error: Option<rpc::Error>,
    ) {
        self.write(rpc::Message::response(id, params, error));
    }

    /// Write out an RPC request.
    ///
    /// **Note:** This does not internally expect a response from the server.
    /// Prefer using [`request`](Omegga::request) over this for the ability to
    /// await a response from the RPC server.
    pub fn write_request(
        &self,
        id: rpc::RequestId,
        method: impl Into<String>,
        params: Option<Value>,
    ) {
        self.write(rpc::Message::request(id, method.into(), params));
    }

    /// Request a response from the RPC server.
    /// This returns a `ResponseAwaiter`, a `Future` that awaits a response.
    pub fn request(&self, method: impl Into<String>, params: Option<Value>) -> ResponseAwaiter {
        // fetch the next ID
        let id = self.request_id.fetch_sub(-1, Ordering::SeqCst);

        // write out the request
        self.write_request(rpc::RequestId::Int(id), method, params);

        // create a channel to send the response over
        let (tx, rx) = oneshot::channel::<rpc::Response>();

        // insert the transmitter into the dashmap
        self.awaiter_txs.insert(rpc::RequestId::Int(id), tx);

        // return back with an awaiter to await the receiver
        ResponseAwaiter(rx)
    }
}

impl Default for Omegga {
    fn default() -> Self {
        Self::new()
    }
}

/// A future that waits for the server to respond.
/// This will await indefinitely, so use with Tokio's `select!`
/// macro to impose a timeout.
pub struct ResponseAwaiter(oneshot::Receiver<rpc::Response>);

impl Future for ResponseAwaiter {
    type Output = Response;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match Pin::new(&mut self.0).poll(cx) {
            // we received a response, filter between a real result or an RPC error
            Poll::Ready(Ok(response)) => Poll::Ready(match response.error {
                Some(e) => Response::RpcError(e),
                None => Response::Ok(response.result),
            }),

            // no response received, the channel errored
            Poll::Ready(Err(error)) => Poll::Ready(Response::RecvError(error)),

            // we are still waiting
            Poll::Pending => Poll::Pending,
        }
    }
}

/// A response from Omegga's RPC server. Can be an optional value,
/// an RPC error (see [`rpc::Error`](rpc::Error)), or a channel
/// receive error (see [`oneshot::error::RecvError`](oneshot::error::RecvError))
#[derive(Debug)]
pub enum Response {
    /// The response succeeded and did not give an error.
    Ok(Option<Value>),

    /// The response was received but gave an error.
    RpcError(rpc::Error),

    /// An error occurred while awaiting the awaiter's channel.
    RecvError(oneshot::error::RecvError),
}
