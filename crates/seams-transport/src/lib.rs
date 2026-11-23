//! QUIC transport with mutual self-signed certificate pinning. Each protocol message
//! travels as exactly one unidirectional QUIC stream, which gives us framing and
//! multiplexing without any custom wire format on top of the payload.

use std::net::SocketAddr;
use std::sync::Arc;

use blake3::Hasher;
use quinn::{Connection, Endpoint};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{verify_tls12_signature, verify_tls13_signature, CryptoProvider};
use rustls::pki_types::{
    CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime,
};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::{DigitallySignedStruct, DistinguishedName, Error as TlsError, SignatureScheme};
use seams_identity::{DeviceId, DeviceIdentity};
use seams_proto::{decode, encode, Message};
use thiserror::Error;

/// Upper bound for one protocol message on a stream.
pub const MAX_MESSAGE_BYTES: usize = 16 * 1024 * 1024;

#[derive(Debug, Error)]
pub enum TransportError {
    #[error("socket: {0}")]
    Socket(#[from] std::io::Error),
    #[error("transport closed")]
    Closed,
    #[error("connection failed: {0}")]
    Connect(#[from] quinn::ConnectError),
    #[error("connection lost: {0}")]
    Connection(#[from] quinn::ConnectionError),
    #[error("stream write failed: {0}")]
    Write(#[from] quinn::WriteError),
    #[error("stream already finished: {0}")]
    StreamClosed(#[from] quinn::ClosedStream),
    #[error("tls config is incompatible with quic: {0}")]
    IncompatibleConfig(String),
    #[error("stream read failed: {0}")]
    Read(#[from] quinn::ReadError),
    #[error("stream read failed: {0}")]
    ReadToEnd(#[from] quinn::ReadToEndError),
    #[error("protocol: {0}")]
    Proto(#[from] seams_proto::ProtoError),
    #[error("tls: {0}")]
    Tls(#[from] TlsError),
}

/// Client-side verifier: the peer's certificate must hash to the pinned device ID.
#[derive(Debug)]
struct PinServer {
    expected: DeviceId,
    provider: Arc<CryptoProvider>,
}

impl ServerCertVerifier for PinServer {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, TlsError> {
        let mut hasher = Hasher::new();
        hasher.update(end_entity.as_ref());
        if DeviceId(*hasher.finalize().as_bytes()) == self.expected {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(TlsError::General(
                "server device ID does not match the pinned value".into(),
            ))
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// Server-side verifier: any self-signed certificate with a valid signature is accepted
/// for the handshake. The pairing allow-list is enforced above the transport; pinning
/// the client happens as soon as its identity is known.
#[derive(Debug)]
struct AcceptAnyClient {
    provider: Arc<CryptoProvider>,
}

impl ClientCertVerifier for AcceptAnyClient {
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }

    fn verify_client_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> Result<ClientCertVerified, TlsError> {
        Ok(ClientCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

fn leaf_certificate(identity: &DeviceIdentity) -> CertificateDer<'static> {
    CertificateDer::from(identity.cert_der().to_vec())
}

fn private_key(identity: &DeviceIdentity) -> PrivateKeyDer<'static> {
    PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(identity.key_pkcs8_der()))
}

/// A bound endpoint that presents one device identity and dials pinned peers.
/// Cloning shares the same underlying socket, e.g. for concurrent accept loops.
#[derive(Clone)]
pub struct Transport {
    endpoint: Endpoint,
}

impl Transport {
    /// Binds a QUIC endpoint that also serves as the dialing socket.
    pub fn bind(identity: Arc<DeviceIdentity>, addr: SocketAddr) -> Result<Self, TransportError> {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let verifier = Arc::new(AcceptAnyClient {
            provider: provider.clone(),
        });

        let tls = rustls::ServerConfig::builder_with_provider(provider)
            .with_protocol_versions(&[&rustls::version::TLS13])
            .expect("TLS 1.3 is compiled in")
            .with_client_cert_verifier(verifier)
            .with_single_cert(vec![leaf_certificate(&identity)], private_key(&identity))?;

        let quic_server_config = quinn::crypto::rustls::QuicServerConfig::try_from(tls)
            .map_err(|e| TransportError::IncompatibleConfig(e.to_string()))?;
        let server_config = quinn::ServerConfig::with_crypto(Arc::new(quic_server_config));
        let endpoint = Endpoint::server(server_config, addr)?;
        Ok(Self { endpoint })
    }

    /// Dials `addr` and aborts the handshake unless the peer's certificate hashes to
    /// exactly `expected`.
    pub async fn connect(
        &self,
        identity: &DeviceIdentity,
        addr: SocketAddr,
        expected: DeviceId,
    ) -> Result<Connection, TransportError> {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let verifier = Arc::new(PinServer {
            expected,
            provider: provider.clone(),
        });

        let tls = rustls::ClientConfig::builder_with_provider(provider)
            .with_protocol_versions(&[&rustls::version::TLS13])
            .expect("TLS 1.3 is compiled in")
            .dangerous()
            .with_custom_certificate_verifier(verifier)
            .with_client_auth_cert(vec![leaf_certificate(identity)], private_key(identity))?;

        let quic_client_config = quinn::crypto::rustls::QuicClientConfig::try_from(tls)
            .map_err(|e| TransportError::IncompatibleConfig(e.to_string()))?;
        let connection = self
            .endpoint
            .connect_with(
                quinn::ClientConfig::new(Arc::new(quic_client_config)),
                addr,
                "seams",
            )?
            .await?;
        Ok(connection)
    }

    pub fn local_addr(&self) -> Result<SocketAddr, TransportError> {
        Ok(self.endpoint.local_addr()?)
    }

    /// Accepts the next inbound connection; the peer's device ID arrives inside Hello.
    pub async fn accept(&self) -> Result<Connection, TransportError> {
        let incoming = self.endpoint.accept().await.ok_or(TransportError::Closed)?;
        Ok(incoming.await?)
    }
}

/// Sends one message on a fresh unidirectional stream.
pub async fn send_message(conn: &Connection, msg: &Message) -> Result<(), TransportError> {
    let bytes = encode(msg)?;
    let mut stream = conn.open_uni().await?;
    stream.write_all(&bytes).await?;
    stream.finish()?;
    Ok(())
}

/// Receives one message; the stream is expected to carry exactly one.
pub async fn recv_message(conn: &Connection) -> Result<Message, TransportError> {
    let mut stream = conn.accept_uni().await?;
    let bytes = stream.read_to_end(MAX_MESSAGE_BYTES).await?;
    Ok(decode(&bytes)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use seams_proto::{Hello, PROTOCOL_VERSION};
    use std::net::Ipv4Addr;

    fn loopback(port: u16) -> SocketAddr {
        SocketAddr::from((Ipv4Addr::LOCALHOST, port))
    }

    #[tokio::test]
    async fn pinned_client_exchanges_messages_with_server() {
        let server_identity = Arc::new(DeviceIdentity::generate());
        let client_identity = Arc::new(DeviceIdentity::generate());

        let transport = Transport::bind(server_identity.clone(), loopback(0)).unwrap();
        let server_addr = transport.local_addr().unwrap();

        let expected_id = server_identity.device_id();
        let server_transport = transport.clone();
        let server = tokio::spawn(async move {
            let conn = server_transport.accept().await.unwrap();
            let msg = recv_message(&conn).await.unwrap();
            send_message(&conn, &msg).await.unwrap();
            (server_transport, conn, msg)
        });

        let conn = transport
            .connect(&client_identity, server_addr, expected_id)
            .await
            .unwrap();

        let hello = Message::Hello(Hello {
            protocol_version: PROTOCOL_VERSION,
            device_id: client_identity.device_id(),
            device_name: "client".into(),
        });
        send_message(&conn, &hello).await.unwrap();
        let echoed = recv_message(&conn).await.unwrap();
        assert_eq!(echoed, hello);

        let (_, _, received) = server.await.unwrap();
        assert_eq!(received, hello);
    }

    #[tokio::test]
    async fn wrong_pinned_device_id_fails_the_handshake() {
        let server_identity = Arc::new(DeviceIdentity::generate());
        let client_identity = Arc::new(DeviceIdentity::generate());
        let impostor = DeviceIdentity::generate();

        let transport = Transport::bind(server_identity.clone(), loopback(0)).unwrap();
        let server_addr = transport.local_addr().unwrap();

        let server_transport = transport.clone();
        let server = tokio::spawn(async move {
            // The server side of a rejected handshake may either fail or wait forever on
            // accept; either way the test only asserts the client outcome.
            let _ = server_transport.accept().await;
        });

        let result = transport
            .connect(&client_identity, server_addr, impostor.device_id())
            .await;
        assert!(result.is_err(), "connection to an unpinned server must fail");

        let _ = server.await;
    }
}
