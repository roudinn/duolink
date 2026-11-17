//! Pair each guest with the private local Sunshine instance.
use std::{
    fs,
    io::{self, BufRead, BufReader, Read, Write},
    net::{Ipv4Addr, SocketAddr, TcpStream},
    path::Path,
    sync::Arc,
    thread,
    time::Duration,
};

use aes::{
    Aes128,
    cipher::{BlockDecrypt, BlockEncrypt, KeyInit},
};
use anyhow::{Context, Result, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use rand::{Rng, rngs::OsRng};
use ring::signature::{
    RSA_PKCS1_2048_8192_SHA256, RSA_PKCS1_SHA256, RsaKeyPair, UnparsedPublicKey,
};
use rustls::{ClientConfig, ClientConnection, StreamOwned};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use uuid::Uuid;

use crate::protocol::{ClientKey, Identity, MEDIA_PORT, Pin};

pub struct Pairing {
    address: Ipv4Addr,
    tls: Arc<ClientConfig>,
    authorization: String,
}

impl Pairing {
    pub fn configure(address: Ipv4Addr, host: &Identity, credentials: &Path) -> Result<Self> {
        let password = Uuid::new_v4().simple().to_string();
        let salt = Uuid::new_v4().simple().to_string();
        let mut hash = Sha256::digest(format!("{password}{salt}"));
        hash.reverse();
        fs::write(
            credentials,
            serde_json::to_vec(&json!({
                "username": "duolink",
                "salt": salt,
                "password": hex::encode_upper(hash),
            }))?,
        )?;
        let tls = ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(Pin(host.fingerprint())))
            .with_no_client_auth();
        Ok(Self {
            address,
            tls: Arc::new(tls),
            authorization: STANDARD.encode(format!("duolink:{password}")),
        })
    }

    pub fn add(&self, host: &Identity) -> Result<(ClientKey, String)> {
        let client = Identity::generate("DuoLink Guest")?;
        let uid = Uuid::new_v4().simple().to_string();
        let name = format!("DuoLink-{uid}");
        let pin = format!("{:04}", OsRng.gen_range(0..10_000));
        let salt: [u8; 16] = rand::random();
        let path = format!(
            "phrase=getservercert&devicename={name}&salt={}&clientcert={}",
            hex::encode(salt),
            hex::encode(client.cert_pem.as_bytes())
        );
        let socket = self
            .start_exchange(&uid, &path)
            .context("Opening local Sunshine pairing")?;
        let pending = (0..25)
            .find_map(|_| {
                let pending = self.admin("/api/pin", None).ok()?;
                let id = pending["pairings"]
                    .as_array()?
                    .iter()
                    .find(|request| request["name"] == name)
                    .and_then(|request| request["id"].as_str())
                    .map(str::to_owned);
                if id.is_none() {
                    thread::sleep(Duration::from_millis(40));
                }
                id
            })
            .context("Sunshine did not list the guest pairing request")?;
        // Sunshine sends the certificate first, then holds the PIN response
        // open until the remaining GameStream pairing exchanges complete.
        thread::scope(|scope| {
            let approval = scope.spawn(|| {
                self.admin(
                    "/api/pin",
                    Some(json!({ "pairing_id": pending, "pin": pin, "name": name })),
                )
            });
            let result = self.complete(host, &client, &uid, &pin, salt, socket);
            let approved = approval
                .join()
                .map_err(|_| anyhow::anyhow!("Sunshine PIN worker failed"))?;
            result?;
            ensure!(
                approved?["status"] == true,
                "Sunshine rejected the local pairing PIN"
            );
            Ok(())
        })?;
        Ok((
            ClientKey {
                certificate: client.cert_pem,
                key: client.key_pem,
            },
            name,
        ))
    }

    fn complete(
        &self,
        host: &Identity,
        client: &Identity,
        uid: &str,
        pin: &str,
        salt: [u8; 16],
        socket: TcpStream,
    ) -> Result<()> {
        let first = read_response(socket).context("Reading the approved Sunshine certificate")?;
        let pem = String::from_utf8(hex::decode(field(&first, "plaincert")?)?)?;
        let certificate = pem
            .lines()
            .filter(|line| !line.starts_with("-----"))
            .collect::<String>();
        ensure!(
            STANDARD.decode(certificate)? == host.cert_der.as_ref(),
            "Unexpected Sunshine certificate"
        );

        let key = Sha256::new()
            .chain_update(salt)
            .chain_update(pin.as_bytes())
            .finalize();
        let cipher = Aes128::new_from_slice(&key[..16]).expect("AES-128 key size");
        let challenge: [u8; 16] = rand::random();
        let reply = self.exchange(uid, "clientchallenge", crypt(&cipher, &challenge, true))?;
        ensure!(reply.len() == 48, "Invalid Sunshine pairing challenge");
        let reply = crypt(&cipher, &reply, false);
        let client_secret: [u8; 16] = rand::random();
        let mut hash = Sha256::new();
        hash.update(&reply[32..]);
        hash.update(cert_signature(client)?);
        hash.update(client_secret);
        let secret = self.exchange(
            &uid,
            "serverchallengeresp",
            crypt(&cipher, &hash.finalize(), true),
        )?;
        ensure!(secret.len() == 272, "Invalid Sunshine pairing secret");
        let server_key = RsaKeyPair::from_pkcs8(host.key_der().secret_pkcs8_der())
            .map_err(|_| anyhow::anyhow!("Invalid Sunshine RSA key"))?;
        UnparsedPublicKey::new(&RSA_PKCS1_2048_8192_SHA256, server_key.public().as_ref())
            .verify(&secret[..16], &secret[16..])
            .map_err(|_| anyhow::anyhow!("Invalid Sunshine pairing signature"))?;
        let expected = Sha256::new()
            .chain_update(challenge)
            .chain_update(cert_signature(host)?)
            .chain_update(&secret[..16])
            .finalize();
        ensure!(
            bool::from(expected.as_slice().ct_eq(&reply[..32])),
            "Sunshine pairing challenge did not match"
        );

        let client_key = RsaKeyPair::from_pkcs8(client.key_der().secret_pkcs8_der())
            .map_err(|_| anyhow::anyhow!("Invalid guest RSA key"))?;
        let mut signed = [0; 272];
        signed[..16].copy_from_slice(&client_secret);
        client_key
            .sign(
                &RSA_PKCS1_SHA256,
                &ring::rand::SystemRandom::new(),
                &client_secret,
                &mut signed[16..],
            )
            .map_err(|_| anyhow::anyhow!("Could not sign guest pairing challenge"))?;
        let response = read_response(self.start_exchange(
            &uid,
            &format!("clientpairingsecret={}", hex::encode(signed)),
        )?)?;
        ensure!(
            field(&response, "paired")? == "1",
            "Sunshine refused the guest certificate"
        );
        Ok(())
    }

    pub fn remove(&self, name: &str) -> Result<()> {
        let clients = self.admin("/api/clients/list", None)?;
        let uuid = clients["named_certs"]
            .as_array()
            .context("Missing Sunshine client list")?
            .iter()
            .find(|client| client["name"] == name)
            .and_then(|client| client["uuid"].as_str());
        if let Some(uuid) = uuid {
            // Disable first to terminate the stream and its retained input state.
            self.admin(
                "/api/clients/update",
                Some(json!({ "uuid": uuid, "enabled": false })),
            )?;
            self.admin("/api/clients/unpair", Some(json!({ "uuid": uuid })))?;
        }
        Ok(())
    }

    fn admin(&self, path: &str, body: Option<Value>) -> Result<Value> {
        let port = MEDIA_PORT + 1;
        let tcp = self.connect(port)?;
        let tls = ClientConnection::new(self.tls.clone(), self.address.into())?;
        let mut socket = StreamOwned::new(tls, tcp);
        let method = if body.is_some() { "POST" } else { "GET" };
        let body = body.map(|value| value.to_string()).unwrap_or_default();
        write!(
            socket,
            "{method} {path} HTTP/1.1\r\nHost: {}:{port}\r\nConnection: close\r\nAuthorization: Basic {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            self.address,
            self.authorization,
            body.len()
        )?;
        Ok(serde_json::from_str(
            &read_response(socket).with_context(|| format!("Sunshine API {path}"))?,
        )?)
    }

    fn connect(&self, port: u16) -> Result<TcpStream> {
        let timeout = Duration::from_secs(2);
        let socket = TcpStream::connect_timeout(&SocketAddr::from((self.address, port)), timeout)?;
        socket.set_read_timeout(Some(timeout))?;
        socket.set_write_timeout(Some(timeout))?;
        Ok(socket)
    }

    fn start_exchange(&self, uid: &str, args: &str) -> Result<TcpStream> {
        let mut socket = self.connect(MEDIA_PORT)?;
        write!(
            socket,
            "GET /pair?uniqueid={uid}&{args} HTTP/1.1\r\nHost: {}:{MEDIA_PORT}\r\nConnection: close\r\n\r\n",
            self.address
        )?;
        Ok(socket)
    }

    fn exchange(&self, uid: &str, phase: &str, payload: Vec<u8>) -> Result<Vec<u8>> {
        let response =
            read_response(self.start_exchange(uid, &format!("{phase}={}", hex::encode(payload)))?)?;
        let response_field = if phase == "clientchallenge" {
            "challengeresponse"
        } else {
            "pairingsecret"
        };
        Ok(hex::decode(field(&response, response_field)?)?)
    }
}

