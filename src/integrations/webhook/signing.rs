use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;

pub(super) fn sign_alert(secret: &str, timestamp: &str, body: &[u8]) -> String {
    let mac = mac(secret, b'.', &[timestamp.as_bytes(), body]);
    hex::encode(mac.finalize().into_bytes())
}

pub(super) fn verify_request(
    secret: &str,
    timestamp: &str,
    method: &str,
    path: &str,
    body: &[u8],
    signature: &[u8],
) -> bool {
    mac(
        secret,
        b'\n',
        &[
            timestamp.as_bytes(),
            method.as_bytes(),
            path.as_bytes(),
            body,
        ],
    )
    .verify_slice(signature)
    .is_ok()
}

fn mac(secret: &str, separator: u8, parts: &[&[u8]]) -> Hmac<Sha256> {
    let mut mac =
        Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("HMAC accepts keys of any length");
    for (index, part) in parts.iter().enumerate() {
        if index > 0 {
            mac.update(&[separator]);
        }
        mac.update(part);
    }
    mac
}
