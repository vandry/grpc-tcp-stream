pub mod pb {
    tonic::include_proto!("tcpstream");
    pub(crate) const FILE_DESCRIPTOR_SET: &[u8] = tonic::include_file_descriptor_set!("fdset");
}

use async_stream::stream;
use comprehensive::{NoDependencies, Resource, ResourceDependencies};
use comprehensive_grpc::GrpcService;
use futures::future::Either;
use futures::{pin_mut, Stream, StreamExt};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use tokio::io::{AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpSocket, TcpStream};
use tonic::{Request, Response, Status, Streaming};

struct StreamServiceImpl(HashMap<String, SocketAddr>);

#[derive(Clone, Debug)]
struct MethodAndBackend(String, SocketAddr);

#[derive(Debug)]
enum ParseMethodAndBackendError {
    NoSeparator,
    AddrParseError(std::net::AddrParseError),
}

impl std::fmt::Display for ParseMethodAndBackendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match *self {
            Self::NoSeparator => write!(f, "Expected: method=address"),
            Self::AddrParseError(ref e) => e.fmt(f),
        }
    }
}

impl From<std::net::AddrParseError> for ParseMethodAndBackendError {
    fn from(e: std::net::AddrParseError) -> Self {
        Self::AddrParseError(e)
    }
}

impl std::error::Error for ParseMethodAndBackendError {}

impl std::str::FromStr for MethodAndBackend {
    type Err = ParseMethodAndBackendError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.find('=') {
            None => Err(ParseMethodAndBackendError::NoSeparator),
            Some(i) => Ok(MethodAndBackend(s[..i].to_string(), s[i + 1..].parse()?)),
        }
    }
}

#[derive(clap::Args, Debug)]
struct Args {
    #[arg(long)]
    backend: Vec<MethodAndBackend>,
}

impl Resource for StreamServiceImpl {
    type Args = Args;
    type Dependencies = comprehensive::NoDependencies;
    const NAME: &str = "StreamServiceImpl";

    fn new(_: NoDependencies, args: Args) -> Result<Self, Box<dyn std::error::Error>> {
        Ok(Self(
            args.backend.into_iter().map(|mb| (mb.0, mb.1)).collect(),
        ))
    }
}

async fn write_to_backend<T>(mut in_stream: Streaming<pb::Bytes>, mut tx: T) -> Result<(), Status>
where
    T: AsyncWrite + Unpin,
{
    while let Some(req) = in_stream.next().await {
        if let Some(b) = req?.b {
            if !b.is_empty() {
                tx.write_all(&b).await?;
            }
        }
    }
    Ok(())
}

fn stream_bytes(
    in_stream: Streaming<pb::Bytes>,
    backend: TcpStream,
) -> impl Stream<Item = Result<pb::Bytes, Status>> {
    let (mut rx, tx) = backend.into_split();
    let task = tokio::spawn(async move {
        let ret = write_to_backend(in_stream, tx).await;
        log::info!("client disconnected");
        ret
    });
    stream! {
        let mut buf = [0; 4096];
        pin_mut!(task);
        loop {
            let reader = rx.read(&mut buf);
            pin_mut!(reader);
            match futures::future::select(task, reader).await {
                Either::Left((Err(e), _)) => {
                    yield Err(Status::internal(e.to_string()));  // join error
                    break;
                }
                Either::Left((Ok(Err(e)), _)) => {
                    yield Err(e);  // error from tx.write_all
                    break;
                }
                Either::Left((Ok(Ok(())), _)) => {
                    break;  // in_stream finished
                }
                Either::Right((Err(e), _)) => {
                    yield Err(e.into());  // error from rx.read
                    break;
                }
                Either::Right((Ok(0), _)) => {
                    break;  // eof from rx.read
                }
                Either::Right((Ok(size), ret_task)) => {
                    task = ret_task;
                    yield Ok(pb::Bytes{b: Some(buf[..size].to_vec())});
                }
            }
        }
    }
}

async fn connect_backend(addr: SocketAddr) -> Result<TcpStream, Status> {
    let sock = TcpSocket::new_v6()?;
    Ok(sock.connect(addr).await?)
}

#[derive(Clone)]
struct OriginalMethod(String);

#[tonic::async_trait]
impl pb::stream_server::Stream for StreamServiceImpl {
    type StreamStream = Pin<Box<dyn Stream<Item = Result<pb::Bytes, Status>> + Send>>;

    async fn stream(
        &self,
        req: Request<Streaming<pb::Bytes>>,
    ) -> Result<Response<Self::StreamStream>, Status> {
        let Some(om) = req.extensions().get::<OriginalMethod>() else {
            return Err(Status::unimplemented(""));
        };
        log::info!("client connected {}", om.0);
        let Some(addr) = self.0.get(&om.0) else {
            return Err(Status::unimplemented(""));
        };
        let backend = connect_backend(*addr).await?;
        Ok(Response::new(Box::pin(stream_bytes(
            req.into_inner(),
            backend,
        ))))
    }
}

struct StreamDynamicMethod<T>(pb::stream_server::StreamServer<T>);

impl<T> StreamDynamicMethod<T> {
    pub fn from_arc(inner: Arc<T>) -> Self {
        Self(pb::stream_server::StreamServer::from_arc(inner))
    }
}

impl<T, B> tower_service::Service<http::Request<B>> for StreamDynamicMethod<T>
where
    T: pb::stream_server::Stream,
    B: http_body::Body + Send + 'static,
    B::Error: Into<Box<dyn std::error::Error + Send + Sync + 'static>> + Send + 'static,
{
    type Response =
        <pb::stream_server::StreamServer<T> as tower_service::Service<http::Request<B>>>::Response;
    type Error =
        <pb::stream_server::StreamServer<T> as tower_service::Service<http::Request<B>>>::Error;
    type Future =
        <pb::stream_server::StreamServer<T> as tower_service::Service<http::Request<B>>>::Future;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        <pb::stream_server::StreamServer<T> as tower_service::Service<http::Request<B>>>::poll_ready(
            &mut self.0,
            cx,
        )
    }

    fn call(&mut self, mut req: http::Request<B>) -> Self::Future {
        let uri = req.uri_mut();
        let mut parts = uri.clone().into_parts();
        if let Some(ref mut pq) = parts.path_and_query {
            let s = pq.as_str();
            if let Some(slash) = s[1..].find('/') {
                let om = OriginalMethod(s[slash + 2..].to_string());
                if let Ok(new_pq) = http::uri::PathAndQuery::from_maybe_shared(format!(
                    "{}/Stream",
                    &s[..slash + 1]
                )) {
                    *pq = new_pq;
                    if let Ok(new_uri) = parts.try_into() {
                        *uri = new_uri;
                    }
                }
                req.extensions_mut().insert(om);
            }
        }
        self.0.call(req)
    }
}

impl<T> tonic::server::NamedService for StreamDynamicMethod<T> {
    const NAME: &'static str = pb::stream_server::StreamServer::<T>::NAME;
}

impl<T> Clone for StreamDynamicMethod<T> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

#[derive(GrpcService)]
#[implementation(StreamServiceImpl)]
#[service(StreamDynamicMethod)]
#[descriptor(pb::FILE_DESCRIPTOR_SET)]
struct StreamService;

#[derive(ResourceDependencies)]
struct TopDependencies {
    _l: Arc<StreamService>,
    _d: Arc<comprehensive::diag::HttpServer>,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::init();
    #[cfg(feature = "tls")]
    let _ = tokio_rustls::rustls::crypto::aws_lc_rs::default_provider().install_default();
    comprehensive::Assembly::<TopDependencies>::new()?
        .run()
        .await
}
