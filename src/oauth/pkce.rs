use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use ring::digest::{digest, SHA256};

pub fn random_url_token(bytes: usize) -> String {
    let mut data = vec![0u8; bytes];
    getrandom::fill(&mut data).expect("secure randomness must be available for OAuth PKCE");
    URL_SAFE_NO_PAD.encode(data)
}

pub fn pkce_pair() -> (String, String) {
    let verifier = random_url_token(32);
    let challenge = URL_SAFE_NO_PAD.encode(digest(&SHA256, verifier.as_bytes()).as_ref());
    (verifier, challenge)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_challenge_is_s256_url_safe_no_pad() {
        let (verifier, challenge) = pkce_pair();
        assert!(verifier.len() >= 43);
        assert_eq!(challenge.len(), 43);
        assert!(!challenge.contains('='));
        assert!(challenge
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
    }
}
