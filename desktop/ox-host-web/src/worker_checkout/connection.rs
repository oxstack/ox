//! Keep the network connection responsive while either host is running local Git.
//! Backpressure is measured in bounded archive chunks, independent of repository size.
use futures_util::{Sink, SinkExt, Stream, StreamExt};
use std::{io, time::Duration};
use tokio::{
    sync::{mpsc, oneshot},
    time::{interval, timeout},
};
use tokio_tungstenite::tungstenite;
const HEARTBEAT: Duration = Duration::from_secs(15);
pub(crate) const IDLE: Duration = Duration::from_secs(90);

pub(crate) trait Frame: Send + 'static {
    fn ping() -> Self;
    fn is_heartbeat(&self) -> bool;
}
impl Frame for tungstenite::Message {
    fn ping() -> Self {
        Self::Ping(Default::default())
    }
    fn is_heartbeat(&self) -> bool {
        matches!(self, Self::Ping(_) | Self::Pong(_))
    }
}
impl Frame for axum::extract::ws::Message {
    fn ping() -> Self {
        Self::Ping(Default::default())
    }
    fn is_heartbeat(&self) -> bool {
        matches!(self, Self::Ping(_) | Self::Pong(_))
    }
}
type Outbound<M> = (M, oneshot::Sender<()>);
pub(crate) struct Connection<M> {
    out: mpsc::Sender<Outbound<M>>,
    incoming: mpsc::Receiver<M>,
    task: tokio::task::JoinHandle<()>,
}
impl<M> Drop for Connection<M> {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl<M: Frame> Connection<M> {
    pub fn open<S, E>(socket: S) -> Self
    where
        S: Sink<M, Error = E> + Stream<Item = Result<M, E>> + Unpin + Send + 'static,
        E: Send + 'static,
    {
        let (out, mut outgoing) = mpsc::channel::<Outbound<M>>(1);
        let (incoming_tx, incoming) = mpsc::channel(4);
        let task = tokio::spawn(async move {
            let (mut writer, mut reader) = socket.split();
            let write = async {
                let mut ping = interval(HEARTBEAT);
                loop {
                    let (frame, ack) = tokio::select! {
                        next = outgoing.recv() => match next {
                            Some((m, ack)) => (m,Some(ack)), None => break,
                        },
                        _ = ping.tick() => (M::ping(),None)
                    };
                    if !matches!(timeout(IDLE, writer.send(frame)).await, Ok(Ok(()))) {
                        break;
                    }
                    if let Some(ack) = ack {
                        let _ = ack.send(());
                    }
                }
            };
            let read = async {
                loop {
                    let Ok(Some(Ok(frame))) = timeout(IDLE, reader.next()).await else {
                        break;
                    };
                    if !frame.is_heartbeat() && incoming_tx.send(frame).await.is_err() {
                        break;
                    }
                }
            };
            tokio::select! { _=read=>(), _=write=>() }
        });
        Self {
            out,
            incoming,
            task,
        }
    }
    pub async fn send(&self, frame: M) -> io::Result<()> {
        let (ack, wait) = oneshot::channel();
        self.out
            .send((frame, ack))
            .await
            .map_err(|_| disconnected())?;
        wait.await.map_err(|_| disconnected())
    }
    pub async fn next(&mut self) -> io::Result<M> {
        self.incoming.recv().await.ok_or_else(disconnected)
    }
}

impl Connection<axum::extract::ws::Message> {
    /// Let the peer read the terminal reply before closing the transport.
    pub async fn finish(&mut self) {
        if self
            .send(axum::extract::ws::Message::Close(None))
            .await
            .is_ok()
        {
            // A peer may close its connection as soon as it reads the reply.
            let _ = timeout(HEARTBEAT, self.next()).await;
        }
    }
}

fn disconnected() -> io::Error {
    io::Error::new(
        io::ErrorKind::ConnectionAborted,
        "Worker checkout connection closed or timed out. Retry the job to recover its receipt.",
    )
}
