use std::{collections::HashMap, fmt::Debug, net::SocketAddr, ops::Deref, pin::Pin, sync::Arc};

use anyhow::Result;
use futures::{Future, SinkExt, StreamExt};
use parking_lot::Mutex;
use s2n_quic::{client::Connect, connection::Handle, provider::event::default::Subscriber, stream::BidirectionalStream, Client, Connection};
use serde::{Deserialize, Serialize};
use tokio_serde::formats;
use tokio_util::codec::{Framed, LengthDelimitedCodec};

use crate::MtlsProvider;

pub static CA_CERT_PEM: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/certs/ca.crt");
pub static MY_CERT_PEM: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/certs/server.crt");
pub static MY_KEY_PEM: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/certs/server.key");

///
/// 1. 连接 master 节点, 获取到 master 节点的 connection
/// 2. 通过 connection 创建 node 实例
/// 3. 通过 node 实例注册当前节点到 master 节点
/// 4. master 节点返回 peer 和 master 节点的 services, 并保存到 node 实例
///

pub type FramedIO = Framed<BidirectionalStream, LengthDelimitedCodec>;

#[derive(Clone)]
pub struct P2pRt {
    pub client: Client,
    pub peers: Arc<Mutex<Vec<Peer>>>,
    pub service: Arc<Service>,
}

impl P2pRt {
    pub async fn new(service: Service) -> Result<Self> {
        Ok(Self {
            client: create_client("0.0.0.0:0".parse()?).await?,
            peers: Arc::new(Mutex::new(vec![])),
            service: Arc::new(service),
        })
    }

    pub async fn open_stream(&self, addr: SocketAddr, service_name: impl Into<ServiceName>) -> Result<FramedIO> {
        if self.peers.lock().iter().find(|peer| peer.openner.remote_addr() == Ok(addr)).is_none() {
            let mut conn = self.client.connect(Connect::new(addr).with_server_name("localhost")).await?;
            conn.keep_alive(true)?;
            self.clone().serve(conn).await;
        }

        let mut openner = self
            .peers
            .lock()
            .iter()
            .find(|peer| peer.remote_addr() == Ok(addr))
            .ok_or(anyhow::anyhow!("no peer"))?
            .openner
            .clone();

        let stream = openner.open_bidirectional_stream().await?;
        let mut framed_io = LengthDelimitedCodec::builder().max_frame_length(1024 * 1024 * 4).new_framed(stream);

        let negotiate = Negotiate {
            service_name: service_name.into(),
        };
        framed_io.send(rmp_serde::to_vec(&negotiate)?.into()).await?;

        Ok(framed_io)
    }
}

impl P2pRt {
    pub async fn spawn_with_addr(self, addr: SocketAddr) -> Result<Self> {
        let this = self.clone();

        let mut server = create_server(addr).await?;
        println!("server addr: {}", server.local_addr()?);

        tokio::spawn(async move {
            while let Some(conn) = server.accept().await {
                this.clone().serve(conn).await;
            }
        });

        Ok(self.clone())
    }

    async fn serve(self, conn: Connection) {
        let (handle, mut acceptor) = conn.split();

        self.peers.lock().push(Peer::new(handle.clone()));

        tokio::spawn(async move {
            while let Ok(Some(stream)) = acceptor.accept_bidirectional_stream().await {
                let this = self.clone();

                tokio::spawn(async move {
                    let mut framed_io = LengthDelimitedCodec::builder().max_frame_length(1024 * 1024 * 4).new_framed(stream);

                    let bytes = framed_io.next().await.ok_or(anyhow::anyhow!("no bytes"))??;
                    let negotiate = rmp_serde::from_slice::<Negotiate>(&bytes).map_err(|e| anyhow::anyhow!("rmp_serde::from_slice: {}", e))?;

                    let handler = this.service.handlers.get(&negotiate.service_name).ok_or(anyhow::anyhow!("no handler"))?;

                    handler(framed_io, this.clone()).await;

                    Result::<()>::Ok(())
                });
            }

            self.peers.lock().retain(|peer| peer.remote_addr() != handle.remote_addr());
        });
    }
}

pub fn framed_msgpack<Msg>(framed_io: FramedIO) -> tokio_serde::Framed<FramedIO, Msg, Msg, formats::MessagePack<Msg, Msg>> {
    tokio_serde::Framed::new(framed_io, formats::MessagePack::default())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Negotiate {
    service_name: ServiceName,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct ServiceName(String);

impl ServiceName {
    pub fn new<I: Into<String>>(name: I) -> Self {
        Self(name.into())
    }
}

impl From<&str> for ServiceName {
    fn from(name: &str) -> Self {
        Self::new(name)
    }
}

impl From<String> for ServiceName {
    fn from(name: String) -> Self {
        Self::new(name)
    }
}

// =====

#[derive(Debug, Clone)]
pub struct Peer {
    openner: Handle,
}

impl Deref for Peer {
    type Target = Handle;

    fn deref(&self) -> &Self::Target {
        &self.openner
    }
}

impl Peer {
    pub fn new(openner: Handle) -> Self {
        Self { openner }
    }
}

pub struct Service {
    handlers: HashMap<ServiceName, Box<dyn Fn(FramedIO, P2pRt) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>>,
}

impl Service {
    pub fn new() -> Self {
        Self { handlers: HashMap::new() }
    }

    pub fn add_service<S, H, F>(mut self, name: S, handler: H) -> Self
    where
        S: Into<ServiceName>,
        H: Fn(FramedIO, P2pRt) -> F + Send + Sync + 'static,
        F: Future<Output = ()> + Send + 'static,
    {
        self.handlers
            .insert(name.into(), Box::new(move |framed_io, p2p_rt| Box::pin(handler(framed_io, p2p_rt))));
        self
    }
}

// =====

async fn create_client(addr: SocketAddr) -> Result<s2n_quic::Client> {
    let client = s2n_quic::Client::builder()
        .with_event(Subscriber::default())?
        .with_tls(MtlsProvider::new(CA_CERT_PEM, MY_CERT_PEM, MY_KEY_PEM).await?)?
        .with_io(addr)?
        .start()
        .map_err(|e| anyhow::anyhow!("failed to create client: {:?}", e))?;

    Ok(client)
}

async fn create_server(addr: SocketAddr) -> Result<s2n_quic::Server> {
    let server = s2n_quic::Server::builder()
        .with_event(Subscriber::default())?
        .with_tls(MtlsProvider::new(CA_CERT_PEM, MY_CERT_PEM, MY_KEY_PEM).await?)?
        .with_io(addr)?
        .start()
        .map_err(|e| anyhow::anyhow!("failed to create server: {:?}", e))?;

    Ok(server)
}
