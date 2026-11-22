//! Experimental adapter proving that Quinn can use an ICE-nominated UDP path.
//!
//! This is a transport feasibility spike. It is not a production ICE policy or
//! a media transport. Quinn and ICE share one selected candidate pair; packets
//! are bounded and preserve datagram boundaries.

use std::{
    future::Future,
    io::{self, IoSliceMut},
    net::SocketAddr,
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll},
};

use futures_util::{Sink, SinkExt, Stream, StreamExt};
use quinn::{AsyncUdpSocket, UdpPoller};
use skyber_signaling::RelayClient;
use tokio::sync::{mpsc, watch};
use tokio_tungstenite::{tungstenite::Error as WebSocketError, tungstenite::Message};
use webrtc_util::conn::Conn;

const PACKET_CAPACITY: usize = 64;
const MAX_PACKET_SIZE: usize = 1200;

/// A Quinn UDP socket adapter around one ICE-selected candidate pair.
///
/// The ICE `Conn` owns the candidate sockets and selected remote pair. Quinn
/// therefore does not create a second UDP socket or a second NAT mapping.
pub struct IceQuinnSocket {
    local_addr: SocketAddr,
    remote_addr: SocketAddr,
    outgoing: Arc<mpsc::Sender<Vec<u8>>>,
    incoming: Mutex<mpsc::Receiver<Vec<u8>>>,
    shutdown: watch::Sender<bool>,
}

impl std::fmt::Debug for IceQuinnSocket {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("IceQuinnSocket")
            .field("local_addr", &self.local_addr)
            .field("remote_addr", &self.remote_addr)
            .finish_non_exhaustive()
    }
}

impl IceQuinnSocket {
    /// Bridge a connected ICE transport into Quinn's synchronous UDP socket API.
    pub fn new<I>(ice: Arc<I>) -> io::Result<Arc<Self>>
    where
        I: Conn + Send + Sync + 'static + ?Sized,
    {
        let local_addr = ice
            .local_addr()
            .map_err(|error| io::Error::other(error.to_string()))?;
        let remote_addr = ice.remote_addr().ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotConnected, "ICE pair is not selected")
        })?;

        let (outgoing_tx, mut outgoing_rx) = mpsc::channel::<Vec<u8>>(PACKET_CAPACITY);
        let (incoming_tx, incoming_rx) = mpsc::channel::<Vec<u8>>(PACKET_CAPACITY);
        let (shutdown, _) = watch::channel(false);

        let send_ice = Arc::clone(&ice);
        let mut send_shutdown = shutdown.subscribe();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = send_shutdown.changed() => break,
                    packet = outgoing_rx.recv() => {
                        let Some(packet) = packet else { break };
                        match send_ice.send(&packet).await {
                            Ok(written) if written == packet.len() => {}
                            _ => break,
                        }
                    }
                }
            }
        });

        let mut receive_shutdown = shutdown.subscribe();
        tokio::spawn(async move {
            let mut packet = vec![0_u8; MAX_PACKET_SIZE + 1];
            loop {
                tokio::select! {
                    _ = receive_shutdown.changed() => break,
                    result = ice.recv_from(&mut packet) => match result {
                        Ok((size, source)) if source == remote_addr && size <= MAX_PACKET_SIZE => {
                            tokio::select! {
                                _ = receive_shutdown.changed() => break,
                                sent = incoming_tx.send(packet[..size].to_vec()) => if sent.is_err() { break },
                            }
                        }
                        Ok(_) => {}
                        Err(_) => break,
                    }
                }
            }
        });

        Ok(Arc::new(Self {
            local_addr,
            remote_addr,
            outgoing: Arc::new(outgoing_tx),
            incoming: Mutex::new(incoming_rx),
            shutdown,
        }))
    }

    /// Stop background ICE bridge tasks and close their bounded queues.
    pub fn close(&self) {
        self.shutdown.send_replace(true);
    }
}

impl Drop for IceQuinnSocket {
    fn drop(&mut self) {
        self.close();
    }
}

impl AsyncUdpSocket for IceQuinnSocket {
    fn create_io_poller(self: Arc<Self>) -> Pin<Box<dyn UdpPoller>> {
        Box::pin(IceWritePoller {
            sender: Arc::clone(&self.outgoing),
            reserve: Mutex::new(None),
        })
    }

