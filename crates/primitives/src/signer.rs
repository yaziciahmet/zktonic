use crate::crypto::{sha256, sign_prehash, SecretKey, ToPublicKey};
use crate::{Address, Signature};

#[derive(Debug, Clone)]
pub struct Signer {
    secret_key: SecretKey,
    address: Address,
}

impl Signer {
    fn from_secret_key(secret_key: SecretKey) -> Self {
        let address = Address::from(secret_key.to_public_key());
        Self {
            secret_key,
            address,
        }
    }

    pub fn new(secret: [u8; 32]) -> Self {
        let secret_key = SecretKey::from_byte_array(&secret)
            .expect("Signer should not be instantiated with invalid private key");
        Self::from_secret_key(secret_key)
    }

    pub fn from_bytes(secret: impl AsRef<[u8]>) -> anyhow::Result<Self> {
        Ok(Self::new(secret.as_ref().try_into()?))
    }

    pub fn from_str(secret: &str) -> anyhow::Result<Self> {
        if !secret.starts_with("0x") {
            return Err(anyhow::anyhow!("Signed secret key must have 0x-prefix"));
        }

        Self::from_bytes(hex::decode(&secret[2..])?)
    }

    pub fn sign(&self, message: &[u8]) -> Signature {
        let prehash = sha256(message);
        self.sign_prehash(prehash)
    }

    pub fn sign_prehash(&self, prehash: [u8; 32]) -> Signature {
        let (signature, recid) = sign_prehash(&self.secret_key, prehash);
        Signature::from_parts(signature, recid).expect("Signing can not produce invalid signature")
    }

    #[inline(always)]
    pub fn address(&self) -> Address {
        self.address
    }

    #[cfg(feature = "test-helpers")]
    pub fn random() -> Self {
        Self::from_secret_key(crate::crypto::generate_keypair().secret_key())
    }
}

#[cfg(test)]
mod tests {
    use super::Signer;

    #[test]
    fn sign_and_verify() {
        let signer = Signer::random();
        let message = &[1, 2, 3];

        let signature = signer.sign(message);
        assert_eq!(signature.recover(message).unwrap(), signer.address());
    }
}
