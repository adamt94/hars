//! A minimal MQTT 3.1.1 client over TLS, enough for Tuya's push channel
//! (`tuya_sharing/mq.py` uses paho): connect with a username and password,
//! subscribe at QoS 0, receive publishes, keep the link alive with pings.
//! Nothing is ever published.

use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// paho's default keepalive.
pub const KEEPALIVE: Duration = Duration::from_secs(60);

/// How long connecting (TCP, TLS, CONNECT and CONNACK) may take.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Debug, thiserror::Error)]
pub enum MqttError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("connection refused, code {0}")]
    Refused(u8),
    #[error("protocol: {0}")]
    Protocol(&'static str),
    #[error("bad url {0}")]
    Url(String),
    #[error("no ping answer")]
    Timeout,
    #[error("{0}")]
    Connect(#[from] ha_http::Error),
}

fn put_str(buf: &mut Vec<u8>, s: &str) {
    buf.extend_from_slice(&(s.len() as u16).to_be_bytes());
    buf.extend_from_slice(s.as_bytes());
}

fn packet(kind: u8, body: &[u8]) -> Vec<u8> {
    let mut out = vec![kind];
    let mut len = body.len();
    loop {
        let mut b = (len % 128) as u8;
        len /= 128;
        if len > 0 {
            b |= 0x80;
        }
        out.push(b);
        if len == 0 {
            break;
        }
    }
    out.extend_from_slice(body);
    out
}

pub fn connect_packet(client_id: &str, user: &str, pass: &str, keepalive: u16) -> Vec<u8> {
    let mut b = Vec::new();
    put_str(&mut b, "MQTT");
    b.push(4); // 3.1.1
    b.push(0x80 | 0x40 | 0x02); // username, password, clean session
    b.extend_from_slice(&keepalive.to_be_bytes());
    put_str(&mut b, client_id);
    put_str(&mut b, user);
    put_str(&mut b, pass);
    packet(0x10, &b)
}

pub fn subscribe_packet(id: u16, topics: &[String]) -> Vec<u8> {
    let mut b = id.to_be_bytes().to_vec();
    for t in topics {
        put_str(&mut b, t);
        b.push(0);
    }
    packet(0x82, &b)
}

/// One received packet: its first byte and its body.
async fn read_packet<R: AsyncRead + Unpin>(r: &mut R) -> Result<(u8, Vec<u8>), MqttError> {
    let first = r.read_u8().await?;
    let mut len: usize = 0;
    let mut shift = 0;
    loop {
        let b = r.read_u8().await?;
        len |= ((b & 0x7f) as usize) << shift;
        if b & 0x80 == 0 {
            break;
        }
        shift += 7;
        if shift > 21 {
            return Err(MqttError::Protocol("length"));
        }
    }
    if len > 1 << 20 {
        return Err(MqttError::Protocol("packet too large"));
    }
    let mut body = vec![0; len];
    r.read_exact(&mut body).await?;
    Ok((first, body))
}

/// A publish's topic and payload, and its packet id when QoS > 0.
pub fn parse_publish(first: u8, body: &[u8]) -> Result<(String, Option<u16>, Vec<u8>), MqttError> {
    let qos = (first >> 1) & 3;
    if body.len() < 2 {
        return Err(MqttError::Protocol("publish"));
    }
    let tlen = u16::from_be_bytes([body[0], body[1]]) as usize;
    let mut at = 2 + tlen;
    if body.len() < at {
        return Err(MqttError::Protocol("publish topic"));
    }
    let topic = String::from_utf8_lossy(&body[2..at]).into_owned();
    let id = if qos > 0 {
        if body.len() < at + 2 {
            return Err(MqttError::Protocol("publish id"));
        }
        let id = u16::from_be_bytes([body[at], body[at + 1]]);
        at += 2;
        Some(id)
    } else {
        None
    };
    Ok((topic, id, body[at..].to_vec()))
}

pub trait Stream: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Stream for T {}

/// `ssl://host:port` or `tcp://host:port`: a connected socket.
pub async fn open(url: &str) -> Result<Box<dyn Stream>, MqttError> {
    let (scheme, rest) = url
        .split_once("://")
        .ok_or_else(|| MqttError::Url(url.into()))?;
    let (host, port) = rest
        .rsplit_once(':')
        .and_then(|(h, p)| Some((h.to_owned(), p.trim_end_matches('/').parse::<u16>().ok()?)))
        .ok_or_else(|| MqttError::Url(url.into()))?;
    match scheme {
        "tcp" | "mqtt" => Ok(Box::new(
            ha_http::stream::connect_tcp(&host, port, CONNECT_TIMEOUT).await?,
        )),
        "ssl" | "mqtts" | "tls" => Ok(Box::new(
            ha_http::stream::connect_tls(&host, port, CONNECT_TIMEOUT).await?,
        )),
        _ => Err(MqttError::Url(url.into())),
    }
}

/// Connect, subscribe to `topics`, then hand each publish's topic and
/// payload to `on_message` until the link fails or `stop` resolves.
pub async fn run(
    url: &str,
    client_id: &str,
    user: &str,
    pass: &str,
    topics: &[String],
    stop: impl std::future::Future<Output = ()>,
    mut on_message: impl FnMut(&str, &[u8]),
) -> Result<(), MqttError> {
    // Everything up to the broker's CONNACK, within one timeout: a
    // stalled handshake or write ends the attempt and the caller retries.
    let connect = async {
        let stream = open(url).await?;
        let (mut r, mut w) = tokio::io::split(stream);
        w.write_all(&connect_packet(
            client_id,
            user,
            pass,
            KEEPALIVE.as_secs() as u16,
        ))
        .await?;
        w.flush().await?;
        let (first, body) = read_packet(&mut r).await?;
        if first >> 4 != 2 || body.len() < 2 {
            return Err(MqttError::Protocol("expected CONNACK"));
        }
        if body[1] != 0 {
            return Err(MqttError::Refused(body[1]));
        }
        Ok((r, w))
    };
    let (mut r, mut w) = tokio::time::timeout(CONNECT_TIMEOUT, connect)
        .await
        .map_err(|_| MqttError::Timeout)??;
    for (i, chunk) in topics.chunks(20).enumerate() {
        w.write_all(&subscribe_packet(i as u16 + 1, chunk)).await?;
    }
    w.flush().await?;
    tracing::debug!("tuya: mqtt connected, {} topics", topics.len());
    tokio::pin!(stop);
    // Packets are read whole by their own task, so a ping or a stop never
    // abandons one half read.
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Result<(u8, Vec<u8>), MqttError>>(16);
    let reader = tokio::spawn(async move {
        loop {
            let p = read_packet(&mut r).await;
            let failed = p.is_err();
            if tx.send(p).await.is_err() || failed {
                return;
            }
        }
    });
    struct Abort(tokio::task::JoinHandle<()>);
    impl Drop for Abort {
        fn drop(&mut self) {
            self.0.abort();
        }
    }
    let _reader = Abort(reader);
    let mut awaiting_pong = false;
    // paho's ping: once per keepalive (the broker allows one and a half).
    let mut ping = tokio::time::interval_at(tokio::time::Instant::now() + KEEPALIVE, KEEPALIVE);
    // After the host slept, one late ping, not a burst that would read as
    // an unanswered one.
    ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = &mut stop => {
                let _ = w.write_all(&[0xE0, 0]).await; // DISCONNECT
                let _ = w.flush().await;
                return Ok(());
            }
            _ = ping.tick() => {
                if awaiting_pong {
                    return Err(MqttError::Timeout);
                }
                w.write_all(&[0xC0, 0]).await?; // PINGREQ
                w.flush().await?;
                awaiting_pong = true;
            }
            p = rx.recv() => match p {
                None => return Err(MqttError::Protocol("reader ended")),
                Some(p) => {
                    let (first, body) = p?;
                    match first >> 4 {
                        3 => {
                            let (topic, id, payload) = parse_publish(first, &body)?;
                            if let Some(id) = id {
                                let [a, b] = id.to_be_bytes();
                                w.write_all(&[0x40, 2, a, b]).await?; // PUBACK
                                w.flush().await?;
                            }
                            on_message(&topic, &payload);
                        }
                        13 => awaiting_pong = false, // PINGRESP
                        9 => tracing::debug!("tuya: mqtt suback {:?}", body.get(2..)),
                        _ => {}
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packets() {
        let c = connect_packet("c", "u", "p", 60);
        assert_eq!(c[0], 0x10);
        assert_eq!(c[1] as usize, c.len() - 2);
        let s = subscribe_packet(1, &["a/b".into()]);
        assert_eq!(s, vec![0x82, 8, 0, 1, 0, 3, b'a', b'/', b'b', 0]);
        let (t, id, p) = parse_publish(0x30, &[0, 1, b'x', b'h', b'i']).unwrap();
        assert_eq!((t.as_str(), id, p.as_slice()), ("x", None, &b"hi"[..]));
    }

    /// A broker on 127.0.0.1: it answers CONNECT and SUBSCRIBE, publishes
    /// one message, and sees DISCONNECT when the client is stopped.
    #[tokio::test]
    async fn talks_to_a_broker() {
        use tokio::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let broker = tokio::spawn(async move {
            let (mut s, _) = listener.accept().await.unwrap();
            let (first, body) = read_packet(&mut s).await.unwrap();
            assert_eq!(first, 0x10);
            assert_eq!(&body[..6], &[0, 4, b'M', b'Q', b'T', b'T']);
            s.write_all(&[0x20, 2, 0, 0]).await.unwrap();
            let (first, body) = read_packet(&mut s).await.unwrap();
            assert_eq!(first, 0x82);
            assert_eq!(&body[..2], &[0, 1]);
            s.write_all(&[0x90, 3, 0, 1, 0]).await.unwrap();
            s.write_all(&packet(0x30, &[0, 1, b't', b'h', b'i']))
                .await
                .unwrap();
            let (first, _) = read_packet(&mut s).await.unwrap();
            first
        });
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let mut got = Vec::new();
        let mut tx = Some(tx);
        let stop = async {
            let _ = rx.await;
        };
        run(
            &format!("tcp://127.0.0.1:{port}"),
            "id",
            "user",
            "pass",
            &["t".into()],
            stop,
            |topic, payload| {
                got.push((topic.to_owned(), payload.to_vec()));
                if let Some(tx) = tx.take() {
                    let _ = tx.send(());
                }
            },
        )
        .await
        .unwrap();
        assert_eq!(got, [("t".to_owned(), b"hi".to_vec())]);
        assert_eq!(broker.await.unwrap(), 0xE0, "DISCONNECT on stop");
    }

    #[test]
    fn long_length() {
        let p = packet(0x30, &vec![0u8; 300]);
        assert_eq!(&p[..3], &[0x30, 0xAC, 0x02]);
    }
}