    fn try_send(&self, transmit: &quinn::udp::Transmit) -> io::Result<()> {
        if transmit.destination != self.remote_addr {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Quinn attempted to send outside the nominated ICE pair",
            ));
        }
        if transmit.segment_size.is_some() || transmit.contents.len() > MAX_PACKET_SIZE {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "ICE/Quinn proof only accepts single datagrams up to 1200 bytes",
            ));
        }

        match self.outgoing.try_send(transmit.contents.to_vec()) {
            Ok(()) => Ok(()),
            Err(mpsc::error::TrySendError::Full(_)) => Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "ICE send queue is full",
            )),
            Err(mpsc::error::TrySendError::Closed(_)) => Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "ICE send task is closed",
            )),
        }
    }

    fn poll_recv(
        &self,
        cx: &mut Context<'_>,
        bufs: &mut [IoSliceMut<'_>],
        meta: &mut [quinn::udp::RecvMeta],
    ) -> Poll<io::Result<usize>> {
        let (Some(buffer), Some(metadata)) = (bufs.first_mut(), meta.first_mut()) else {
            return Poll::Ready(Ok(0));
        };
        let mut incoming = match self.incoming.lock() {
            Ok(incoming) => incoming,
            Err(_) => return Poll::Ready(Err(io::Error::other("ICE receive queue lock poisoned"))),
        };
        match incoming.poll_recv(cx) {
            Poll::Ready(Some(packet)) => {
                if packet.len() > buffer.len() {
                    return Poll::Ready(Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "ICE packet exceeds Quinn receive buffer",
                    )));
                }
                buffer[..packet.len()].copy_from_slice(&packet);
                *metadata = quinn::udp::RecvMeta {
                    addr: self.remote_addr,
                    len: packet.len(),
                    stride: packet.len(),
                    ecn: None,
                    dst_ip: Some(self.local_addr.ip()),
                };
                Poll::Ready(Ok(1))
            }
            Poll::Ready(None) => Poll::Ready(Err(io::Error::new(
                io::ErrorKind::ConnectionAborted,
                "ICE receive task is closed",
            ))),
            Poll::Pending => Poll::Pending,
        }
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        Ok(self.local_addr)
    }
}

type WriteReserveFuture = Pin<
    Box<dyn Future<Output = Result<mpsc::OwnedPermit<Vec<u8>>, mpsc::error::SendError<()>>> + Send>,
>;

struct IceWritePoller {
    sender: Arc<mpsc::Sender<Vec<u8>>>,
    reserve: Mutex<Option<WriteReserveFuture>>,
}

impl std::fmt::Debug for IceWritePoller {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("IceWritePoller")
            .finish_non_exhaustive()
    }
}

impl UdpPoller for IceWritePoller {
    fn poll_writable(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.as_mut().get_mut();
        let mut reserve_slot = match this.reserve.lock() {
            Ok(slot) => slot,
            Err(_) => return Poll::Ready(Err(io::Error::other("ICE write poller lock poisoned"))),
        };
        let reserve = reserve_slot.get_or_insert_with(|| {
            let sender = Arc::clone(&this.sender);
            Box::pin(async move {
                sender
                    .as_ref()
                    .clone()
                    .reserve_owned()
                    .await
                    .map_err(|_| mpsc::error::SendError(()))
            })
        });
        match reserve.as_mut().poll(cx) {
            Poll::Ready(Ok(permit)) => {
                drop(permit);
                *reserve_slot = None;
                Poll::Ready(Ok(()))
            }
            Poll::Ready(Err(_)) => Poll::Ready(Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "ICE send task is closed",
            ))),
            Poll::Pending => Poll::Pending,
        }
    }
}

/// Quinn's bounded UDP-socket adapter for an authenticated WSS relay tunnel.
/// Each binary WebSocket message remains one QUIC datagram.
pub struct RelayQuinnSocket {
    local_addr: SocketAddr,
    remote_addr: SocketAddr,
    outgoing: Arc<mpsc::Sender<Vec<u8>>>,
    incoming: Mutex<mpsc::Receiver<Vec<u8>>>,
    shutdown: watch::Sender<bool>,
}

impl std::fmt::Debug for RelayQuinnSocket {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RelayQuinnSocket")
            .field("local_addr", &self.local_addr)
            .field("remote_addr", &self.remote_addr)
            .finish_non_exhaustive()
    }
}

