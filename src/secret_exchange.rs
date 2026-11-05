//! The `sx-aes-1` secret exchange that the GNOME system prompter uses to
//! hand over a typed passphrase, compatible with gcr's `GcrSecretExchange`
//! (protocol 1):
//!
//! * Diffie-Hellman in the 1536-bit MODP group of RFC 3526 (generator 2);
//! * the shared secret, left-padded to 192 bytes, goes through HKDF-SHA256
//!   (32 zero bytes of salt, no info) and the first 16 bytes are the
//!   AES-128 key;
//! * secrets are AES-128-CBC encrypted with a fresh random IV and PKCS#7
//!   padding;
//! * messages are key files: `[sx-aes-1]`, `public=<base64>`, and with a
//!   secret also `secret=<base64>` and `iv=<base64>`.
//!
//! The prompter starts the exchange, so the daemon only receives and
//! sends. Starting one is needed by the tests, which play the prompter.

use std::collections::HashMap;
use std::fmt;
use std::sync::OnceLock;

use aes::Aes128;
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine as _;
use cbc::cipher::{block_padding::Pkcs7, BlockDecryptMut, BlockEncryptMut, KeyIvInit};
use hmac::{Hmac, Mac};
use num_bigint::BigUint;
use sha2::Sha256;

/// Key file group of the protocol.
const PROTOCOL: &str = "sx-aes-1";

/// The RFC 3526 1536-bit MODP prime, the group gcr uses.
const PRIME_HEX: &str = concat!(
    "FFFFFFFFFFFFFFFFC90FDAA22168C234C4C6628B80DC1CD1",
    "29024E088A67CC74020BBEA63B139B22514A08798E3404DD",
    "EF9519B3CD3A431B302B0A6DF25F14374FE1356D6D51C245",
    "E485B576625E7EC6F44C42E9A637ED6B0BFF5CB6F406B7ED",
    "EE386BFB5A899FA5AE9F24117C4B1FE649286651ECE45B3D",
    "C2007CB8A163BF0598DA48361C55D39A69163FA8FD24CF5F",
    "83655D23DCA3AD961C62F356208552BB9ED529077096966D",
    "670C354E4ABC9804F1746C08CA237327FFFFFFFFFFFFFFFF",
);
/// Length of the prime in bytes, which the shared secret is padded to.
const PRIME_LEN: usize = 192;
const GENERATOR: u8 = 2;
const KEY_LEN: usize = 16;
const IV_LEN: usize = 16;
const BLOCK_LEN: usize = 16;

fn prime() -> &'static BigUint {
    static PRIME: OnceLock<BigUint> = OnceLock::new();
    PRIME.get_or_init(|| BigUint::parse_bytes(PRIME_HEX.as_bytes(), 16).expect("valid hex"))
}

/// Why a message could not be used. Nothing the peer sends makes these
/// functions panic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExchangeError {
    /// No `public` key, bad base64, a public value outside `1 < y < p`, or
    /// a secret without a valid IV.
    Malformed,
    /// The secret did not decrypt (wrong key or bad padding).
    Decrypt,
    /// [`SecretExchange::send`] before any peer key was received.
    NotReady,
    /// `/dev/urandom` could not be read.
    Randomness,
}

impl fmt::Display for ExchangeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            ExchangeError::Malformed => "malformed secret exchange message",
            ExchangeError::Decrypt => "the secret could not be decrypted",
            ExchangeError::NotReady => "no key agreed yet",
            ExchangeError::Randomness => "cannot read /dev/urandom",
        })
    }
}

impl std::error::Error for ExchangeError {}

fn random_bytes<const N: usize>() -> Result<[u8; N], ExchangeError> {
    use std::io::Read;
    let mut buf = [0u8; N];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut file| file.read_exact(&mut buf))
        .map_err(|_| ExchangeError::Randomness)?;
    Ok(buf)
}

/// The public value `g^x mod p` as minimal big-endian bytes.
fn public_value(private: &BigUint) -> Vec<u8> {
    BigUint::from(GENERATOR)
        .modpow(private, prime())
        .to_bytes_be()
}

/// A random 256-bit private exponent with its top and bottom bits set.
fn generate_private() -> Result<BigUint, ExchangeError> {
    let mut raw = random_bytes::<32>()?;
    raw[0] |= 0x80;
    raw[31] |= 0x01;
    Ok(BigUint::from_bytes_be(&raw))
}

