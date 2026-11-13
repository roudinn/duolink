use std::{
    io::{self, Read, Write},
    net::{Ipv4Addr, SocketAddrV4},
    sync::Arc,
};

use anyhow::{Context, Result, bail, ensure};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use rand::{RngCore, rngs::OsRng};
use rcgen::{CertificateParams, DnType, KeyPair, PKCS_RSA_SHA256};
use rsa::{RsaPrivateKey, pkcs8::EncodePrivateKey};
use rustls::{
    ClientConfig, DigitallySignedStruct, ServerConfig, SignatureScheme,
    client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
    crypto::{verify_tls12_signature, verify_tls13_signature},
    pki_types::{CertificateDer, PrivatePkcs8KeyDer, ServerName, UnixTime},
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use uuid::Uuid;

pub const CONTROL_PORT: u16 = 48171;
pub const MEDIA_PORT: u16 = 48189;
const MAX_FRAME: usize = 8192;
const PREFIX: &str = "DL2-";
const MAX_CANDIDATES: usize = 12;

/// How a candidate address reaches the host.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PathKind {
    /// An address of one of the host's own interfaces (LAN, VPN, loopback).
    Local = 0,
    /// A port the host's router forwards through UPnP.
    Forwarded = 1,
    /// The host's public address as seen by STUN; needs NAT hole punching.
    Public = 2,
}

