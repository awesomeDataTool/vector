use crate::{
    internal_events::TcpConnectionError,
    shutdown::ShutdownSignal,
    tls::{MaybeTlsIncomingStream, MaybeTlsListener, MaybeTlsSettings},
    Event, Pipeline,
};
use bytes::Bytes;
use futures::{
    compat::{Future01CompatExt, Sink01CompatExt},
    future::{self, BoxFuture},
    stream, FutureExt, StreamExt, TryFutureExt,
};
use futures01::Sink;
use listenfd::ListenFd;
use serde::{de, Deserialize, Deserializer, Serialize};
use std::{fmt, io, net::SocketAddr, task::Poll, time::Duration};
use tokio::{
    net::{TcpListener, TcpStream},
    time::delay_for,
};
use tokio_util::codec::{Decoder, FramedRead};
use tracing::field;
use tracing_futures::Instrument;

async fn make_listener(
    addr: SocketListenAddr,
    mut listenfd: ListenFd,
    tls: &MaybeTlsSettings,
) -> Option<MaybeTlsListener> {
    match addr {
        SocketListenAddr::SocketAddr(addr) => match tls.bind(&addr).await {
            Ok(listener) => Some(listener),
            Err(err) => {
                error!("Failed to bind to listener socket: {}", err);
                None
            }
        },
        SocketListenAddr::SystemdFd(offset) => match listenfd.take_tcp_listener(offset) {
            Ok(Some(listener)) => match TcpListener::from_std(listener) {
                Ok(listener) => Some(listener.into()),
                Err(err) => {
                    error!("Failed to bind to listener socket: {}", err);
                    None
                }
            },
            Ok(None) => {
                error!("Failed to take listen FD, not open or already taken");
                None
            }
            Err(err) => {
                error!("Failed to take listen FD: {}", err);
                None
            }
        },
    }
}

pub trait TcpSource: Clone + Send + Sync + 'static {
    // Should be default: `std::io::Error`.
    // Right now this is unstable: https://github.com/rust-lang/rust/issues/29661
    type Error: From<io::Error> + std::fmt::Debug + std::fmt::Display;
    type Decoder: Decoder<Error = Self::Error> + Send + 'static;

    fn decoder(&self) -> Self::Decoder;

    fn build_event(&self, frame: <Self::Decoder as Decoder>::Item, host: Bytes) -> Option<Event>;

    fn run(
        self,
        addr: SocketListenAddr,
        shutdown_timeout_secs: u64,
        tls: MaybeTlsSettings,
        shutdown: ShutdownSignal,
        out: Pipeline,
    ) -> crate::Result<crate::sources::Source> {
        let out = out.sink_map_err(|e| error!("Error sending event: {:?}", e));

        let listenfd = ListenFd::from_env();

        let fut = async move {
            let listener = match make_listener(addr, listenfd, &tls).await {
                None => return Err(()),
                Some(listener) => listener,
            };

            info!(
                message = "Listening.",
                addr = field::display(
                    listener
                        .local_addr()
                        .map(SocketListenAddr::SocketAddr)
                        .unwrap_or(addr)
                )
            );

            let tripwire = shutdown.clone().compat();
            let tripwire = async move {
                let _ = tripwire.await;
                delay_for(Duration::from_secs(shutdown_timeout_secs)).await;
            }
            .shared();

            listener
                .accept_stream()
                .take_until(shutdown.clone().compat())
                .for_each(|connection| {
                    let shutdown = shutdown.clone();
                    let tripwire = tripwire.clone();
                    let source = self.clone();
                    let out = out.clone();

                    async move {
                        let socket = match connection {
                            Ok(socket) => socket,
                            Err(error) => {
                                error!(
                                    message = "failed to accept socket",
                                    %error
                                );
                                return;
                            }
                        };

                        let peer_addr = socket.peer_addr().ip().to_string();
                        let span = info_span!("connection", %peer_addr);
                        let host = Bytes::from(peer_addr);

                        let tripwire = tripwire
                            .map(move |_| {
                                info!(
                                    "Resetting connection (still open after {} seconds).",
                                    shutdown_timeout_secs
                                );
                            })
                            .boxed();

                        span.in_scope(|| {
                            let peer_addr = socket.peer_addr();
                            debug!(message = "accepted a new connection", %peer_addr);

                            let fut = handle_stream(shutdown, socket, source, tripwire, host, out);
                            tokio::spawn(fut.instrument(span.clone()));
                        });
                    }
                })
                .map(Ok)
                .await
        };

        Ok(Box::new(fut.boxed().compat()))
    }
}