impl RelayQuinnSocket {
    /// Wrap a relay that has already passed `RelayClient::join` authentication.
    pub fn from_authenticated_relay<S>(
        relay: RelayClient<S>,
        local_addr: SocketAddr,
        remote_addr: SocketAddr,
    ) -> Arc<Self>
    where
        S: Sink<Message, Error = WebSocketError>
            + Stream<Item = Result<Message, WebSocketError>>
            + Unpin
            + Send
            + 'static,
    {
        Self::new(relay.into_websocket(), local_addr, remote_addr)
    }

    /// Wrap a WebSocket stream. Callers should prefer `from_authenticated_relay`
    /// so arbitrary unjoined WebSockets cannot be mistaken for the relay path.
    pub fn new<S>(websocket: S, local_addr: SocketAddr, remote_addr: SocketAddr) -> Arc<Self>
    where
        S: Sink<Message, Error = WebSocketError>
            + Stream<Item = Result<Message, WebSocketError>>
            + Unpin
            + Send
            + 'static,
    {
        let (mut sink, mut stream) = websocket.split();
        let (outgoing_tx, mut outgoing_rx) = mpsc::channel::<Vec<u8>>(PACKET_CAPACITY);
        let (incoming_tx, incoming_rx) = mpsc::channel::<Vec<u8>>(PACKET_CAPACITY);
        let (shutdown, _) = watch::channel(false);

        let writer_shutdown = shutdown.clone();
        let mut writer_cancel = shutdown.subscribe();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = writer_cancel.changed() => break,
                    packet = outgoing_rx.recv() => match packet {
                        Some(packet) if !packet.is_empty() && packet.len() <= MAX_PACKET_SIZE => {
                            if sink.send(Message::Binary(packet.into())).await.is_err() { break; }
                        }
                        _ => break,
                    }
                }
            }
            let _ = sink.close().await;
            writer_shutdown.send_replace(true);
        });

        let reader_shutdown = shutdown.clone();
        let mut reader_cancel = shutdown.subscribe();
        tokio::spawn(async move {
            loop {
                let message = tokio::select! {
                    _ = reader_cancel.changed() => break,
                    message = stream.next() => message,
                };
                match message {
                    Some(Ok(Message::Binary(packet)))
                        if !packet.is_empty() && packet.len() <= MAX_PACKET_SIZE =>
                    {
                        tokio::select! {
                            _ = reader_cancel.changed() => break,
                            sent = incoming_tx.send(packet.to_vec()) => if sent.is_err() { break; },
                        }
                    }
                    _ => break,
                }
            }
            reader_shutdown.send_replace(true);
        });

        Arc::new(Self {
            local_addr,
            remote_addr,
            outgoing: Arc::new(outgoing_tx),
            incoming: Mutex::new(incoming_rx),
            shutdown,
        })
    }

    /// Stop the relay bridge tasks. Use this after closing its Quinn endpoint.
    pub fn close(&self) {
        self.shutdown.send_replace(true);
    }

    pub const fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    pub const fn remote_addr(&self) -> SocketAddr {
        self.remote_addr
    }
}

impl Drop for RelayQuinnSocket {
    fn drop(&mut self) {
        self.close();
    }
}

impl AsyncUdpSocket for RelayQuinnSocket {
    fn create_io_poller(self: Arc<Self>) -> Pin<Box<dyn UdpPoller>> {
        Box::pin(IceWritePoller {
            sender: Arc::clone(&self.outgoing),
            reserve: Mutex::new(None),
        })
    }