impl PathKind {
    fn from_byte(byte: u8) -> Option<Self> {
        match byte {
            0 => Some(Self::Local),
            1 => Some(Self::Forwarded),
            2 => Some(Self::Public),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Candidate {
    pub kind: PathKind,
    pub addr: SocketAddrV4,
}

/// Everything a guest needs to find and authenticate the host: the one-time
/// token, the host certificate fingerprint and every address it may answer on.
#[derive(Clone, Debug)]
pub struct Invite {
    pub token: [u8; 16],
    pub fingerprint: [u8; 32],
    pub candidates: Vec<Candidate>,
}

pub fn new_token() -> [u8; 16] {
    let mut token = [0; 16];
    OsRng.fill_bytes(&mut token);
    token
}

impl Invite {
    pub fn encode(&self) -> String {
        let candidates = &self.candidates[..self.candidates.len().min(MAX_CANDIDATES)];
        let mut bytes = Vec::with_capacity(49 + 7 * candidates.len());
        bytes.extend_from_slice(&self.token);
        bytes.extend_from_slice(&self.fingerprint);
        bytes.push(candidates.len() as u8);
        for candidate in candidates {
            bytes.push(candidate.kind as u8);
            bytes.extend_from_slice(&candidate.addr.ip().octets());
            bytes.extend_from_slice(&candidate.addr.port().to_be_bytes());
        }
        format!("{PREFIX}{}", URL_SAFE_NO_PAD.encode(bytes))
    }

    pub fn decode(code: &str) -> Result<Self> {
        let payload = code.trim().strip_prefix(PREFIX).context(
            "Код сессии начинается с DL2-.",
        )?;
        let bytes = URL_SAFE_NO_PAD
            .decode(payload)
            .ok()
            .context("Повреждённый код приглашения.")?;
        ensure!(bytes.len() >= 49, "Повреждённый код приглашения.");
        let count = bytes[48] as usize;
        ensure!(
            count > 0 && count <= MAX_CANDIDATES && bytes.len() == 49 + 7 * count,
            "Повреждённый код приглашения."
        );
        let candidates = bytes[49..]
            .chunks_exact(7)
            .map(|chunk| {
                Some(Candidate {
                    kind: PathKind::from_byte(chunk[0])?,
                    addr: SocketAddrV4::new(
                        Ipv4Addr::new(chunk[1], chunk[2], chunk[3], chunk[4]),
                        u16::from_be_bytes([chunk[5], chunk[6]]),
                    ),
                })
            })
            .collect::<Option<Vec<_>>>()
            .context("Повреждённый код приглашения.")?;
        Ok(Self {
            token: bytes[..16].try_into()?,
            fingerprint: bytes[16..48].try_into()?,
            candidates,
        })
    }

    pub fn authorizes(&self, token: &[u8; 16]) -> bool {
        bool::from(self.token.ct_eq(token))
    }
}

pub struct Identity {
    pub cert_der: CertificateDer<'static>,
    pub cert_pem: String,
    pub key_pem: String,
    key_der: PrivatePkcs8KeyDer<'static>,
}

impl Identity {
    pub fn generate(common_name: &str) -> Result<Self> {
        // Moonlight's Windows identity loader requires RSA. The same host identity
        // authenticates the invite's TLS connection and the GameStream connection.
        let rsa = RsaPrivateKey::new(&mut OsRng, 2048)?;
        let document = rsa.to_pkcs8_der()?;
        let private_key = PrivatePkcs8KeyDer::from(document.as_bytes().to_vec());
        let key = KeyPair::from_pkcs8_der_and_sign_algo(&private_key, &PKCS_RSA_SHA256)?;
        let mut params = CertificateParams::new(Vec::<String>::new())?;
        params
            .distinguished_name
            .push(DnType::CommonName, common_name);
        let certificate = params.self_signed(&key)?;
        Ok(Self {
            cert_der: certificate.der().clone(),
            cert_pem: certificate.pem(),
            key_pem: key.serialize_pem(),
            key_der: private_key,
        })
    }

    pub fn key_der(&self) -> PrivatePkcs8KeyDer<'static> {
        self.key_der.clone_key()
    }

    pub fn fingerprint(&self) -> [u8; 32] {
        Sha256::digest(self.cert_der.as_ref()).into()
    }

    pub fn server_config(&self) -> Result<Arc<ServerConfig>> {
        Ok(Arc::new(
            ServerConfig::builder()
                .with_no_client_auth()
                .with_single_cert(vec![self.cert_der.clone()], self.key_der.clone_key().into())?,
        ))
    }
}

/// Accepts exactly one server certificate: the one whose SHA-256 is in the invite.
#[derive(Debug)]
pub struct Pin(pub [u8; 32]);

impl ServerCertVerifier for Pin {
    fn verify_server_cert(
        &self,
        leaf: &CertificateDer<'_>,
        _: &[CertificateDer<'_>],
        _: &ServerName<'_>,
        _: &[u8],
        _: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let actual: [u8; 32] = Sha256::digest(leaf.as_ref()).into();
        if bool::from(actual.ct_eq(&self.0)) {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::InvalidCertificate(
                rustls::CertificateError::ApplicationVerificationFailure,
            ))
        }
    }

    fn verify_tls12_signature(
        &self,
        data: &[u8],
        cert: &CertificateDer<'_>,
        signature: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(
            data,
            cert,
            signature,
            &rustls::crypto::ring::default_provider().signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        data: &[u8],
        cert: &CertificateDer<'_>,
        signature: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(
            data,
            cert,
            signature,
            &rustls::crypto::ring::default_provider().signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

pub fn client_config(invite: &Invite) -> Arc<ClientConfig> {
    Arc::new(
        ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(Pin(invite.fingerprint)))
            .with_no_client_auth(),
    )
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Request {
    Join {
        token: [u8; 16],
        name: String,
        certificate: String,
        /// Forward error correction the guest measured it needs, in percent.
        #[serde(default)]
        fec: Option<u8>,
        /// The guest takes the client key from Ready (lobby hosts, 1.10+).
        #[serde(default)]
        lobby: bool,
        /// A disconnected physical controller releases its virtual controller.
        #[serde(default)]
        controller_hotplug: bool,
        #[serde(default)]
        audio_opus: bool,
        #[serde(default)]
        bitrate_restart: bool,
    },
    Ping,
    Leave,
    /// What the guest's path needs, measured while the host decides.
    Measured {
        fec: u8,
        #[serde(default)]
        bandwidth: Option<StreamBandwidth>,
    },
    /// Actual request to Moonlight, sent only after a host advertises a budget.
    Bitrate { kbps: u32 },
    /// The old Moonlight process has exited and the replacement can start.
    RestartReady { closed_cleanly: bool },
}

#[derive(Clone, Copy, Serialize, Deserialize)]
pub struct StreamBandwidth {
    pub path_kbps: Option<u32>,
    pub manual_kbps: Option<u32>,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Reply {
    Pending,
    Preparing,
    Ready {
        host: Uuid,
        certificate: String,
        mode: Mode,
        /// Game audio comes from DuoLink without voice chat; Moonlight stays silent.
        #[serde(default)]
        own_audio: bool,
        #[serde(default)]
        audio_opus: bool,
        #[serde(default)]
        auto_bitrate_kbps: Option<u32>,
        /// The client identity Sunshine already trusts.
        #[serde(default)]
        client: Option<ClientKey>,
    },
    Connected,
    /// Reopen this automatic stream at a lower rate while keeping its lobby slot.
    LowerBitrate { kbps: u32 },
    RestartStream,
    Pong,
    Closed {
        reason: String,
    },
}

#[derive(Clone, Serialize, Deserialize)]
pub struct ClientKey {
    pub certificate: String,
    pub key: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Mode {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
}

pub struct Wire<S> {
    stream: S,
    buffer: Vec<u8>,
}

impl<S: Read + Write> Wire<S> {
    pub fn new(stream: S) -> Self {
        Self {
            stream,
            buffer: Vec::with_capacity(2048),
        }
    }

    pub fn send(&mut self, message: &impl Serialize) -> Result<()> {
        let mut bytes = serde_json::to_vec(message)?;
        ensure!(bytes.len() < MAX_FRAME, "Сообщение слишком длинное.");
        bytes.push(b'\n');
        self.stream.write_all(&bytes)?;
        self.stream.flush()?;
        Ok(())
    }

    pub fn poll<T: DeserializeOwned>(&mut self) -> Result<Option<T>> {
        if let Some(message) = self.take_frame()? {
            return Ok(Some(message));
        }
        let mut block = [0; 2048];
        match self.stream.read(&mut block) {
            Ok(0) => bail!("Соединение закрыто."),
            Ok(count) => self.buffer.extend_from_slice(&block[..count]),
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
                ) =>
            {
                return Ok(None);
            }
            Err(error) => return Err(error.into()),
        }
        self.take_frame()
    }

    fn take_frame<T: DeserializeOwned>(&mut self) -> Result<Option<T>> {
        let Some(end) = self.buffer.iter().position(|byte| *byte == b'\n') else {
            ensure!(self.buffer.len() < MAX_FRAME, "Сообщение слишком длинное.");
            return Ok(None);
        };
        ensure!(end < MAX_FRAME, "Сообщение слишком длинное.");
        let message = serde_json::from_slice(&self.buffer[..end])
            .context("Неверный формат сообщения DuoLink.")?;
        self.buffer.drain(..=end);
        Ok(Some(message))
    }
}

pub fn valid_pem(certificate: &str) -> bool {
    let mut lines = certificate.trim_end().lines();
    if lines.next() != Some("-----BEGIN CERTIFICATE-----") {
        return false;
    }
    let Some(last) = lines.next_back() else {
        return false;
    };
    last == "-----END CERTIFICATE-----"
        && certificate.len() <= 4096
        && lines.all(|line| {
            !line.is_empty()
                && line
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"+/=".contains(&b))
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn legacy_messages_keep_pcm_and_local_bitrate_selection() {
        let token = [0u8; 16];
        let join = serde_json::json!({
            "type": "join", "token": token, "name": "Guest", "certificate": "",
            "lobby": true, "controller_hotplug": true
        });
        let ready = serde_json::json!({
            "type": "ready", "host": Uuid::nil(), "certificate": "", "own_audio": true,
            "mode": Mode { width: 1920, height: 1080, fps: 60 }
        });
        let measured = serde_json::json!({ "type": "measured", "fec": 20 });
        assert!(matches!(
            serde_json::from_value(join).unwrap(),
            Request::Join { audio_opus: false, bitrate_restart: false, .. }
        ));
        assert!(matches!(
            serde_json::from_value(ready).unwrap(),
            Reply::Ready { own_audio: true, audio_opus: false, auto_bitrate_kbps: None, .. }
        ));
        assert!(matches!(
            serde_json::from_value(measured).unwrap(),
            Request::Measured { fec: 20, bandwidth: None }
        ));
    }

    #[test]
    fn invite_round_trip_and_validation() {
        let invite = Invite {
            token: [7; 16],
            fingerprint: [3; 32],
            candidates: vec![
                Candidate {
                    kind: PathKind::Forwarded,
                    addr: SocketAddrV4::new(Ipv4Addr::new(203, 0, 113, 9), 48170),
                },
                Candidate {
                    kind: PathKind::Local,
                    addr: SocketAddrV4::new(Ipv4Addr::new(192, 168, 1, 20), 48170),
                },
            ],
        };
        let code = invite.encode();
        let decoded = Invite::decode(&format!(" {code}\r\n")).unwrap();
        assert_eq!(decoded.candidates, invite.candidates);
        assert_eq!(decoded.fingerprint, invite.fingerprint);
        assert!(decoded.authorizes(&invite.token));
        assert!(!decoded.authorizes(&[6; 16]));
        assert!(Invite::decode(&code[..code.len() - 1]).is_err());
        assert!(Invite::decode("DL1-AAAA").is_err());
    }

    #[test]
    fn frames_handle_partial_and_multiple_reads() {
        let padding = "a".repeat(2600);
        let data =
            format!("{{\"type\":\"closed\",\"reason\":\"{padding}\"}}\n{{\"type\":\"pong\"}}\n");
        let mut wire = Wire::new(Cursor::new(data.into_bytes()));
        assert!(wire.poll::<Reply>().unwrap().is_none());
        assert!(
            matches!(wire.poll::<Reply>().unwrap(), Some(Reply::Closed { reason }) if reason == padding)
        );
        assert!(matches!(wire.poll::<Reply>().unwrap(), Some(Reply::Pong)));
        let mut oversized = Wire::new(Cursor::new(vec![b'x'; MAX_FRAME + 1]));
        for _ in 0..3 {
            assert!(oversized.poll::<Reply>().unwrap().is_none());
        }
        assert!(oversized.poll::<Reply>().is_err());
    }
}
