//! Подпись L1-действий Hyperliquid агент-кошельком.
//!
//! Схема (официальный Python SDK, `utils/signing.py`, и раздел «Signing» документации):
//! 1. `action_hash = keccak256(msgpack(action) ‖ nonce (8 байт BE) ‖ vault-флаг [‖ vault] [‖ 0x00 ‖ expires_after])`;
//! 2. «фантомный агент» `Agent { source: "a" (mainnet) | "b" (testnet), connectionId: action_hash }`;
//! 3. EIP-712 с доменом `Exchange`, версия `1`, chainId 1337, нулевой verifyingContract.
//!
//! msgpack кодируется вручную: порядок ключей словаря значим и совпадает с порядком
//! вставки в Python-словарь SDK. Корректность закреплена тест-векторами SDK.

use alloy::primitives::{Address, B256, keccak256};
use secrecy::SecretString;
use sh_venue_api::eip712::{EvmSigner, Field, KeyError, digest, domain_separator, hash_struct};

/// Значение для msgpack с упорядоченными словарями.
#[derive(Debug, Clone, PartialEq)]
pub enum Mp {
    /// `nil`.
    Nil,
    /// Логическое значение.
    Bool(bool),
    /// Неотрицательное целое.
    UInt(u64),
    /// Строка.
    Str(String),
    /// Массив.
    Arr(Vec<Mp>),
    /// Словарь в порядке вставки.
    Map(Vec<(String, Mp)>),
}

impl Mp {
    /// Строковое значение.
    pub fn s(v: impl Into<String>) -> Self {
        Mp::Str(v.into())
    }

    /// Словарь из пар.
    pub fn map<const N: usize>(items: [(&str, Mp); N]) -> Self {
        Mp::Map(items.into_iter().map(|(k, v)| (k.to_owned(), v)).collect())
    }

    /// Кодирование msgpack так же, как `msgpack.packb` в Python (минимальные форматы).
    pub fn encode(&self, out: &mut Vec<u8>) {
        match self {
            Mp::Nil => out.push(0xc0),
            Mp::Bool(b) => out.push(if *b { 0xc3 } else { 0xc2 }),
            Mp::UInt(v) => {
                let v = *v;
                if v < 0x80 {
                    out.push(v as u8);
                } else if let Ok(x) = u8::try_from(v) {
                    out.extend([0xcc, x]);
                } else if let Ok(x) = u16::try_from(v) {
                    out.push(0xcd);
                    out.extend(x.to_be_bytes());
                } else if let Ok(x) = u32::try_from(v) {
                    out.push(0xce);
                    out.extend(x.to_be_bytes());
                } else {
                    out.push(0xcf);
                    out.extend(v.to_be_bytes());
                }
            }
            Mp::Str(s) => {
                let b = s.as_bytes();
                let n = b.len();
                if n < 32 {
                    out.push(0xa0 | n as u8);
                } else if let Ok(x) = u8::try_from(n) {
                    out.extend([0xd9, x]);
                } else if let Ok(x) = u16::try_from(n) {
                    out.push(0xda);
                    out.extend(x.to_be_bytes());
                } else {
                    out.push(0xdb);
                    out.extend(u32::try_from(n).unwrap_or(u32::MAX).to_be_bytes());
                }
                out.extend_from_slice(b);
            }
            Mp::Arr(items) => {
                Self::len_header(out, items.len(), 0x90, 0xdc, 0xdd);
                for it in items {
                    it.encode(out);
                }
            }
            Mp::Map(items) => {
                Self::len_header(out, items.len(), 0x80, 0xde, 0xdf);
                for (k, v) in items {
                    Mp::Str(k.clone()).encode(out);
                    v.encode(out);
                }
            }
        }
    }

    fn len_header(out: &mut Vec<u8>, n: usize, fix: u8, m16: u8, m32: u8) {
        if n < 16 {
            out.push(fix | n as u8);
        } else if let Ok(x) = u16::try_from(n) {
            out.push(m16);
            out.extend(x.to_be_bytes());
        } else {
            out.push(m32);
            out.extend(u32::try_from(n).unwrap_or(u32::MAX).to_be_bytes());
        }
    }

