//! One plain HTTP/1.1 request on a connection of its own: the PUTs that write a tree into the
//! upstream, and the GET of a node's `/metricz`.

use bytes::Bytes;
use http_body_util::{BodyExt as _, Full};
use hyper::Method;
use hyper::client::conn::http1;
use hyper_util::rt::TokioIo;
use tokio::net::TcpStream;

/// Sends `method` of `path` with `body` to the loopback address `address`: the answer's status
/// and body.
///
/// # Errors
/// The connection, the request or the answer failed, as text naming the request.
pub(crate) async fn call(
    address: &str,
    method: Method,
    path: &str,
    body: Vec<u8>,
) -> Result<(u16, Vec<u8>), String> {
    let what = format!("{method} http://{address}{path}");
    let request = hyper::Request::builder()
        .method(method)
        .uri(path)
        .header(hyper::header::HOST, address)
        .body(Full::new(Bytes::from(body)))
        .map_err(|why| format!("{what}: {why}"))?;
    let answer = async {
        let stream = TcpStream::connect(address).await?;
        let (mut sender, connection) = http1::handshake(TokioIo::new(stream)).await?;
        tokio::spawn(connection);
        let response = sender.send_request(request).await?;
        let status = response.status().as_u16();
        let bytes = response.into_body().collect().await?.to_bytes();
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>((status, bytes.to_vec()))
    };
    answer.await.map_err(|why| format!("{what}: {why}"))
}

#[cfg(test)]
#[path = "http_tests.rs"]
mod tests;
