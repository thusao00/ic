use ic_crypto_ecdsa_secp256k1::PublicKey;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;

/// An Ethereum account address.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Address(#[serde(with = "crate::serde_data")] [u8; 20]);

impl Address {
    pub fn new(bytes: [u8; 20]) -> Self {
        Self(bytes)
    }

    pub fn from_pubkey(pubkey: &PublicKey) -> Self {
        let key_bytes = pubkey.serialize_sec1(/*compressed=*/ false);
        debug_assert_eq!(key_bytes[0], 0x04);
        let hash = keccak(&key_bytes[1..]);
        let mut addr = [0u8; 20];
        addr[..].copy_from_slice(&hash[12..32]);
        Self(addr)
    }
}

impl FromStr for Address {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if !s.starts_with("0x") {
            return Err("address doesn't start with '0x'".to_string());
        }
        let mut bytes = [0u8; 20];
        hex::decode_to_slice(&s[2..], &mut bytes)
            .map_err(|e| format!("address is not hex: {}", e))?;
        Ok(Self(bytes))
    }
}

impl fmt::Display for Address {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Display address using EIP-55
        // https://eips.ethereum.org/EIPS/eip-55
        let mut addr_chars = [0u8; 20 * 2];
        hex::encode_to_slice(self.0, &mut addr_chars)
            .expect("bug: failed to encode an address as hex");

        let checksum = keccak(&addr_chars[..]);
        let mut cs_nibbles = [0u8; 32 * 2];
        for i in 0..32 {
            cs_nibbles[2 * i] = checksum[i] >> 4;
            cs_nibbles[2 * i + 1] = checksum[i] & 0x0f;
        }
        write!(f, "0x")?;
        for (a, cs) in addr_chars.iter().zip(cs_nibbles.iter()) {
            let ascii_byte = if *cs >= 0x08 {
                a.to_ascii_uppercase()
            } else {
                *a
            };
            write!(f, "{}", char::from(ascii_byte))?;
        }
        Ok(())
    }
}

fn keccak(bytes: &[u8]) -> [u8; 32] {
    use tiny_keccak::Hasher;
    let mut hash = tiny_keccak::Keccak::v256();
    hash.update(bytes.as_ref());
    let mut output = [0u8; 32];
    hash.finalize(&mut output);
    output
}