/// HKDF-SHA256 (RFC 5869) with 32 zero bytes of salt and no info, cut to
/// the AES key length. One expand block is enough for 16 bytes.
fn derive_key(shared: &[u8]) -> [u8; KEY_LEN] {
    let mut extract = Hmac::<Sha256>::new_from_slice(&[0u8; 32]).expect("any key length");
    extract.update(shared);
    let prk = extract.finalize().into_bytes();
    let mut expand = Hmac::<Sha256>::new_from_slice(&prk).expect("any key length");
    expand.update(&[0x01]);
    let okm = expand.finalize().into_bytes();
    let mut key = [0u8; KEY_LEN];
    key.copy_from_slice(&okm[..KEY_LEN]);
    key
}

/// AES-128-CBC with PKCS#7 padding.
fn encrypt(key: &[u8; KEY_LEN], iv: &[u8; IV_LEN], secret: &[u8]) -> Vec<u8> {
    let mut buf = vec![0u8; (secret.len() / BLOCK_LEN + 1) * BLOCK_LEN];
    buf[..secret.len()].copy_from_slice(secret);
    let len = cbc::Encryptor::<Aes128>::new(key.into(), iv.into())
        .encrypt_padded_mut::<Pkcs7>(&mut buf, secret.len())
        .expect("the buffer has room for the padding")
        .len();
    buf.truncate(len);
    buf
}

fn decode(text: &str) -> Result<Vec<u8>, ExchangeError> {
    B64.decode(text).map_err(|_| ExchangeError::Malformed)
}

/// The `key=value` pairs of the `[sx-aes-1]` group of a key file. Comments,
/// blank lines and other groups are skipped; a later duplicate wins.
fn parse(text: &str) -> HashMap<&str, &str> {
    let mut in_group = false;
    let mut values = HashMap::new();
    for line in text.lines().map(str::trim) {
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if let Some(group) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            in_group = group == PROTOCOL;
        } else if in_group {
            if let Some((key, value)) = line.split_once('=') {
                values.insert(key.trim(), value.trim());
            }
        }
    }
    values
}

/// One side of a secret exchange.
///
/// The prompter starts with [`begin`](Self::begin); the daemon
/// [`receive`](Self::receive)s that message, which agrees on a key, and
/// answers with [`send`](Self::send). From then on either side can send a
/// secret that the other receives.
#[derive(Debug, Default)]
pub struct SecretExchange {
    private: Option<BigUint>,
    /// Set once a peer key has been received.
    key: Option<[u8; KEY_LEN]>,
}

impl SecretExchange {
    pub fn new() -> Self {
        Self::default()
    }

    /// Start an exchange with a fresh key pair, forgetting any agreed key.
    #[cfg(test)]
    pub fn begin(&mut self) -> Result<String, ExchangeError> {
        let private = generate_private()?;
        let public = public_value(&private);
        self.private = Some(private);
        self.key = None;
        Ok(message(&public, None))
    }

    /// Take in a peer message: agree on a key with the peer's public value
    /// (generating our key pair first if needed), and decrypt the secret if
    /// the message carries one. Returns that secret.
    pub fn receive(&mut self, text: &str) -> Result<Option<Vec<u8>>, ExchangeError> {
        let values = parse(text);
        let peer = values.get("public").ok_or(ExchangeError::Malformed)?;
        let peer = BigUint::from_bytes_be(&decode(peer)?);
        if peer <= BigUint::from(1u8) || peer >= *prime() {
            return Err(ExchangeError::Malformed);
        }
        if self.private.is_none() {
            self.private = Some(generate_private()?);
        }
        let private = self.private.as_ref().expect("private key present");
        let shared = peer.modpow(private, prime()).to_bytes_be();
        let mut padded = vec![0u8; PRIME_LEN - shared.len()];
        padded.extend_from_slice(&shared);
        let key = derive_key(&padded);
        self.key = Some(key);

        let Some(secret) = values.get("secret") else {
            return Ok(None);
        };
        let iv = decode(values.get("iv").ok_or(ExchangeError::Malformed)?)?;
        let mut data = decode(secret)?;
        if iv.len() != IV_LEN || data.is_empty() || data.len() % BLOCK_LEN != 0 {
            return Err(ExchangeError::Malformed);
        }
        let plain = cbc::Decryptor::<Aes128>::new_from_slices(&key, &iv)
            .map_err(|_| ExchangeError::Malformed)?
            .decrypt_padded_mut::<Pkcs7>(&mut data)
            .map_err(|_| ExchangeError::Decrypt)?;
        Ok(Some(plain.to_vec()))
    }

