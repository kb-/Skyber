//! Quinn endpoint construction for an acquired direct or relay socket.
//!
//! This connector owns Quinn's client/server role and handshake. Acquiring
//! signaling, ICE and authenticated relay resources remains the responsibility
//! of `CallPathSocketProvider`, so the desktop can supply its validated TLS
//! configuration and WebSocket trust policy without hidden defaults.

use std::{future::Future, net::SocketAddr, pin::Pin, sync::Arc, time::Duration};

use crate::{
    call_controller::CallPathConnector, ice_quinn::RelayQuinnSocket,
    ice_session::NominatedIceConnection, path_policy::NetworkPath,
};
use futures_util::{Sink, Stream};
use quinn::{AsyncUdpSocket, ClientConfig, Connection, Endpoint, EndpointConfig, ServerConfig};
use skyber_signaling::{PeerCredentials, PeerIdentity, RelayClient, RoomId, SignalClient};
use tokio::sync::watch;
use tokio_tungstenite::{tungstenite::Error as WebSocketError, tungstenite::Message};

/// Opens a separately authenticated WSS relay connection. Implementations
/// own the platform CA or explicit service-pin policy for that connection.
pub trait RelayWebSocketConnector: Send {
    type WebSocket: Sink<Message, Error = WebSocketError>
        + Stream<Item = Result<Message, WebSocketError>>
        + Unpin
        + Send
        + 'static;
    type ConnectFuture<'a>: Future<Output = Result<Self::WebSocket, String>> + Send + 'a
    where
        Self: 'a;

    fn connect(&mut self) -> Self::ConnectFuture<'_>;
}

/// Concrete path-resource provider joining typed signaling, IPv4 ICE and the
/// authenticated WSS relay. It reuses the call's signaling socket for direct
/// ICE and opens a separately TLS-validated WebSocket for relay fallback.
pub struct SignalingPathSocketProvider<S, W> {
    signaling: SignalClient<S>,
    relay_websocket: W,
    room: RoomId,
    remote_peer: PeerIdentity,
    credentials: PeerCredentials,
    signing_key: ed25519_dalek::SigningKey,
    is_inviter: bool,
    stun_server_urls: Vec<String>,
    cancellation: watch::Receiver<bool>,
}

impl<S, W> SignalingPathSocketProvider<S, W> {
    pub fn new(
        signaling: SignalClient<S>,
        relay_websocket: W,
        accepted_room: skyber_signaling::AcceptedRoom,
        signing_key: ed25519_dalek::SigningKey,
        is_inviter: bool,
        stun_server_urls: Vec<String>,
        cancellation: watch::Receiver<bool>,
    ) -> Self {
        Self {
            signaling,
            relay_websocket,
            room: accepted_room.room,
            remote_peer: accepted_room.remote_peer,
            credentials: accepted_room.credentials,
            signing_key,
            is_inviter,
            stun_server_urls,
            cancellation,
        }
    }
}