    /// JSON-представление для тела запроса `/exchange` (тот же порядок ключей).
    pub fn to_json(&self) -> serde_json::Value {
        match self {
            Mp::Nil => serde_json::Value::Null,
            Mp::Bool(b) => serde_json::Value::Bool(*b),
            Mp::UInt(v) => serde_json::Value::from(*v),
            Mp::Str(s) => serde_json::Value::String(s.clone()),
            Mp::Arr(a) => serde_json::Value::Array(a.iter().map(Mp::to_json).collect()),
            Mp::Map(m) => serde_json::Value::Object(m.iter().map(|(k, v)| (k.clone(), v.to_json())).collect()),
        }
    }
}

/// Хеш действия.
pub fn action_hash(action: &Mp, vault: Option<Address>, nonce: u64, expires_after: Option<u64>) -> B256 {
    let mut data = Vec::with_capacity(256);
    action.encode(&mut data);
    data.extend(nonce.to_be_bytes());
    match vault {
        None => data.push(0x00),
        Some(a) => {
            data.push(0x01);
            data.extend_from_slice(a.as_slice());
        }
    }
    if let Some(e) = expires_after {
        data.push(0x00);
        data.extend(e.to_be_bytes());
    }
    keccak256(&data)
}

/// Подпись в формате тела запроса.
#[derive(Clone, PartialEq, Eq)]
pub struct HlSignature {
    /// `r` в hex без ведущих нулей, как в SDK.
    pub r: String,
    /// `s` в hex без ведущих нулей.
    pub s: String,
    /// 27 или 28.
    pub v: u8,
}

impl std::fmt::Debug for HlSignature {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // подпись не секрет, но в журналы она не нужна
        f.write_str("HlSignature(..)")
    }
}

impl HlSignature {
    /// JSON `{"r","s","v"}`.
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({"r": self.r, "s": self.s, "v": self.v})
    }
}

/// Подписант L1-действий (агент-кошелёк). Ключ не выводится ни в Debug, ни в ошибки.
#[derive(Debug)]
pub struct L1Signer {
    signer: EvmSigner,
    mainnet: bool,
}

impl L1Signer {
    /// Из hex-ключа (`0x…`).
    pub fn new(key: &SecretString, mainnet: bool) -> Result<Self, KeyError> {
        Ok(Self { signer: EvmSigner::new(key)?, mainnet })
    }

    /// Адрес агента.
    pub fn address(&self) -> Address {
        self.signer.address()
    }

    /// Mainnet или testnet.
    pub fn is_mainnet(&self) -> bool {
        self.mainnet
    }

    /// Подписывает L1-действие.
    pub fn sign_l1(
        &self,
        action: &Mp,
        vault: Option<Address>,
        nonce: u64,
        expires_after: Option<u64>,
    ) -> Result<HlSignature, KeyError> {
        let hash = action_hash(action, vault, nonce, expires_after);
        let source = if self.mainnet { "a" } else { "b" };
        let st = hash_struct("Agent(string source,bytes32 connectionId)", &[Field::Str(source), Field::B32(hash)]);
        let dom = domain_separator("Exchange", "1", 1337, Address::ZERO);
        let sig = self.signer.sign_digest(digest(dom, st))?;
        Ok(HlSignature { r: format!("{:#x}", sig.r), s: format!("{:#x}", sig.s), v: sig.v })
    }
}