fn cert_signature(identity: &Identity) -> Result<&[u8]> {
    // Identity::generate produces RSA-2048 DER certificates ending in a
    // BIT STRING with 0 unused bits and a 256-byte PKCS#1 signature.
    let der = identity.cert_der.as_ref();
    let field = der
        .get(der.len().saturating_sub(261)..)
        .context("Missing certificate signature")?;
    ensure!(
        field.len() == 261 && field.starts_with(&[0x03, 0x82, 0x01, 0x01, 0]),
        "Invalid certificate signature"
    );
    Ok(&field[5..])
}

fn crypt(cipher: &Aes128, input: &[u8], encrypt: bool) -> Vec<u8> {
    let mut output = input.to_vec();
    for bytes in output.chunks_exact_mut(16) {
        let block = aes::Block::from_mut_slice(bytes);
        if encrypt {
            cipher.encrypt_block(block);
        } else {
            cipher.decrypt_block(block);
        }
    }
    output
}

fn field<'a>(xml: &'a str, tag: &str) -> Result<&'a str> {
    xml.split_once(&format!("<{tag}>"))
        .and_then(|(_, tail)| tail.split_once(&format!("</{tag}>")))
        .map(|(value, _)| value)
        .with_context(|| format!("Missing Sunshine response field {tag}"))
}