impl<S, W> CallPathSocketProvider for SignalingPathSocketProvider<S, W>
where
    S: Sink<Message, Error = WebSocketError>
        + Stream<Item = Result<Message, WebSocketError>>
        + Unpin
        + Send,
    W: RelayWebSocketConnector,
{
    type SocketFuture<'a>
        = Pin<Box<dyn Future<Output = Result<QuicPathSocket, String>> + Send + 'a>>
    where
        S: 'a,
        W: 'a;

    fn acquire(&mut self, path: NetworkPath) -> Self::SocketFuture<'_> {
        Box::pin(async move {
            if *self.cancellation.borrow() {
                return Err("call path setup cancelled".to_owned());
            }
            match path {
                NetworkPath::DirectUdp => {
                    let mut ice_cancellation = self.cancellation.clone();
                    let mut cancel_wait = self.cancellation.clone();
                    let role = if self.is_inviter {
                        crate::ice_session::IceRole::Controlling
                    } else {
                        crate::ice_session::IceRole::Controlled
                    };
                    let connection = tokio::select! {
                        result = crate::ice_session::negotiate_direct_ice(
                            &mut self.signaling,
                            self.room,
                            self.remote_peer,
                            role,
                            &self.stun_server_urls,
                            &mut ice_cancellation,
                        ) => result.map_err(|error| error.to_string())?,
                        _ = wait_for_cancel(&mut cancel_wait) => {
                            return Err("direct ICE setup cancelled".to_owned());
                        }
                    };
                    Ok(QuicPathSocket::Direct(connection))
                }
                NetworkPath::WebSocketRelay => {
                    let mut cancellation = self.cancellation.clone();
                    let websocket = tokio::select! {
                        result = self.relay_websocket.connect() => result?,
                        _ = wait_for_cancel(&mut cancellation) => {
                            return Err("relay connection cancelled".to_owned());
                        }
                    };
                    let relay = RelayClient::join(
                        websocket,
                        self.room,
                        self.credentials.relay_token(),
                        &self.signing_key,
                    )
                    .await
                    .map_err(|error| error.to_string())?;
                    let (local_addr, remote_addr) = relay_virtual_addresses(self.is_inviter);
                    Ok(QuicPathSocket::Relay(
                        RelayQuinnSocket::from_authenticated_relay(relay, local_addr, remote_addr),
                    ))
                }
            }
        })
    }
}

async fn wait_for_cancel(cancellation: &mut watch::Receiver<bool>) {
    while !*cancellation.borrow() {
        if cancellation.changed().await.is_err() {
            return;
        }
    }
}

fn relay_virtual_addresses(is_inviter: bool) -> (SocketAddr, SocketAddr) {
    let inviter = SocketAddr::from(([127, 0, 0, 1], 41_000));
    let invitee = SocketAddr::from(([127, 0, 0, 1], 41_001));
    if is_inviter {
        (inviter, invitee)
    } else {
        (invitee, inviter)
    }
}

/// Authenticated transport socket prepared for one path attempt.
///
/// The direct variant retains the nominated ICE agent and connection for the
/// lifetime of the Quinn endpoint. The relay variant retains its bounded WSS
/// datagram adapter.
pub enum QuicPathSocket {
    Direct(NominatedIceConnection),
    Relay(Arc<RelayQuinnSocket>),
}

impl QuicPathSocket {
    fn socket(&self) -> Arc<dyn AsyncUdpSocket> {
        match self {
            Self::Direct(connection) => connection.quinn_socket(),
            Self::Relay(socket) => Arc::clone(socket) as Arc<dyn AsyncUdpSocket>,
        }
    }

    fn local_addr(&self) -> Result<SocketAddr, String> {
        match self {
            Self::Direct(connection) => connection.local_addr().map_err(|error| error.to_string()),
            Self::Relay(socket) => Ok(socket.local_addr()),
        }
    }

    fn remote_addr(&self) -> Option<SocketAddr> {
        match self {
            Self::Direct(connection) => connection.remote_addr(),
            Self::Relay(socket) => Some(socket.remote_addr()),
        }
    }
}

/// Supplies path-specific sockets. Direct implementations normally call
/// `negotiate_direct_ice`; relay implementations join the authenticated relay
/// with the room-scoped credential before constructing `RelayQuinnSocket`.
pub trait CallPathSocketProvider: Send {
    type SocketFuture<'a>: Future<Output = Result<QuicPathSocket, String>> + Send + 'a
    where
        Self: 'a;

    fn acquire(&mut self, path: NetworkPath) -> Self::SocketFuture<'_>;
}

/// Constructs the Quinn client or server endpoint and performs its handshake.
/// The inviter is always the QUIC client, matching the room-auth protocol.
pub struct QuinnCallPathConnector<P> {
    provider: P,
    is_inviter: bool,
    client_config: ClientConfig,
    server_config: ServerConfig,
    server_name: String,
    endpoint_config: EndpointConfig,
    active_endpoint: Option<Endpoint>,
    active_socket: Option<QuicPathSocket>,
}