/// Число в строку провода: без лишних нулей и экспоненты (`float_to_wire` SDK).
pub fn decimal_to_wire(v: rust_decimal::Decimal) -> String {
    let n = v.normalize();
    let s = n.to_string();
    if s == "-0" { "0".into() } else { s }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal::dec;

    const KEY: &str = "0x0123456789012345678901234567890123456789012345678901234567890123";

    fn signer(mainnet: bool) -> L1Signer {
        L1Signer::new(&SecretString::from(KEY), mainnet).unwrap()
    }

    fn order(cloid: Option<&str>) -> Mp {
        let mut o = vec![
            ("a".to_owned(), Mp::UInt(1)),
            ("b".to_owned(), Mp::Bool(true)),
            ("p".to_owned(), Mp::s("100")),
            ("s".to_owned(), Mp::s("100")),
            ("r".to_owned(), Mp::Bool(false)),
            ("t".to_owned(), Mp::map([("limit", Mp::map([("tif", Mp::s("Gtc"))]))])),
        ];
        if let Some(c) = cloid {
            o.push(("c".to_owned(), Mp::s(c)));
        }
        Mp::map([("type", Mp::s("order")), ("orders", Mp::Arr(vec![Mp::Map(o)])), ("grouping", Mp::s("na"))])
    }

    // Векторы — из hyperliquid-python-sdk, tests/signing_test.py.
    #[test]
    fn phantom_agent_matches_sdk() {
        let o = Mp::Map(vec![
            ("a".into(), Mp::UInt(4)),
            ("b".into(), Mp::Bool(true)),
            ("p".into(), Mp::s("1670.1")),
            ("s".into(), Mp::s("0.0147")),
            ("r".into(), Mp::Bool(false)),
            ("t".into(), Mp::map([("limit", Mp::map([("tif", Mp::s("Ioc"))]))])),
        ]);
        let action = Mp::map([("type", Mp::s("order")), ("orders", Mp::Arr(vec![o])), ("grouping", Mp::s("na"))]);
        let h = action_hash(&action, None, 1_677_777_606_040, None);
        assert_eq!(format!("{h:#x}"), "0x0fcbeda5ae3c4950a548021552a4fea2226858c4453571bf3f24ba017eac2908");
    }

    #[test]
    fn dummy_action_signature_matches_sdk() {
        let action = Mp::map([("type", Mp::s("dummy")), ("num", Mp::UInt(100_000_000_000))]);
        let m = signer(true).sign_l1(&action, None, 0, None).unwrap();
        assert_eq!(m.r, "0x53749d5b30552aeb2fca34b530185976545bb22d0b3ce6f62e31be961a59298");
        assert_eq!(m.s, "0x755c40ba9bf05223521753995abb2f73ab3229be8ec921f350cb447e384d8ed8");
        assert_eq!(m.v, 27);
        let t = signer(false).sign_l1(&action, None, 0, None).unwrap();
        assert_eq!(t.r, "0x542af61ef1f429707e3c76c5293c80d01f74ef853e34b76efffcb57e574f9510");
        assert_eq!(t.v, 28);
        let vault: Address = "0x1719884eb866cb12b2287399b15f7db5e7d775ea".parse().unwrap();
        let mv = signer(true).sign_l1(&action, Some(vault), 0, None).unwrap();
        assert_eq!(mv.r, "0x3c548db75e479f8012acf3000ca3a6b05606bc2ec0c29c50c515066a326239");
        assert_eq!(mv.v, 28);
    }

    #[test]
    fn order_signature_matches_sdk() {
        let m = signer(true).sign_l1(&order(None), None, 0, None).unwrap();
        assert_eq!(m.r, "0xd65369825a9df5d80099e513cce430311d7d26ddf477f5b3a33d2806b100d78e");
        assert_eq!(m.s, "0x2b54116ff64054968aa237c20ca9ff68000f977c93289157748a3162b6ea940e");
        assert_eq!(m.v, 28);
        let c = signer(false).sign_l1(&order(Some("0x00000000000000000000000000000001")), None, 0, None).unwrap();
        assert_eq!(c.r, "0xeba0664bed2676fc4e5a743bf89e5c7501aa6d870bdb9446e122c9466c5cd16d");
        assert_eq!(c.s, "0x7f3e74825c9114bc59086f1eebea2928c190fdfbfde144827cb02b85bbe90988");
        assert_eq!(c.v, 28);
    }

    #[test]
    fn debug_hides_key() {
        let s = format!("{:?}", signer(true));
        assert!(s.contains("[redacted]"));
        assert!(!s.contains("0123456789012345678901234567890123456789"));
    }

    #[test]
    fn wire_numbers() {
        assert_eq!(decimal_to_wire(dec!(100.000)), "100");
        assert_eq!(decimal_to_wire(dec!(0.01470)), "0.0147");
        assert_eq!(decimal_to_wire(dec!(1670.1)), "1670.1");
    }

    #[test]
    fn msgpack_lengths() {
        let mut out = Vec::new();
        Mp::s("x".repeat(40)).encode(&mut out);
        assert_eq!(&out[..2], &[0xd9, 40]);
        let mut out = Vec::new();
        Mp::UInt(70_000).encode(&mut out);
        assert_eq!(out[0], 0xce);
    }
}
