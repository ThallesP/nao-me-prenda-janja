use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

pub const SESSION_TTL_SECS: u64 = 24 * 60 * 60;

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// Authenticated inside the activity iframe — can watch streams.
    Member,
    /// Authenticated on the external share page — can also publish.
    Publisher,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Claims {
    /// Discord user id.
    pub sub: String,
    pub name: String,
    pub avatar: Option<String>,
    /// Activity instance id — doubles as the room key.
    pub inst: String,
    pub chan: Option<String>,
    pub guild: Option<String>,
    pub role: Role,
    /// Unix seconds.
    pub exp: u64,
}

pub fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock before unix epoch")
        .as_secs()
}

pub fn mint(secret: &str, claims: &Claims) -> String {
    let payload = URL_SAFE_NO_PAD.encode(serde_json::to_vec(claims).expect("claims serialize"));
    let sig = sign(secret, payload.as_bytes());
    format!("{payload}.{sig}")
}

pub fn verify(secret: &str, token: &str) -> Option<Claims> {
    let (payload, sig) = token.split_once('.')?;
    let mut mac = mac(secret);
    mac.update(payload.as_bytes());
    let expected = URL_SAFE_NO_PAD.decode(sig).ok()?;
    mac.verify_slice(&expected).ok()?;
    let claims: Claims = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(payload).ok()?).ok()?;
    if claims.exp < now_secs() {
        return None;
    }
    Some(claims)
}

fn mac(secret: &str) -> HmacSha256 {
    HmacSha256::new_from_slice(secret.as_bytes()).expect("hmac accepts any key length")
}

fn sign(secret: &str, data: &[u8]) -> String {
    let mut mac = mac(secret);
    mac.update(data);
    URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes())
}

pub fn random_id(bytes: usize) -> String {
    use rand::RngCore;
    let mut buf = vec![0u8; bytes];
    rand::rng().fill_bytes(&mut buf);
    crate::config::hex(&buf)
}