impl<P> QuinnCallPathConnector<P> {
    pub fn new(
        provider: P,
        is_inviter: bool,
        client_config: ClientConfig,
        server_config: ServerConfig,
        server_name: String,
    ) -> Self {
        Self {
            provider,
            is_inviter,
            client_config,
            server_config,
            server_name,
            endpoint_config: EndpointConfig::default(),
            active_endpoint: None,
            active_socket: None,
        }
    }

    pub fn with_endpoint_config(mut self, endpoint_config: EndpointConfig) -> Self {
        self.endpoint_config = endpoint_config;
        self
    }

    async fn connect_on_path(&mut self, path: NetworkPath) -> Result<Connection, String>
    where
        P: CallPathSocketProvider,
    {
        if self.active_endpoint.is_some() || self.active_socket.is_some() {
            self.close();
        }
        let path_socket = self.provider.acquire(path).await?;
        let remote_addr = path_socket
            .remote_addr()
            .ok_or_else(|| "path has no nominated remote address".to_owned())?;
        let local_addr = path_socket.local_addr()?;
        let socket = path_socket.socket();
        if socket.local_addr().map_err(|error| error.to_string())? != local_addr {
            return Err("path socket local address changed during setup".to_owned());
        }

        let server_config = (!self.is_inviter).then(|| self.server_config.clone());
        let mut endpoint = Endpoint::new_with_abstract_socket(
            self.endpoint_config.clone(),
            server_config,
            socket,
            Arc::new(quinn::TokioRuntime),
        )
        .map_err(|error| format!("could not create Quinn endpoint: {error}"))?;

        let connection = if self.is_inviter {
            endpoint.set_default_client_config(self.client_config.clone());
            let connecting = endpoint
                .connect(remote_addr, &self.server_name)
                .map_err(|error| format!("could not start Quinn handshake: {error}"))?;
            connecting
                .await
                .map_err(|error| format!("Quinn client handshake failed: {error}"))?
        } else {
            let incoming = endpoint.accept().await.ok_or_else(|| {
                "Quinn endpoint closed before an incoming peer arrived".to_owned()
            })?;
            incoming
                .await
                .map_err(|error| format!("Quinn server handshake failed: {error}"))?
        };

        self.active_endpoint = Some(endpoint);
        self.active_socket = Some(path_socket);
        Ok(connection)
    }

    pub fn close(&mut self) {
        if let Some(endpoint) = self.active_endpoint.take() {
            endpoint.close(quinn::VarInt::from_u32(0), b"call path replaced or closed");
        }
        self.active_socket.take();
    }

    pub const fn is_inviter(&self) -> bool {
        self.is_inviter
    }
}

impl<P: CallPathSocketProvider> CallPathConnector for QuinnCallPathConnector<P> {
    type ConnectFuture<'a>
        = Pin<Box<dyn Future<Output = Result<Connection, String>> + Send + 'a>>
    where
        P: 'a;

    fn connect(&mut self, path: NetworkPath) -> Self::ConnectFuture<'_> {
        Box::pin(async move {
            tokio::time::timeout(Duration::from_secs(100), self.connect_on_path(path))
                .await
                .map_err(|_| "Quinn endpoint setup timed out".to_owned())?
        })
    }

    fn close(&mut self) {
        QuinnCallPathConnector::close(self);
    }
}

impl<P> Drop for QuinnCallPathConnector<P> {
    fn drop(&mut self) {
        self.close();
    }
}

#[cfg(test)]
mod tests {
    use std::{
        future::{Future, Ready, ready},
        net::SocketAddr,
        sync::Arc,
        time::Duration,
    };