async fn handle_stream(
    shutdown: ShutdownSignal,
    mut socket: MaybeTlsIncomingStream<TcpStream>,
    source: impl TcpSource,
    tripwire: BoxFuture<'static, ()>,
    host: Bytes,
    out: impl Sink<SinkItem = Event, SinkError = ()> + Send + 'static,
) {
    let mut shutdown = shutdown.compat();
    tokio::select! {
        result = socket.handshake() => {
            if let Err(error) = result {
                emit!(TcpConnectionError { error });
                return;
            }
        },
        _ = &mut shutdown => {
            return;
        }
    };

    let mut _token = None;
    let mut shutdown = Some(shutdown);
    let mut reader = FramedRead::new(socket, source.decoder());
    stream::poll_fn(move |cx| {
        if let Some(fut) = shutdown.as_mut() {
            match fut.poll_unpin(cx) {
                Poll::Ready(Ok(token)) => {
                    debug!("Start graceful shutdown");
                    // Close our write part of TCP socket to signal the other side
                    // that it should stop writing and close the channel.
                    let socket: Option<&TcpStream> = reader.get_ref().get_ref();
                    if let Some(socket) = socket {
                        if let Err(error) = socket.shutdown(std::net::Shutdown::Write) {
                            warn!(message = "Failed in signalling to the other side to close the TCP channel.", %error);
                        }
                    } else {
                        // Connection hasn't yet been established so we are done here.
                        debug!("Closing connection that hasn't yet been fully established.");
                        return Poll::Ready(None);
                    }

                    _token = Some(token);
                    shutdown = None;
                }
                Poll::Ready(Err(())) => {
                    shutdown = None;
                }
                Poll::Pending => {}
            }
        }

        reader.poll_next_unpin(cx)
    })
    .take_until(tripwire)
    .filter_map(move |frame| future::ready(match frame {
        Ok(frame) => {
            let host = host.clone();
            source.build_event(frame, host).map(Ok)
        }
        Err(error) => {
            warn!(message = "Failed to read data from TCP source.", %error);
            None
        }
    }))
    .forward(out.sink_compat())
    .map_err(|_| warn!(message = "Error received while processing TCP source."))
    .map(|_| debug!("connection closed."))
    .await
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
#[serde(untagged)]
pub enum SocketListenAddr {
    SocketAddr(SocketAddr),
    #[serde(deserialize_with = "parse_systemd_fd")]
    SystemdFd(usize),
}

impl fmt::Display for SocketListenAddr {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Self::SocketAddr(ref addr) => addr.fmt(f),
            Self::SystemdFd(offset) => write!(f, "systemd socket #{}", offset),
        }
    }
}

impl From<SocketAddr> for SocketListenAddr {
    fn from(addr: SocketAddr) -> Self {
        Self::SocketAddr(addr)
    }
}

fn parse_systemd_fd<'de, D>(des: D) -> Result<usize, D::Error>
where
    D: Deserializer<'de>,
{
    let s: &'de str = Deserialize::deserialize(des)?;
    match s {
        "systemd" => Ok(0),
        s if s.starts_with("systemd#") => {
            Ok(s[8..].parse::<usize>().map_err(de::Error::custom)? - 1)
        }
        _ => Err(de::Error::custom("must start with \"systemd\"")),
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use serde::Deserialize;
    use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};

    #[derive(Debug, Deserialize)]
    struct Config {
        addr: SocketListenAddr,
    }

    #[test]
    fn parse_socket_listen_addr() {
        let test: Config = toml::from_str(r#"addr="127.1.2.3:1234""#).unwrap();
        assert_eq!(
            test.addr,
            SocketListenAddr::SocketAddr(SocketAddr::V4(SocketAddrV4::new(
                Ipv4Addr::new(127, 1, 2, 3),
                1234,
            )))
        );
        let test: Config = toml::from_str(r#"addr="systemd""#).unwrap();
        assert_eq!(test.addr, SocketListenAddr::SystemdFd(0));
        let test: Config = toml::from_str(r#"addr="systemd#3""#).unwrap();
        assert_eq!(test.addr, SocketListenAddr::SystemdFd(2));
    }
}
