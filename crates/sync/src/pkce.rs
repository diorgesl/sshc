use anyhow::Result;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use sha2::{Digest, Sha256};

/// Verifier aleatório (43-128 chars, RFC 7636). 32 bytes -> 43 chars.
pub fn verifier() -> Result<String> {
    let mut buf = [0u8; 32];
    getrandom::getrandom(&mut buf)?;
    Ok(URL_SAFE_NO_PAD.encode(buf))
}

/// S256 challenge: BASE64URL(SHA256(ASCII(verifier))), sem padding.
pub fn challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

pub fn state() -> Result<String> {
    let mut buf = [0u8; 16];
    getrandom::getrandom(&mut buf)?;
    Ok(URL_SAFE_NO_PAD.encode(buf))
}

#[cfg(test)]
mod tests {
    use super::*;

    // Vetor do RFC 7636, Apêndice B.
    #[test]
    fn rfc7636_vector() {
        let v = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        assert_eq!(challenge(v), "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");
    }

    #[test]
    fn verifier_has_valid_shape() {
        let v = verifier().unwrap();
        assert!((43..=128).contains(&v.len()));
        assert!(v
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
        assert_ne!(v, verifier().unwrap());
    }
}
