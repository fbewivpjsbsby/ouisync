//! Client and Server than run in different processes on the same device.

pub use interprocess::local_socket::{
    tokio::LocalSocketListener, LocalSocketName, ToLocalSocketName,
};

use super::{
    utils::{self, Invoker},
    Client, Server,
};
use crate::{
    error::Result,
    protocol::{Request, Response},
    state::ServerState,
};
use async_trait::async_trait;
use interprocess::local_socket::tokio::{LocalSocketStream, OwnedReadHalf, OwnedWriteHalf};
use std::{io, sync::Arc};
use tokio::task::JoinSet;
use tokio_util::{
    codec::{length_delimited::LengthDelimitedCodec, Framed, FramedRead, FramedWrite},
    compat::{Compat, FuturesAsyncReadCompatExt, FuturesAsyncWriteCompatExt},
};

pub struct LocalServer {
    listener: LocalSocketListener,
}

impl LocalServer {
    pub fn bind<'a>(name: impl ToLocalSocketName<'a>) -> io::Result<Self> {
        let listener = LocalSocketListener::bind(name)?;

        Ok(Self { listener })
    }
}

#[async_trait]
impl Server for LocalServer {
    async fn run(self, state: Arc<ServerState>) {
        let mut connections = JoinSet::new();

        loop {
            match self.listener.accept().await {
                Ok(socket) => {
                    let socket = make_socket(socket);
                    let state = state.clone();

                    connections.spawn(async move {
                        utils::handle_server_connection(socket, &state).await
                    });
                }
                Err(error) => {
                    tracing::error!(?error, "failed to accept client");
                    break;
                }
            }
        }
    }
}

pub struct LocalClient {
    invoker: Invoker<Reader, Writer>,
}

impl LocalClient {
    pub async fn connect<'a>(name: impl ToLocalSocketName<'a>) -> io::Result<Self> {
        let socket = LocalSocketStream::connect(name).await?;
        let (reader, writer) = socket.into_split();
        let reader = make_reader(reader);
        let writer = make_writer(writer);

        Ok(Self {
            invoker: Invoker::new(reader, writer),
        })
    }
}

#[async_trait]
impl Client for LocalClient {
    async fn invoke(&self, request: Request) -> Result<Response> {
        self.invoker.invoke(request).await
    }
}

type Socket = Framed<Compat<LocalSocketStream>, LengthDelimitedCodec>;
type Reader = FramedRead<Compat<OwnedReadHalf>, LengthDelimitedCodec>;
type Writer = FramedWrite<Compat<OwnedWriteHalf>, LengthDelimitedCodec>;

fn make_socket(inner: LocalSocketStream) -> Socket {
    Framed::new(inner.compat(), LengthDelimitedCodec::new())
}

fn make_reader(inner: OwnedReadHalf) -> Reader {
    FramedRead::new(inner.compat(), LengthDelimitedCodec::new())
}

fn make_writer(inner: OwnedWriteHalf) -> Writer {
    FramedWrite::new(inner.compat_write(), LengthDelimitedCodec::new())
}