    fn try_send(&self, transmit: &quinn::udp::Transmit) -> io::Result<()> {
        if transmit.destination != self.remote_addr
            || transmit.segment_size.is_some()
            || transmit.contents.is_empty()
            || transmit.contents.len() > MAX_PACKET_SIZE
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid QUIC datagram for WebSocket relay",
            ));
        }
        self.outgoing
            .try_send(transmit.contents.to_vec())
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => {
                    io::Error::new(io::ErrorKind::WouldBlock, "WebSocket send queue is full")
                }
                mpsc::error::TrySendError::Closed(_) => {
                    io::Error::new(io::ErrorKind::BrokenPipe, "WebSocket relay is closed")
                }
            })
    }

    fn poll_recv(
        &self,
        cx: &mut Context<'_>,
        bufs: &mut [IoSliceMut<'_>],
        meta: &mut [quinn::udp::RecvMeta],
    ) -> Poll<io::Result<usize>> {
        let (Some(buffer), Some(metadata)) = (bufs.first_mut(), meta.first_mut()) else {
            return Poll::Ready(Ok(0));
        };
        let mut incoming = match self.incoming.lock() {
            Ok(incoming) => incoming,
            Err(_) => {
                return Poll::Ready(Err(io::Error::other("WebSocket receive queue poisoned")));
            }
        };
        match incoming.poll_recv(cx) {
            Poll::Ready(Some(packet)) if packet.len() <= buffer.len() => {
                buffer[..packet.len()].copy_from_slice(&packet);
                *metadata = quinn::udp::RecvMeta {
                    addr: self.remote_addr,
                    len: packet.len(),
                    stride: packet.len(),
                    ecn: None,
                    dst_ip: Some(self.local_addr.ip()),
                };
                Poll::Ready(Ok(1))
            }
            Poll::Ready(Some(_)) => Poll::Ready(Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "WebSocket datagram exceeds Quinn receive buffer",
            ))),
            Poll::Ready(None) => Poll::Ready(Err(io::Error::new(
                io::ErrorKind::ConnectionAborted,
                "WebSocket relay closed",
            ))),
            Poll::Pending => Poll::Pending,
        }
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        Ok(self.local_addr)
    }
}

#[cfg(test)]
mod tests {
    use std::{net::SocketAddr, sync::Arc, time::Duration};

    use super::MAX_PACKET_SIZE;
    use crate::ice_session::{IceCandidateEvent, IceRole, IceSession, RemoteIceCredentials};
    use crate::room_auth::authenticate_room;
    use crate::transport::{
        AudioMode, Role, TrackProtocols, VideoMode, register_tracks_with_protocols,
    };
    use anyhow::{Context, Result, ensure};
    use ed25519_dalek::SigningKey;
    use futures_util::{SinkExt, StreamExt};
    use quinn::{AsyncUdpSocket, EndpointConfig, TokioRuntime};
    use skyber_signaling::{AuthenticatedPeers, PeerIdentity, RoomId};
    use tokio::{
        io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
        net::TcpListener,
        task::JoinHandle,
    };
    use tokio_tungstenite::{
        WebSocketStream, accept_async_with_config, connect_async_with_config, tungstenite::Message,
    };

    #[tokio::test]
    async fn nominated_ice_pair_carries_pinned_kyber_quic() -> Result<()> {
        let mut client_ice = IceSession::new(&[]).await?;
        let mut server_ice = IceSession::new(&[]).await?;
        client_ice.gather()?;
        server_ice.gather()?;
        let client_candidate = receive_candidate(&mut client_ice).await?;
        let server_candidate = receive_candidate(&mut server_ice).await?;
        client_ice.add_remote_candidate(&server_candidate)?;
        server_ice.add_remote_candidate(&client_candidate)?;
        let client_remote = RemoteIceCredentials::new(
            server_ice.local_credentials().ufrag().to_owned(),
            server_ice.local_credentials().password().to_owned(),
        )?;
        let server_remote = RemoteIceCredentials::new(
            client_ice.local_credentials().ufrag().to_owned(),
            client_ice.local_credentials().password().to_owned(),
        )?;
        let (client_ice, server_ice) = tokio::try_join!(
            client_ice.connect(IceRole::Controlling, client_remote),
            server_ice.connect(IceRole::Controlled, server_remote),
        )?;
        let client_socket = client_ice.quinn_socket();
        let server_socket = server_ice.quinn_socket();
        let server_addr = server_socket.local_addr()?;

        let certified = rcgen::generate_simple_self_signed(vec!["localhost".to_owned()])?;
        let certificate = certified.cert.der().clone();
        let private_key = rustls::pki_types::PrivateKeyDer::Pkcs8(
            rustls::pki_types::PrivatePkcs8KeyDer::from(certified.signing_key.serialize_der()),
        );
        let mut server_tls = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![certificate.clone()], private_key)?;
        server_tls.alpn_protocols = vec![b"kymux".to_vec()];
        let server_config = quinn::ServerConfig::with_crypto(Arc::new(
            quinn::crypto::rustls::QuicServerConfig::try_from(server_tls)?,
        ));

        let mut roots = rustls::RootCertStore::empty();
        roots.add(certificate)?;
        let mut tls = rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        tls.alpn_protocols = vec![b"kymux".to_vec()];
        let client_config = quinn::ClientConfig::new(Arc::new(
            quinn::crypto::rustls::QuicClientConfig::try_from(tls)?,
        ));

