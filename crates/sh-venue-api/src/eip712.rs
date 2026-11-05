//! EIP-712 для подписей Hyperliquid и Aster: разделитель домена, хеш структуры,
//! итоговый дайджест и подписант на `secp256k1`. Структуры простые (строки, bytes32),
//! поэтому кодирование написано вручную и закреплено тест-векторами SDK площадок.

use alloy::primitives::{Address, B256, U256, keccak256};
use alloy::signers::SignerSync;
use alloy::signers::local::PrivateKeySigner;
use secrecy::{ExposeSecret, SecretString};

/// Поле структуры EIP-712.
#[derive(Debug, Clone)]
pub enum Field<'a> {
    /// `string`: кодируется как keccak256 содержимого.
    Str(&'a str),
    /// `bytes32`.
    B32(B256),
    /// `uint256`.
    U(U256),
    /// `address`.
    Addr(Address),
    /// `uint64` и прочие целые до 256 бит.
    U64(u64),
}

fn word(f: &Field<'_>) -> B256 {
    match f {
        Field::Str(s) => keccak256(s.as_bytes()),
        Field::B32(b) => *b,
        Field::U(u) => B256::from(*u),
        Field::Addr(a) => a.into_word(),
        Field::U64(v) => B256::from(U256::from(*v)),
    }
}

/// `hashStruct = keccak256(typeHash ‖ enc(field_1) ‖ …)`.
pub fn hash_struct(type_sig: &str, fields: &[Field<'_>]) -> B256 {
    let mut data = Vec::with_capacity(32 * (fields.len() + 1));
    data.extend_from_slice(keccak256(type_sig.as_bytes()).as_slice());
    for f in fields {
        data.extend_from_slice(word(f).as_slice());
    }
    keccak256(&data)
}

/// Разделитель домена `EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)`.
pub fn domain_separator(name: &str, version: &str, chain_id: u64, verifying_contract: Address) -> B256 {
    hash_struct(
        "EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)",
        &[Field::Str(name), Field::Str(version), Field::U64(chain_id), Field::Addr(verifying_contract)],
    )
}

/// Итоговый дайджест `keccak256(0x1901 ‖ domain ‖ struct)`.
pub fn digest(domain: B256, struct_hash: B256) -> B256 {
    let mut data = Vec::with_capacity(66);
    data.extend_from_slice(&[0x19, 0x01]);
    data.extend_from_slice(domain.as_slice());
    data.extend_from_slice(struct_hash.as_slice());
    keccak256(&data)
}

/// Подпись `(r, s, v)`, `v` = 27 или 28.
#[derive(Clone, PartialEq, Eq)]
pub struct RawSig {
    /// r.
    pub r: U256,
    /// s.
    pub s: U256,
    /// v.
    pub v: u8,
}

impl std::fmt::Debug for RawSig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("RawSig(..)")
    }
}

impl RawSig {
    /// 65 байт `r ‖ s ‖ v` в hex с префиксом `0x`.
    pub fn to_hex65(&self) -> String {
        let mut b = Vec::with_capacity(65);
        b.extend_from_slice(&self.r.to_be_bytes::<32>());
        b.extend_from_slice(&self.s.to_be_bytes::<32>());
        b.push(self.v);
        let mut s = String::with_capacity(132);
        s.push_str("0x");
        for x in b {
            s.push_str(&format!("{x:02x}"));
        }
        s
    }
}

/// Ошибка ключа или подписи; без значения ключа.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum KeyError {
    /// Ключ не разобран.
    #[error("invalid private key format")]
    BadKey,
    /// Подпись не удалась.
    #[error("signing failed")]
    Sign,
}

/// Подписант EVM. Ключ не выводится ни в Debug, ни в ошибки.
pub struct EvmSigner {
    inner: PrivateKeySigner,
}

impl std::fmt::Debug for EvmSigner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "EvmSigner {{ address: {}, key: [redacted] }}", self.inner.address())
    }
}

impl EvmSigner {
    /// Из hex-ключа (`0x…` или без префикса).
    pub fn new(key: &SecretString) -> Result<Self, KeyError> {
        let inner: PrivateKeySigner = key.expose_secret().trim().parse().map_err(|_| KeyError::BadKey)?;
        Ok(Self { inner })
    }

    /// Адрес.
    pub fn address(&self) -> Address {
        self.inner.address()
    }

    /// Подписывает готовый дайджест.
    pub fn sign_digest(&self, d: B256) -> Result<RawSig, KeyError> {
        let sig = self.inner.sign_hash_sync(&d).map_err(|_| KeyError::Sign)?;
        Ok(RawSig { r: sig.r(), s: sig.s(), v: 27 + u8::from(sig.v()) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn domain_separator_known_value() {
        // Пример Mail из EIP-712: домен «Ether Mail», версия 1, chainId 1,
        // verifyingContract 0xCcCCccccCCCCcCCCCCCcCcCccCcCCCcCcccccccC.
        let d = domain_separator("Ether Mail", "1", 1, "0xCcCCccccCCCCcCCCCCCcCcCccCcCCCcCcccccccC".parse().unwrap());
        assert_eq!(format!("{d:#x}"), "0xf2cee375fa42b42143804025fc449deafd50cc031ca257e0b194a650a912090f");
    }

    #[test]
    fn debug_redacts() {
        let s =
            EvmSigner::new(&SecretString::from("0x0123456789012345678901234567890123456789012345678901234567890123"))
                .unwrap();
        assert!(format!("{s:?}").contains("[redacted]"));
        assert_eq!(
            EvmSigner::new(&SecretString::from("nonsense")).unwrap_err().to_string(),
            "invalid private key format"
        );
    }
}
