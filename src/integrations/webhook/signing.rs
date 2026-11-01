use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;

pub(super) fn sign(secret: &str, parts: &[&[u8]]) -> String {
    hex::encode(mac(secret, parts).finalize().into_bytes())
}

pub(super) fn verify(secret: &str, parts: &[&[u8]], signature: &[u8]) -> bool {
    mac(secret, parts).verify_slice(signature).is_ok()
}

fn mac(secret: &str, parts: &[&[u8]]) -> Hmac<Sha256> {
    let mut mac =
        Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("HMAC accepts keys of any length");
    for (index, part) in parts.iter().enumerate() {
        if index > 0 {
            mac.update(b".");
        }
        mac.update(part);
    }
    mac
}
