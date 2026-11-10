use std::{
    io::{self, Read, Write},
    net::Ipv4Addr,
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
const PREFIX: &str = "DL1-";

#[derive(Clone)]
pub struct Invite {
    pub address: Ipv4Addr,
    pub token: [u8; 16],
    pub fingerprint: [u8; 32],
}

impl Invite {
    pub fn new(address: Ipv4Addr, identity: &Identity) -> Self {
        let mut token = [0; 16];
        OsRng.fill_bytes(&mut token);
        Self {
            address,
            token,
            fingerprint: identity.fingerprint(),
        }
    }

    pub fn encode(&self) -> String {
        let mut bytes = [0; 52];
        bytes[..4].copy_from_slice(&self.address.octets());
        bytes[4..20].copy_from_slice(&self.token);
        bytes[20..].copy_from_slice(&self.fingerprint);
        format!("{PREFIX}{}", URL_SAFE_NO_PAD.encode(bytes))
    }

    pub fn decode(code: &str) -> Result<Self> {
        let payload = code
            .trim()
            .strip_prefix(PREFIX)
            .context("Код приглашения должен начинаться с DL1-.")?;
        let mut bytes = [0; 52];
        ensure!(
            URL_SAFE_NO_PAD.decode_slice(payload, &mut bytes).ok() == Some(bytes.len()),
            "Повреждённый код приглашения."
        );
        let address = Ipv4Addr::from(<[u8; 4]>::try_from(&bytes[..4])?);
        ensure!(
            permitted_address(address),
            "Для подключения нужен адрес Tailscale или локальной сети."
        );
        Ok(Self {
            address,
            token: bytes[4..20].try_into()?,
            fingerprint: bytes[20..].try_into()?,
        })
    }

    pub fn authorizes(&self, token: &[u8; 16]) -> bool {
        bool::from(self.token.ct_eq(token))
    }
}

pub fn tailscale_address(ip: Ipv4Addr) -> bool {
    let [a, b, _, _] = ip.octets();
    a == 100 && (64..128).contains(&b)
}

pub fn permitted_address(ip: Ipv4Addr) -> bool {
    ip.is_private() || tailscale_address(ip) || ip == Ipv4Addr::LOCALHOST
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

#[derive(Debug)]
struct Pin([u8; 32]);

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
        #[serde(default)]
        profile: Profile,
    },
    Ping,
    Leave,
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
    },
    Connected,
    Pong,
    Closed {
        reason: String,
    },
}

/// Encoder/bitrate trade-off chosen by the guest and applied on both ends.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Profile {
    /// Fastest encoder presets, Moonlight's reference bitrate.
    #[default]
    Latency,
    /// Fast presets with extra bitrate headroom.
    Balanced,
    /// Slower presets and a high bitrate for the cleanest image.
    Quality,
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
    fn invite_round_trip_and_validation() {
        let invite = Invite {
            address: Ipv4Addr::new(100, 64, 1, 2),
            token: [7; 16],
            fingerprint: [3; 32],
        };
        let code = invite.encode();
        let decoded = Invite::decode(&format!(" {code}\r\n")).unwrap();
        assert_eq!(decoded.address, invite.address);
        assert_eq!(decoded.fingerprint, invite.fingerprint);
        assert!(decoded.authorizes(&invite.token));
        assert!(!decoded.authorizes(&[6; 16]));
        assert!(Invite::decode(&code[..code.len() - 1]).is_err());
        assert!(
            Invite::decode(
                &Invite {
                    address: Ipv4Addr::new(8, 8, 8, 8),
                    ..invite
                }
                .encode()
            )
            .is_err()
        );
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