        let runtime = Arc::new(TokioRuntime);
        let server_endpoint = quinn::Endpoint::new_with_abstract_socket(
            EndpointConfig::default(),
            Some(server_config),
            server_socket,
            runtime.clone(),
        )?;
        let mut client_endpoint = quinn::Endpoint::new_with_abstract_socket(
            EndpointConfig::default(),
            None,
            client_socket,
            runtime,
        )?;
        client_endpoint.set_default_client_config(client_config);

        let connecting = client_endpoint.connect(server_addr, "localhost")?;
        let (client_quic, server_quic) = tokio::time::timeout(Duration::from_secs(10), async {
            let server_accept = async {
                server_endpoint
                    .accept()
                    .await
                    .context("QUIC endpoint closed before handshake")?
                    .await
                    .context("server QUIC handshake failed")
            };
            tokio::try_join!(
                async { connecting.await.context("client QUIC handshake failed") },
                server_accept
            )
        })
        .await
        .context("ICE-backed QUIC handshake timed out")??;

        let inviter_key = SigningKey::from_bytes(&[11; 32]);
        let invitee_key = SigningKey::from_bytes(&[22; 32]);
        let peers = AuthenticatedPeers {
            room: RoomId::from_bytes([7; 16]),
            inviter: PeerIdentity::from_signing_key(&inviter_key),
            invitee: PeerIdentity::from_signing_key(&invitee_key),
        };
        let (client_quic, server_quic) = tokio::try_join!(
            authenticate_room(client_quic, peers, &inviter_key),
            authenticate_room(server_quic, peers, &invitee_key),
        )?;
        let client = kymux::kynet::Connection::from(client_quic.into_inner());
        let server = kymux::kynet::Connection::from(server_quic.into_inner());
        let (mut client_send, mut client_receive) = client.open_bi().await?;
        client_send.write_all(b"ice-quinn-kymux").await?;
        client_send.finish().await?;
        let (mut server_send, mut server_receive) =
            tokio::time::timeout(Duration::from_secs(5), server.accept_bi())
                .await
                .context("Kyber stream acceptance timed out")??;
        let mut response = [0_u8; 15];
        tokio::time::timeout(
            Duration::from_secs(5),
            server_receive.read_exact(&mut response),
        )
        .await
        .context("Kyber payload receive timed out")??;
        ensure!(
            &response == b"ice-quinn-kymux",
            "unexpected Kyber payload over nominated ICE path"
        );
        server_send.write_all(b"ack").await?;
        server_send.finish().await?;
        let mut acknowledgement = [0_u8; 3];
        tokio::time::timeout(
            Duration::from_secs(5),
            client_receive.read_exact(&mut acknowledgement),
        )
        .await
        .context("Kyber response receive timed out")??;
        ensure!(
            &acknowledgement == b"ack",
            "Kyber response was not received"
        );

        let (client_media, server_media) = tokio::try_join!(
            kymux::kyproto::Connection::connect(client.clone()),
            kymux::kyproto::Connection::accept(server.clone()),
        )?;
        let client_protocols = TrackProtocols {
            microphone: AudioMode::UnreliableFec,
            camera: VideoMode::GopStream,
            screen: VideoMode::Reliable,
        };
        let server_protocols = TrackProtocols {
            microphone: AudioMode::Unreliable,
            camera: VideoMode::UnreliableFec,
            screen: VideoMode::GopStream,
        };
        let (client_tracks, server_tracks) = tokio::try_join!(
            register_tracks_with_protocols(
                &client_media,
                Role::Initiator,
                &[skyber_core::Track::Microphone],
                &[skyber_core::Track::Camera],
                client_protocols,
            ),
            register_tracks_with_protocols(
                &server_media,
                Role::Listener,
                &[skyber_core::Track::Camera],
                &[skyber_core::Track::Microphone],
                server_protocols,
            ),
        )?;
        ensure!(
            client_tracks.effective_protocols().microphone == Some(AudioMode::UnreliableFec)
                && server_tracks.effective_protocols().camera == Some(VideoMode::UnreliableFec),
            "negotiated media protocols did not match the peer selections"
        );
        let mut client_tracks = client_tracks;
        let mut server_tracks = server_tracks;
        tokio::try_join!(
            client_tracks.finish(Role::Initiator),
            server_tracks.finish(Role::Listener),
        )?;