    /// Our message: our public value and, given a secret, the secret
    /// encrypted under a fresh IV.
    pub fn send(&self, secret: Option<&[u8]>) -> Result<String, ExchangeError> {
        let iv = match secret {
            Some(_) => Some(random_bytes::<IV_LEN>()?),
            None => None,
        };
        self.send_with_iv(secret.zip(iv.as_ref()))
    }

    fn send_with_iv(
        &self,
        secret: Option<(&[u8], &[u8; IV_LEN])>,
    ) -> Result<String, ExchangeError> {
        let (Some(private), Some(key)) = (&self.private, &self.key) else {
            return Err(ExchangeError::NotReady);
        };
        let encrypted = secret.map(|(secret, iv)| (*iv, encrypt(key, iv, secret)));
        Ok(message(&public_value(private), encrypted.as_ref()))
    }
}

/// A message with the given public value and `(iv, encrypted secret)`.
fn message(public: &[u8], encrypted: Option<&([u8; IV_LEN], Vec<u8>)>) -> String {
    let mut out = format!("[{PROTOCOL}]\npublic={}\n", B64.encode(public));
    if let Some((iv, data)) = encrypted {
        out.push_str(&format!(
            "secret={}\niv={}\n",
            B64.encode(data),
            B64.encode(iv)
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // Known answers, computed with an independent implementation of the
    // protocol from these two private exponents.
    const PRIVATE_A: &str = "800123456789abcdef0123456789abcdef0123456789abcdef0123456789abcd";
    const PRIVATE_B: &str = "800fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543";
    const PUBLIC_A: &str = "TrlLUX+I9mYB4fhPRZpdEUbA2UkAPt7s1ftvin1nHa+JXWONSqZ1tZXnBv4oa/c/kQ2/lMchx/z7Yq4hAauoaTHMkyaEp8MwmjrvWVpKD4mwCpP1Iv/zGxK3gYx9CAiYNc3KcbO7HF63TFAXsg4dJr3yaOIE0ew9gh9uRg60EyLf7Jxho+npKe9cqoPGKi2uoJ/u8k80PlwizBLxuzkKnrb1smf+wE/WHNpEW6naMMY7DuboeRBUPXevwFWN9Z/0";
    const PUBLIC_B: &str = "FoyNvE8uHl5VJScshj4yIU9B5eoJR28xNCO4oc2+uD1aLVavk3aHR6ac9maHo3KDPLlxmKg+svS1dfaCN8nLlNdYYOdvN6C+/hwOP07JX5DycVtESoVG+6jbjgiU+rowxFdP/36Gxsupk+AYZZHYjvL4MX9torMjRQvQmwRrViZjNQ4cxpiwgX/rlkWjPlocpGrHGxHg6prfcBd7eVJsG+yKjZ5kkeQwjT/hjtX5ZTIclt2JJxBcss3FeCvf+7Yg";
    const KEY: [u8; 16] = [
        0xe6, 0x3e, 0x29, 0x85, 0x5b, 0x35, 0xac, 0x1d, 0x6e, 0xf3, 0x16, 0x78, 0x7d, 0x2f, 0x9d,
        0x3e,
    ];
    /// A's secrets for B, with IVs 00..0f, ff..ff and 0f..00.
    const SECRET_MESSAGES: [(&str, &str, &[u8]); 3] = [
        (
            "SfXP73qtuKeoUwz+Shx1OyzU0+qgmqrm8O2BdgGhGgQ=",
            "AAECAwQFBgcICQoLDA0ODw==",
            b"correct horse battery staple",
        ),
        ("fCCC9rJVTIuqIZe+FGZ9Ng==", "/////////////////////w==", b""),
        (
            "N5kLBf9sgAW/DB5Js92MTyRSM3JV2s2RsjL15PdHRYQ=",
            "Dw4NDAsKCQgHBgUEAwIBAA==",
            b"0123456789abcdef",
        ),
    ];

    fn with_private(hex: &str) -> SecretExchange {
        SecretExchange {
            private: Some(BigUint::parse_bytes(hex.as_bytes(), 16).unwrap()),
            key: None,
        }
    }

    fn public_message(public: &str) -> String {
        format!("[sx-aes-1]\npublic={public}\n")
    }

    fn secret_message(public: &str, secret: &str, iv: &str) -> String {
        format!("[sx-aes-1]\npublic={public}\nsecret={secret}\niv={iv}\n")
    }

    #[test]
    fn known_public_values_and_key() {
        let mut a = with_private(PRIVATE_A);
        let mut b = with_private(PRIVATE_B);
        assert_eq!(a.receive(&public_message(PUBLIC_B)), Ok(None));
        assert_eq!(b.receive(&public_message(PUBLIC_A)), Ok(None));
        assert_eq!(a.key, Some(KEY));
        assert_eq!(b.key, Some(KEY));
        assert_eq!(a.send(None).unwrap(), public_message(PUBLIC_A));
        assert_eq!(b.send(None).unwrap(), public_message(PUBLIC_B));
    }

    #[test]
    fn known_secrets_decrypt_and_encrypt() {
        let mut a = with_private(PRIVATE_A);
        a.receive(&public_message(PUBLIC_B)).unwrap();
        for (data, iv, plain) in SECRET_MESSAGES {
            let mut b = with_private(PRIVATE_B);
            b.receive(&public_message(PUBLIC_A)).unwrap();
            let text = secret_message(PUBLIC_A, data, iv);
            assert_eq!(b.receive(&text), Ok(Some(plain.to_vec())));
            let iv: [u8; IV_LEN] = decode(iv).unwrap().try_into().unwrap();
            assert_eq!(a.send_with_iv(Some((plain, &iv))).unwrap(), text);
        }
    }

    #[test]
    fn random_exchanges_round_trip_any_bytes() {
        let mut prompter = SecretExchange::new();
        let mut daemon = SecretExchange::new();
        let begin = prompter.begin().unwrap();
        assert_eq!(daemon.receive(&begin), Ok(None));
        assert_eq!(prompter.receive(&daemon.send(None).unwrap()), Ok(None));
        let secret: Vec<u8> = (0..=255u8).collect();
        let text = prompter.send(Some(&secret)).unwrap();
        assert_eq!(daemon.receive(&text), Ok(Some(secret)));
        let text = daemon.send(Some("pässwörd 🔑".as_bytes())).unwrap();
        assert_eq!(
            prompter.receive(&text),
            Ok(Some("pässwörd 🔑".as_bytes().to_vec()))
        );
        // A new exchange has a new key pair.
        assert_ne!(prompter.begin().unwrap(), begin);
    }

    #[test]
    fn send_needs_a_received_key() {
        let mut exchange = SecretExchange::new();
        assert_eq!(exchange.send(None), Err(ExchangeError::NotReady));
        exchange.begin().unwrap();
        assert_eq!(exchange.send(Some(b"x")), Err(ExchangeError::NotReady));
    }

    #[test]
    fn reads_only_its_own_group() {
        let text = format!(
            "# comment\n; comment\n\n[other]\npublic=AAAA\n\n[sx-aes-1]\n  public = {PUBLIC_B}  \n\n[trailing]\nsecret=zzz\n"
        );
        let mut a = with_private(PRIVATE_A);
        assert_eq!(a.receive(&text), Ok(None));
        assert_eq!(a.key, Some(KEY));
    }

    #[test]
    fn rejects_malformed_messages() {
        let prime_b64 = B64.encode(prime().to_bytes_be());
        let above_b64 = B64.encode((prime() + BigUint::from(1u8)).to_bytes_be());
        let (data, iv, _) = SECRET_MESSAGES[0];
        let cases = [
            ("empty", String::new()),
            ("no group", "hello world".to_string()),
            ("bad base64", public_message("!!!not-base64!!!")),
            ("zero", public_message("AA==")),
            ("one", public_message("AQ==")),
            ("p", public_message(&prime_b64)),
            ("above p", public_message(&above_b64)),
            ("no public", "[sx-aes-1]\nfoo=bar\n".to_string()),
            (
                "secret without iv",
                format!("{}secret={data}\n", public_message(PUBLIC_A)),
            ),
            ("short iv", secret_message(PUBLIC_A, data, "c2hvcnQ=")),
            ("bad iv", secret_message(PUBLIC_A, data, "!!!")),
            (
                "truncated secret",
                secret_message(PUBLIC_A, &data.replace("GgQ=", "Gg=="), iv),
            ),
            ("empty secret", secret_message(PUBLIC_A, "", iv)),
        ];
        for (name, text) in cases {
            let mut b = with_private(PRIVATE_B);
            assert_eq!(
                b.receive(&text),
                Err(ExchangeError::Malformed),
                "case {name}"
            );
        }
        // A flipped bit in the last block breaks the padding.
        let mut flipped = decode(data).unwrap();
        *flipped.last_mut().unwrap() ^= 0x01;
        let mut b = with_private(PRIVATE_B);
        assert_eq!(
            b.receive(&secret_message(PUBLIC_A, &B64.encode(flipped), iv)),
            Err(ExchangeError::Decrypt)
        );
    }
}