    use ed25519_dalek::SigningKey;
    use quinn::{ClientConfig, ServerConfig};
    use skyber_signaling::{
        AuthenticatedPeers, PeerIdentity, RoomId, SignalClient, SignalEvent, server,
    };
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt, DuplexStream},
        net::TcpListener,
        sync::watch,
        time::timeout,
    };
    use tokio_rustls::TlsConnector;
    use tokio_tungstenite::{WebSocketStream, client_async, tungstenite::protocol::Role};

    use super::{
        CallPathSocketProvider, QuicPathSocket, QuinnCallPathConnector, RelayWebSocketConnector,
        SignalingPathSocketProvider,
    };
    use crate::{
        call_controller::CallController, ice_quinn::RelayQuinnSocket, path_policy::NetworkPath,
    };

    struct RelayOnlyProvider {
        relay: Option<Arc<RelayQuinnSocket>>,
    }

    impl CallPathSocketProvider for RelayOnlyProvider {
        type SocketFuture<'a> = Ready<Result<QuicPathSocket, String>>;

        fn acquire(&mut self, path: NetworkPath) -> Self::SocketFuture<'_> {
            match path {
                NetworkPath::DirectUdp => ready(Err("direct UDP blocked in test".to_owned())),
                NetworkPath::WebSocketRelay => self
                    .relay
                    .take()
                    .map(QuicPathSocket::Relay)
                    .ok_or_else(|| "test relay already consumed".to_owned())
                    .pipe(ready),
            }
        }
    }

    trait Pipe: Sized {
        fn pipe<T>(self, f: impl FnOnce(Self) -> T) -> T {
            f(self)
        }
    }
    impl<T> Pipe for T {}

    struct TestWssConnector {
        address: SocketAddr,
        certificate: rustls::pki_types::CertificateDer<'static>,
    }

    impl RelayWebSocketConnector for TestWssConnector {
        type WebSocket = WebSocketStream<tokio_rustls::client::TlsStream<tokio::net::TcpStream>>;
        type ConnectFuture<'a> =
            std::pin::Pin<Box<dyn Future<Output = Result<Self::WebSocket, String>> + Send + 'a>>;

        fn connect(&mut self) -> Self::ConnectFuture<'_> {
            Box::pin(async move {
                connect_wss(self.address, self.certificate.clone(), "relay")
                    .await
                    .map_err(|error| error.to_string())
            })
        }
    }

    async fn connect_wss(
        address: SocketAddr,
        certificate: rustls::pki_types::CertificateDer<'static>,
        path: &str,
    ) -> anyhow::Result<WebSocketStream<tokio_rustls::client::TlsStream<tokio::net::TcpStream>>>
    {
        let mut roots = rustls::RootCertStore::empty();
        roots.add(certificate)?;
        let config = rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        let connector = TlsConnector::from(Arc::new(config));
        let tcp = tokio::net::TcpStream::connect(address).await?;
        let server_name = rustls::pki_types::ServerName::try_from("localhost".to_owned())?;
        let tls = connector.connect(server_name, tcp).await?;
        let (websocket, _) = client_async(format!("wss://localhost/{path}"), tls).await?;
        Ok(websocket)
    }

    async fn websocket_pair()
    -> anyhow::Result<(WebSocketStream<DuplexStream>, WebSocketStream<DuplexStream>)> {
        let (client_io, server_io) = tokio::io::duplex(512 * 1024);
        let (client, server) = tokio::join!(
            WebSocketStream::from_raw_socket(client_io, Role::Client, None),
            WebSocketStream::from_raw_socket(server_io, Role::Server, None),
        );
        Ok((client, server))
    }

    fn quic_configs() -> anyhow::Result<(ClientConfig, ServerConfig)> {
        let certified = rcgen::generate_simple_self_signed(vec!["localhost".to_owned()])?;
        let certificate = certified.cert.der().clone();
        let private_key = rustls::pki_types::PrivateKeyDer::Pkcs8(
            rustls::pki_types::PrivatePkcs8KeyDer::from(certified.signing_key.serialize_der()),
        );
        let server_tls = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![certificate.clone()], private_key)?;
        let server = ServerConfig::with_crypto(Arc::new(
            quinn::crypto::rustls::QuicServerConfig::try_from(server_tls)?,
        ));
        let mut roots = rustls::RootCertStore::empty();
        roots.add(certificate)?;
        let client_tls = rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        let client = ClientConfig::new(Arc::new(
            quinn::crypto::rustls::QuicClientConfig::try_from(client_tls)?,
        ));
        Ok((client, server))
    }

    #[tokio::test]
    async fn controller_falls_back_to_authenticated_relay_and_binds_both_identities()
    -> anyhow::Result<()> {
        let (client_ws, server_ws) = websocket_pair().await?;
        let inviter_addr: SocketAddr = "127.0.0.1:41000".parse()?;
        let invitee_addr: SocketAddr = "127.0.0.1:41001".parse()?;
        let inviter_relay = RelayQuinnSocket::new(client_ws, inviter_addr, invitee_addr);
        let invitee_relay = RelayQuinnSocket::new(server_ws, invitee_addr, inviter_addr);
        let (client_config, server_config) = quic_configs()?;
        let inviter_key = SigningKey::from_bytes(&[51; 32]);
        let invitee_key = SigningKey::from_bytes(&[52; 32]);
        let peers = AuthenticatedPeers {
            room: RoomId::from_bytes([53; 16]),
            inviter: PeerIdentity::from_signing_key(&inviter_key),
            invitee: PeerIdentity::from_signing_key(&invitee_key),
        };
        let mut inviter = CallController::new(
            QuinnCallPathConnector::new(
                RelayOnlyProvider {
                    relay: Some(inviter_relay),
                },
                true,
                client_config.clone(),
                server_config.clone(),
                "localhost".to_owned(),
            ),
            peers,
            inviter_key,
        )?;
        let mut invitee = CallController::new(
            QuinnCallPathConnector::new(
                RelayOnlyProvider {
                    relay: Some(invitee_relay),
                },
                false,
                client_config,
                server_config,
                "localhost".to_owned(),
            ),
            peers,
            invitee_key,
        )?;
        let (_cancel_inviter, mut cancel_inviter) = watch::channel(false);
        let (_cancel_invitee, mut cancel_invitee) = watch::channel(false);

        let (inviter_result, invitee_result) = timeout(Duration::from_secs(15), async {
            tokio::try_join!(
                inviter.connect(&mut cancel_inviter),
                invitee.connect(&mut cancel_invitee),
            )
        })
        .await??;
        assert_eq!(inviter_result.path(), NetworkPath::WebSocketRelay);
        assert_eq!(invitee_result.path(), NetworkPath::WebSocketRelay);
        assert_eq!(inviter_result.local_peer(), peers.inviter);
        assert_eq!(invitee_result.local_peer(), peers.invitee);
        inviter.hangup();
        invitee.hangup();
        Ok(())
    }

    #[tokio::test]
    async fn typed_signaling_provider_falls_back_to_authenticated_wss_and_authenticates()
    -> anyhow::Result<()> {
        let certified = rcgen::generate_simple_self_signed(vec!["localhost".to_owned()])?;
        let certificate = certified.cert.der().clone();
        let tls = server::tls_acceptor(
            certified.cert.pem().as_bytes(),
            certified.signing_key.serialize_pem().as_bytes(),
        )?;
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let server_task = tokio::spawn(server::serve(listener, tls));

        let inviter_key = SigningKey::from_bytes(&[71; 32]);
        let invitee_key = SigningKey::from_bytes(&[72; 32]);
        let inviter_peer = PeerIdentity::from_signing_key(&inviter_key);
        let invitee_peer = PeerIdentity::from_signing_key(&invitee_key);
        let mut inviter_signaling = SignalClient::from_authenticated_websocket(
            connect_wss(address, certificate.clone(), "signal").await?,
        );
        let invitation = inviter_signaling
            .create_invitation(invitee_peer, &inviter_key)
            .await?;
        let mut invitee_signaling = SignalClient::from_authenticated_websocket(
            connect_wss(address, certificate.clone(), "signal").await?,
        );
        let invitee_room = invitee_signaling
            .accept_invitation(
                invitation.room,
                &invitation.token,
                inviter_peer,
                &invitee_key,
            )
            .await?;
        let SignalEvent::InvitationAccepted(inviter_room) =
            timeout(Duration::from_secs(2), inviter_signaling.receive()).await??
        else {
            anyhow::bail!("expected accepted-room event for inviter");
        };
        let peers = AuthenticatedPeers {
            room: invitation.room,
            inviter: inviter_peer,
            invitee: invitee_peer,
        };
        let (client_config, server_config) = quic_configs()?;
        let (_cancel_inviter, mut cancel_inviter) = watch::channel(false);
        let (_cancel_invitee, mut cancel_invitee) = watch::channel(false);
        let inviter_provider = SignalingPathSocketProvider::new(
            inviter_signaling,
            TestWssConnector {
                address,
                certificate: certificate.clone(),
            },
            inviter_room,
            inviter_key.clone(),
            true,
            vec!["invalid-stun-url".to_owned()],
            cancel_inviter.clone(),
        );
        let invitee_provider = SignalingPathSocketProvider::new(
            invitee_signaling,
            TestWssConnector {
                address,
                certificate,
            },
            invitee_room,
            invitee_key.clone(),
            false,
            vec!["invalid-stun-url".to_owned()],
            cancel_invitee.clone(),
        );
        let mut inviter = CallController::new(
            QuinnCallPathConnector::new(
                inviter_provider,
                true,
                client_config.clone(),
                server_config.clone(),
                "localhost".to_owned(),
            ),
            peers,
            inviter_key,
        )?;
        let mut invitee = CallController::new(
            QuinnCallPathConnector::new(
                invitee_provider,
                false,
                client_config,
                server_config,
                "localhost".to_owned(),
            ),
            peers,
            invitee_key,
        )?;

        let (inviter_call, invitee_call) = timeout(Duration::from_secs(20), async {
            tokio::try_join!(
                inviter.connect(&mut cancel_inviter),
                invitee.connect(&mut cancel_invitee),
            )
        })
        .await??;
        assert_eq!(inviter_call.path(), NetworkPath::WebSocketRelay);
        assert_eq!(invitee_call.path(), NetworkPath::WebSocketRelay);
        assert_eq!(inviter_call.local_peer(), peers.inviter);
        assert_eq!(invitee_call.local_peer(), peers.invitee);

        // Kymux is created only after both controller connects have completed
        // the room-identity exporter exchange.
        let inviter_kyber = kymux::kynet::Connection::from(inviter_call.quic_connection().clone());
        let invitee_kyber = kymux::kynet::Connection::from(invitee_call.quic_connection().clone());
        let (mut sender, mut receiver) = inviter_kyber.open_bi().await?;
        sender.write_all(b"post-auth-kymux").await?;
        sender.finish().await?;
        let (mut reply, mut input) =
            timeout(Duration::from_secs(2), invitee_kyber.accept_bi()).await??;
        let mut payload = [0_u8; 15];
        timeout(Duration::from_secs(2), input.read_exact(&mut payload)).await??;
        anyhow::ensure!(&payload == b"post-auth-kymux", "unexpected Kyber payload");
        reply.write_all(b"ok").await?;
        reply.finish().await?;
        let mut acknowledgement = [0_u8; 2];
        timeout(
            Duration::from_secs(2),
            receiver.read_exact(&mut acknowledgement),
        )
        .await??;
        anyhow::ensure!(&acknowledgement == b"ok", "Kyber response was not received");

        inviter.hangup();
        invitee.hangup();
        server_task.abort();
        Ok(())
    }
}