        client.close(0, "transport proof complete");
        server.close(0, "transport proof complete");
        client_endpoint.wait_idle().await;
        server_endpoint.wait_idle().await;
        Ok(())
    }

    #[tokio::test]
    async fn websocket_relay_carries_pinned_kyber_quic() -> Result<()> {
        websocket_roundtrip(true).await?;
        // Start a fresh tunnel and QUIC connection after the first session closes.
        websocket_roundtrip(false).await
    }

    async fn websocket_roundtrip(drop_first_packet_each_way: bool) -> Result<()> {
        let (client_socket, server_socket, relay) =
            websocket_pair(drop_first_packet_each_way).await?;
        let server_addr = server_socket.local_addr();

        let certified = rcgen::generate_simple_self_signed(vec!["localhost".to_owned()])?;
        let certificate = certified.cert.der().clone();
        let private_key = rustls::pki_types::PrivateKeyDer::Pkcs8(
            rustls::pki_types::PrivatePkcs8KeyDer::from(certified.signing_key.serialize_der()),
        );
        let mut server_tls = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![certificate.clone()], private_key)?;
        server_tls.alpn_protocols = vec![b"kymux".to_vec()];
        let server_config = quinn::ServerConfig::with_crypto(Arc::new(
            quinn::crypto::rustls::QuicServerConfig::try_from(server_tls)?,
        ));
        let mut roots = rustls::RootCertStore::empty();
        roots.add(certificate)?;
        let mut client_tls = rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        client_tls.alpn_protocols = vec![b"kymux".to_vec()];
        let client_config = quinn::ClientConfig::new(Arc::new(
            quinn::crypto::rustls::QuicClientConfig::try_from(client_tls)?,
        ));
        let runtime = Arc::new(TokioRuntime);
        let server_endpoint = quinn::Endpoint::new_with_abstract_socket(
            EndpointConfig::default(),
            Some(server_config),
            server_socket,
            runtime.clone(),
        )?;
        let mut client_endpoint = quinn::Endpoint::new_with_abstract_socket(
            EndpointConfig::default(),
            None,
            client_socket,
            runtime,
        )?;
        client_endpoint.set_default_client_config(client_config);

        let connecting = client_endpoint.connect(server_addr, "localhost")?;
        let (client_quic, server_quic) = tokio::time::timeout(Duration::from_secs(10), async {
            let accepted = async {
                server_endpoint
                    .accept()
                    .await
                    .context("server QUIC endpoint closed")?
                    .await
                    .context("server QUIC handshake failed")
            };
            tokio::try_join!(
                async { connecting.await.context("client QUIC handshake failed") },
                accepted
            )
        })
        .await
        .context("WebSocket QUIC handshake timed out")??;

        let inviter_key = SigningKey::from_bytes(&[11; 32]);
        let invitee_key = SigningKey::from_bytes(&[22; 32]);
        let peers = AuthenticatedPeers {
            room: RoomId::from_bytes([7; 16]),
            inviter: PeerIdentity::from_signing_key(&inviter_key),
            invitee: PeerIdentity::from_signing_key(&invitee_key),
        };
        let (client_quic, server_quic) = tokio::try_join!(
            authenticate_room(client_quic, peers, &inviter_key),
            authenticate_room(server_quic, peers, &invitee_key),
        )?;
        let client = kymux::kynet::Connection::from(client_quic.into_inner());
        let server = kymux::kynet::Connection::from(server_quic.into_inner());
        let (mut client_send, mut client_receive) = client.open_bi().await?;
        const SYNTHETIC_SIZE: usize = 256 * 1024;
        let synthetic = vec![0x5A_u8; SYNTHETIC_SIZE];
        client_send.write_all(&synthetic).await?;
        client_send.finish().await?;
        let (mut server_send, mut server_receive) =
            tokio::time::timeout(Duration::from_secs(5), server.accept_bi())
                .await
                .context("Kyber stream over WebSocket timed out")??;
        let mut request = vec![0_u8; SYNTHETIC_SIZE];
        tokio::time::timeout(
            Duration::from_secs(5),
            server_receive.read_exact(&mut request),
        )
        .await
        .context("Kyber request over WebSocket timed out")??;
        ensure!(
            request.iter().all(|byte| *byte == 0x5A),
            "incorrect WebSocket Kyber payload"
        );
        server_send.write_all(b"ack").await?;
        server_send.finish().await?;
        let mut response = [0_u8; 3];
        tokio::time::timeout(
            Duration::from_secs(5),
            client_receive.read_exact(&mut response),
        )
        .await
        .context("Kyber response over WebSocket timed out")??;
        ensure!(&response == b"ack", "incorrect WebSocket Kyber response");
        client.close(0, "WebSocket transport proof complete");
        server.close(0, "WebSocket transport proof complete");
        client_endpoint.wait_idle().await;
        server_endpoint.wait_idle().await;
        relay.abort();
        Ok(())
    }

    async fn websocket_pair(
        drop_first_packet_each_way: bool,
    ) -> Result<(
        Arc<super::RelayQuinnSocket>,
        Arc<super::RelayQuinnSocket>,
        JoinHandle<()>,
    )> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let relay = tokio::spawn(async move {
            let Ok((first, _)) = listener.accept().await else {
                return;
            };
            let Ok(first) = accept_async_with_config(first, Some(websocket_config())).await else {
                return;
            };
            let Ok((second, _)) = listener.accept().await else {
                return;
            };
            let Ok(second) = accept_async_with_config(second, Some(websocket_config())).await
            else {
                return;
            };
            relay_websockets(first, second, drop_first_packet_each_way).await;
        });
        let (first, _) = connect_async_with_config(
            format!("ws://{address}/room/proof"),
            Some(websocket_config()),
            false,
        )
        .await?;
        let (second, _) = connect_async_with_config(
            format!("ws://{address}/room/proof"),
            Some(websocket_config()),
            false,
        )
        .await?;
        let client_addr = SocketAddr::from(([127, 0, 0, 1], 52001));
        let server_addr = SocketAddr::from(([127, 0, 0, 1], 52002));
        Ok((
            super::RelayQuinnSocket::new(first, client_addr, server_addr),
            super::RelayQuinnSocket::new(second, server_addr, client_addr),
            relay,
        ))
    }

    fn websocket_config() -> tokio_tungstenite::tungstenite::protocol::WebSocketConfig {
        let mut config = tokio_tungstenite::tungstenite::protocol::WebSocketConfig::default();
        config.max_message_size = Some(MAX_PACKET_SIZE);
        config.max_frame_size = Some(MAX_PACKET_SIZE);
        config
    }

    async fn relay_websockets<A, B>(
        first: WebSocketStream<A>,
        second: WebSocketStream<B>,
        drop_first: bool,
    ) where
        A: AsyncRead + AsyncWrite + Unpin,
        B: AsyncRead + AsyncWrite + Unpin,
    {
        let (mut first_sink, mut first_stream) = first.split();
        let (mut second_sink, mut second_stream) = second.split();
        let mut drop_first_from_client = drop_first;
        let mut drop_first_from_server = drop_first;
        loop {
            tokio::select! {
                message = first_stream.next() => match message {
                    Some(Ok(Message::Binary(packet))) if !packet.is_empty() && packet.len() <= MAX_PACKET_SIZE => {
                        if drop_first_from_client {
                            drop_first_from_client = false;
                        } else {
                            tokio::time::sleep(Duration::from_millis(1)).await;
                            if second_sink.send(Message::Binary(packet)).await.is_err() { break; }
                        }
                    }
                    _ => break,
                },
                message = second_stream.next() => match message {
                    Some(Ok(Message::Binary(packet))) if !packet.is_empty() && packet.len() <= MAX_PACKET_SIZE => {
                        if drop_first_from_server {
                            drop_first_from_server = false;
                        } else {
                            tokio::time::sleep(Duration::from_millis(1)).await;
                            if first_sink.send(Message::Binary(packet)).await.is_err() { break; }
                        }
                    }
                    _ => break,
                },
            }
        }
        let _ = first_sink.close().await;
        let _ = second_sink.close().await;
    }

    async fn receive_candidate(session: &mut IceSession) -> Result<String> {
        tokio::time::timeout(Duration::from_secs(5), async {
            match session.next_candidate().await? {
                IceCandidateEvent::Candidate(candidate) => Ok(candidate),
                IceCandidateEvent::GatheringComplete => {
                    anyhow::bail!("ICE candidate gathering completed without a candidate")
                }
            }
        })
        .await
        .context("ICE host candidate gathering timed out")?
    }
}
