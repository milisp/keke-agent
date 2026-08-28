use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rand::RngCore as _;
use sha2::Digest as _;
use sha2::Sha256;

/// A PKCE verifier and its S256 challenge (RFC 7636).
pub struct Pkce {
    pub verifier: String,
    pub challenge: String,
}

impl Pkce {
    #[must_use]
    pub fn generate() -> Self {
        let verifier = random_token(32);
        let digest = Sha256::digest(verifier.as_bytes());
        Self {
            challenge: URL_SAFE_NO_PAD.encode(digest),
            verifier,
        }
    }
}

/// `bytes` bytes of OS randomness, base64url encoded.
///
/// Used for the PKCE verifier and the `state` parameter, both of which are
/// unguessable-or-nothing: a predictable `state` is a working CSRF against the
/// loopback callback.
#[must_use]
pub fn random_token(bytes: usize) -> String {
    let mut buffer = vec![0u8; bytes];
    rand::rng().fill_bytes(&mut buffer);
    URL_SAFE_NO_PAD.encode(buffer)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_challenge_is_the_sha256_of_the_verifier() {
        let pkce = Pkce::generate();
        let expected = URL_SAFE_NO_PAD.encode(Sha256::digest(pkce.verifier.as_bytes()));
        assert_eq!(pkce.challenge, expected);
        assert_ne!(pkce.verifier, Pkce::generate().verifier);
    }
}