fn read_response(socket: impl Read) -> Result<String> {
    const LIMIT: u64 = 1024 * 1024;
    let mut reader = BufReader::new(socket.take(LIMIT));
    let mut line = String::new();
    reader.read_line(&mut line)?;
    ensure!(
        line.split_whitespace().nth(1) == Some("200"),
        "Sunshine HTTP response: {}",
        line.trim()
    );
    let mut length = None;
    loop {
        line.clear();
        ensure!(
            reader.read_line(&mut line)? != 0,
            "Incomplete Sunshine HTTP headers"
        );
        if line == "\r\n" {
            break;
        }
        if let Some((name, value)) = line.split_once(':')
            && name.eq_ignore_ascii_case("Content-Length")
        {
            length = Some(value.trim().parse::<usize>()?);
        }
    }
    let mut body = Vec::new();
    if let Some(length) = length {
        ensure!(length as u64 <= LIMIT, "Sunshine HTTP response too large");
        body.resize(length, 0);
        reader.read_exact(&mut body)?;
    } else if let Err(error) = reader.read_to_end(&mut body) {
        if error.kind() != io::ErrorKind::UnexpectedEof || body.is_empty() {
            return Err(error.into());
        }
    }
    Ok(String::from_utf8(body)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::{DEFAULT_FEC, HostStream, SUNSHINE};
    use std::time::Instant;

    #[test]
    #[ignore = "starts the bundled Sunshine on an isolated loopback address"]
    fn sunshine_pairs_and_removes_two_independent_guests() {
        let installed = std::env::var_os("LOCALAPPDATA").unwrap();
        let exe = SUNSHINE.path(&Path::new(&installed).join("DuoLink"));
        let data = tempfile::tempdir().unwrap();
        let address = Ipv4Addr::new(127, 0, 0, 3);
        let identity = Identity::generate("DuoLink Host").unwrap();
        let mut stream = HostStream::start(
            &exe,
            data.path(),
            address,
            &identity,
            Uuid::new_v4(),
            DEFAULT_FEC,
            None,
        )
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(20);
        while !stream.ready(address).unwrap() {
            assert!(Instant::now() < deadline, "Sunshine did not start");
            thread::sleep(Duration::from_millis(100));
        }
        let (first, first_name) = stream.pairing.add(&identity).unwrap_or_else(|error| {
            let log = fs::read_to_string(data.path().join("logs/host.log")).unwrap_or_default();
            let tail = log.lines().rev().take(15).collect::<Vec<_>>();
            panic!("{error:#}; log tail: {tail:?}");
        });
        let (second, second_name) = stream.pairing.add(&identity).unwrap_or_else(|error| {
            let log = fs::read_to_string(data.path().join("logs/host.log")).unwrap_or_default();
            let tail = log.lines().rev().take(15).collect::<Vec<_>>();
            panic!("{error:#}; log tail: {tail:?}");
        });
        assert_ne!(first.certificate, second.certificate);
        assert_ne!(first.key, second.key);
        let clients =
            || stream.pairing.admin("/api/clients/list", None).unwrap()["named_certs"].clone();
        let names = |entries: &Value, name: &str| {
            entries
                .as_array()
                .unwrap()
                .iter()
                .any(|entry| entry["name"] == name)
        };
        assert!(names(&clients(), &first_name));
        assert!(names(&clients(), &second_name));
        stream.pairing.remove(&first_name).unwrap();
        assert!(!names(&clients(), &first_name));
        assert!(names(&clients(), &second_name));
        stream.pairing.remove(&second_name).unwrap();
        assert!(clients().as_array().unwrap().is_empty());
    }
}
