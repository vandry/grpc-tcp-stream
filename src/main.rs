pub mod pb {
    tonic::include_proto!("tcpstream");
}

use pb::stream_client::StreamClient;

use async_stream::stream;
use futures::{Stream, StreamExt};
use std::task::{Context, Poll};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;
use tonic::transport::Channel;
use tonic::Status;

fn stdin_stream() -> impl Stream<Item = pb::Bytes> {
    stream! {
        let mut stdin = tokio::io::stdin();
        let mut buf = [0; 4096];
        loop {
            let size = stdin.read(&mut buf).await.expect("read stdin");
            if size == 0 {
                break;
            }
            yield pb::Bytes{b: Some(buf[..size].to_vec())};
        }
    }
}

async fn write_stdout(mut rx: mpsc::Receiver<pb::Bytes>) {
    let mut stdout = tokio::io::stdout();
    let mut dirty = false;
    loop {
        let item = if dirty {
            match rx.try_recv() {
                Ok(i) => i,
                Err(mpsc::error::TryRecvError::Empty) => {
                    stdout.flush().await.expect("flush stdout");
                    dirty = false;
                    continue;
                }
                Err(mpsc::error::TryRecvError::Disconnected) => {
                    break;
                }
            }
        } else {
            match rx.recv().await {
                Some(i) => i,
                None => {
                    break;
                }
            }
        };
        if let Some(b) = item.b {
            if !b.is_empty() {
                stdout.write_all(&b).await.expect("write stdout");
                dirty = true;
            }
        }
    }
}

async fn read_from_server<T>(
    mut stream: T,
    tx: mpsc::Sender<pb::Bytes>,
) -> Result<(), Box<dyn std::error::Error>>
where
    T: Stream<Item = Result<pb::Bytes, Status>> + Unpin,
{
    while let Some(item) = stream.next().await {
        tx.send(item?).await?;
    }
    Ok(())
}

async fn stream_bytes(
    client: &mut StreamClient<MethodTranslator<'_, Channel>>,
) -> Result<(), Box<dyn std::error::Error>> {
    let in_stream = stdin_stream();
    let stream = client.stream(in_stream).await?.into_inner();
    let (tx, rx) = mpsc::channel(20);
    let task = tokio::spawn(async move {
        write_stdout(rx).await;
    });
    let ret = read_from_server(stream, tx).await;
    task.await?;
    ret
}

struct MethodTranslator<'a, T> {
    inner: T,
    method: &'a str,
}

impl<'a, T> MethodTranslator<'a, T> {
    fn new(inner: T, method: &'a str) -> Self {
        Self { inner, method }
    }
}

impl<T, B> tonic::client::GrpcService<B> for MethodTranslator<'_, T>
where
    T: tonic::client::GrpcService<B>,
{
    type ResponseBody = T::ResponseBody;
    type Error = T::Error;
    type Future = T::Future;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, mut req: http::Request<B>) -> Self::Future {
        let uri = req.uri_mut();
        let mut parts = uri.clone().into_parts();
        if let Some(ref mut pq) = parts.path_and_query {
            let s = pq.as_str();
            if let Some(slash) = s[1..].find('/') {
                if let Ok(new_pq) = http::uri::PathAndQuery::from_maybe_shared(format!(
                    "{}/{}",
                    &s[..slash + 1],
                    self.method
                )) {
                    *pq = new_pq;
                    if let Ok(new_uri) = parts.try_into() {
                        *uri = new_uri;
                    }
                }
            }
        }
        self.inner.call(req)
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    #[cfg(feature = "tls")]
    let _ = tokio_rustls::rustls::crypto::aws_lc_rs::default_provider().install_default();
    let args: Vec<_> = std::env::args_os().collect();
    if args.len() != 3 {
        eprintln!(
            "Usage: {} method grpc-proxy-addr",
            args[0].to_string_lossy()
        );
        std::process::exit(3);
    }
    let method = args[1].to_string_lossy();
    let addr: tonic::transport::Uri = args[2].to_string_lossy().parse()?;
    let conn = tonic::transport::Endpoint::new(addr)?.connect().await?;
    let mut client = StreamClient::new(MethodTranslator::new(conn, &method));
    match stream_bytes(&mut client).await {
        Ok(()) => {
            std::process::exit(0);
        }
        Err(e) => {
            eprintln!("{}", e);
            std::process::exit(1);
        }
    }
}
