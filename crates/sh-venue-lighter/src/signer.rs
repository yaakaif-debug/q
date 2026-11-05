//! FFI к официальному подписанту Lighter (`lighter-go`, собранному как C-библиотека
//! `signers/lighter-signer-<os>-<arch>.so|.dylib`). Единственный модуль с `unsafe`
//! в репозитории; каждый блок описывает, почему он корректен.
//!
//! Сигнатуры взяты из официального Python SDK (`lighter/signer_client.py`, `argtypes`
//! и `restype`); UNVERIFIED относительно конкретной сборки библиотеки — оператор
//! проверяет версию `.so` и прогоняет `selftest` на testnet.
//!
//! Строки, возвращаемые библиотекой, выделены в Go (`C.CString`) и освобождаются её
//! же функцией `Free`. Ключ передаётся только в `CreateClient` и сразу затирается в
//! нашей копии; в логи и ошибки он не попадает.

#![allow(unsafe_code)]

use std::ffi::{CStr, CString, c_char, c_int, c_longlong, c_void};
use std::sync::Mutex;

use libloading::Library;
use secrecy::{ExposeSecret, SecretString};

/// Ответ подписи транзакции (`SignedTxResponse` в SDK).
#[repr(C)]
struct SignedTxResponse {
    tx_type: u8,
    tx_info: *mut c_char,
    tx_hash: *mut c_char,
    message_to_sign: *mut c_char,
    err: *mut c_char,
}

/// Строка или ошибка (`StrOrErr` в SDK).
#[repr(C)]
struct StrOrErr {
    s: *mut c_char,
    err: *mut c_char,
}

type CreateClientFn = unsafe extern "C" fn(*const c_char, *const c_char, c_int, c_int, c_longlong) -> *mut c_char;
#[allow(clippy::type_complexity)]
type SignCreateOrderFn = unsafe extern "C" fn(
    c_longlong,
    c_longlong,
    c_longlong,
    c_int,
    c_int,
    c_int,
    c_int,
    c_int,
    c_int,
    c_longlong,
    c_longlong,
    c_int,
    c_int,
    u8,
    u8,
    u8,
    c_longlong,
    c_int,
    c_longlong,
) -> SignedTxResponse;
type SignCancelOrderFn =
    unsafe extern "C" fn(c_longlong, c_longlong, u8, c_longlong, c_int, c_longlong) -> SignedTxResponse;
type SignCancelAllFn =
    unsafe extern "C" fn(c_int, c_longlong, c_longlong, u8, c_longlong, c_int, c_longlong) -> SignedTxResponse;
type CreateAuthTokenFn = unsafe extern "C" fn(c_longlong, c_int, c_longlong) -> StrOrErr;
type FreeFn = unsafe extern "C" fn(*mut c_void);

/// Подписанная транзакция для `sendTx`.
#[derive(Clone, PartialEq, Eq)]
pub struct SignedTx {
    /// Тип транзакции.
    pub tx_type: u8,
    /// JSON транзакции.
    pub tx_info: String,
    /// Хеш.
    pub tx_hash: String,
}

impl std::fmt::Debug for SignedTx {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "SignedTx {{ tx_type: {}, tx_hash: {} }}", self.tx_type, self.tx_hash)
    }
}

/// Параметры ордера в целых единицах площадки.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OrderParams {
    /// Рынок.
    pub market_index: i64,
    /// Клиентский индекс (≤ 2^48 − 1).
    pub client_order_index: i64,
    /// Количество в единицах `10^-size_decimals`.
    pub base_amount: i64,
    /// Цена в единицах `10^-price_decimals`.
    pub price: i32,
    /// Продажа.
    pub is_ask: bool,
    /// Время жизни: 0 — IOC, 1 — GTT, 2 — post-only.
    pub time_in_force: i32,
    /// Только сокращение.
    pub reduce_only: bool,
    /// Срок: 0 для IOC, −1 — 28 дней.
    pub order_expiry: i64,
    /// Nonce API-ключа.
    pub nonce: i64,
}

/// Время жизни ордера Lighter.
pub mod tif {
    /// Немедленно или отмена.
    pub const IOC: i32 = 0;
    /// До времени.
    pub const GTT: i32 = 1;
    /// Только мейкер.
    pub const POST_ONLY: i32 = 2;
}

/// Ошибка подписанта; без ключа и без указателей.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SignerError {
    /// Библиотека не загружена или нет символа.
    #[error("signer library: {0}")]
    Library(String),
    /// Ошибка, возвращённая подписантом.
    #[error("signer: {0}")]
    Sign(String),
}

/// Загруженный подписант. Вызовы сериализуются мьютексом: библиотека хранит клиента
/// в глобальном состоянии Go.
pub struct LighterSigner {
    _lib: Library,
    create_order: SignCreateOrderFn,
    cancel_order: SignCancelOrderFn,
    cancel_all: SignCancelAllFn,
    auth_token: CreateAuthTokenFn,
    free: FreeFn,
    api_key_index: c_int,
    account_index: c_longlong,
    lock: Mutex<()>,
}

impl std::fmt::Debug for LighterSigner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "LighterSigner {{ account_index: {}, api_key_index: {}, key: [redacted] }}",
            self.account_index, self.api_key_index
        )
    }
}

impl LighterSigner {
    /// Загружает библиотеку и создаёт клиента подписанта.
    pub fn load(
        path: &std::path::Path,
        url: &str,
        key: &SecretString,
        chain_id: i32,
        api_key_index: u8,
        account_index: i64,
    ) -> Result<Self, SignerError> {
        // SAFETY: загрузка библиотеки исполняет её инициализаторы. Путь задаёт оператор
        // в конфиге (`venues.lighter.signer_lib`); это официальная сборка lighter-go,
        // и её целостность оператор проверяет при установке (docs/venues/lighter.md).
        let lib = unsafe { Library::new(path) }.map_err(|e| SignerError::Library(e.to_string()))?;
        // SAFETY: типы указателей на функции совпадают с `argtypes`/`restype` официального
        // SDK; символы живут, пока жива `lib`, которую структура хранит в `_lib`
        // и не выпускает раньше указателей.
        let (create_client, create_order, cancel_order, cancel_all, auth_token, free) = unsafe {
            (
                *lib.get::<CreateClientFn>(b"CreateClient\0").map_err(|e| SignerError::Library(e.to_string()))?,
                *lib.get::<SignCreateOrderFn>(b"SignCreateOrder\0").map_err(|e| SignerError::Library(e.to_string()))?,
                *lib.get::<SignCancelOrderFn>(b"SignCancelOrder\0").map_err(|e| SignerError::Library(e.to_string()))?,
                *lib.get::<SignCancelAllFn>(b"SignCancelAllOrders\0")
                    .map_err(|e| SignerError::Library(e.to_string()))?,
                *lib.get::<CreateAuthTokenFn>(b"CreateAuthToken\0").map_err(|e| SignerError::Library(e.to_string()))?,
                *lib.get::<FreeFn>(b"Free\0").map_err(|e| SignerError::Library(e.to_string()))?,
            )
        };
        let s = Self {
            _lib: lib,
            create_order,
            cancel_order,
            cancel_all,
            auth_token,
            free,
            api_key_index: c_int::from(api_key_index),
            account_index,
            lock: Mutex::new(()),
        };
        let c_url = CString::new(url).map_err(|_| SignerError::Sign("url contains NUL".into()))?;
        let raw = key.expose_secret().trim().trim_start_matches("0x");
        let mut key_bytes = raw.as_bytes().to_vec();
        let c_key = CString::new(key_bytes.clone()).map_err(|_| SignerError::Sign("key contains NUL".into()))?;
        // затираем промежуточную копию ключа
        key_bytes.iter_mut().for_each(|b| *b = 0);
        // SAFETY: обе строки — валидные C-строки, живут до конца вызова; библиотека
        // копирует их внутрь. Возвращённый указатель — либо NULL, либо строка ошибки,
        // выделенная в Go, которую освобождает `take_string`.
        let err = unsafe { create_client(c_url.as_ptr(), c_key.as_ptr(), chain_id, s.api_key_index, s.account_index) };
        let mut c_key = c_key.into_bytes();
        c_key.iter_mut().for_each(|b| *b = 0);
        if let Some(e) = s.take_string(err) {
            return Err(SignerError::Sign(e));
        }
        Ok(s)
    }

    /// Забирает строку Go и освобождает её.
    fn take_string(&self, p: *mut c_char) -> Option<String> {
        if p.is_null() {
            return None;
        }
        // SAFETY: `p` не NULL и указывает на NUL-терминированную строку, выделенную
        // библиотекой; читаем её один раз и сразу освобождаем функцией `Free` той же
        // библиотеки; повторного использования указателя нет.
        unsafe {
            let s = CStr::from_ptr(p).to_string_lossy().into_owned();
            (self.free)(p.cast::<c_void>());
            Some(s)
        }
    }

    fn take_tx(&self, r: SignedTxResponse) -> Result<SignedTx, SignerError> {
        let info = self.take_string(r.tx_info);
        let hash = self.take_string(r.tx_hash);
        let _ = self.take_string(r.message_to_sign);
        if let Some(e) = self.take_string(r.err) {
            return Err(SignerError::Sign(e));
        }
        Ok(SignedTx { tx_type: r.tx_type, tx_info: info.unwrap_or_default(), tx_hash: hash.unwrap_or_default() })
    }

    /// Подписывает создание ордера.
    pub fn sign_create_order(&self, p: &OrderParams) -> Result<SignedTx, SignerError> {
        let _g = self.lock.lock().map_err(|_| SignerError::Sign("signer lock poisoned".into()))?;
        // SAFETY: все аргументы — числа по значению в порядке `argtypes` SDK; функция не
        // сохраняет указателей от нас; результат разбирается `take_tx` и освобождается.
        let r = unsafe {
            (self.create_order)(
                p.market_index,
                p.client_order_index,
                p.base_amount,
                p.price,
                c_int::from(p.is_ask),
                0, // ORDER_TYPE_LIMIT
                p.time_in_force,
                c_int::from(p.reduce_only),
                0, // NIL_TRIGGER_PRICE
                p.order_expiry,
                0, // integrator_account_index
                0,
                0,
                0,
                0,
                0, // SKIP_NONCE_OFF
                p.nonce,
                self.api_key_index,
                self.account_index,
            )
        };
        self.take_tx(r)
    }

    /// Подписывает отмену ордера по индексу ордера.
    pub fn sign_cancel_order(&self, market_index: i64, order_index: i64, nonce: i64) -> Result<SignedTx, SignerError> {
        let _g = self.lock.lock().map_err(|_| SignerError::Sign("signer lock poisoned".into()))?;
        // SAFETY: аргументы по значению в порядке `argtypes`; результат освобождается в `take_tx`.
        let r =
            unsafe { (self.cancel_order)(market_index, order_index, 0, nonce, self.api_key_index, self.account_index) };
        self.take_tx(r)
    }

    /// Подписывает отмену всех ордеров (немедленно).
    pub fn sign_cancel_all(&self, nonce: i64) -> Result<SignedTx, SignerError> {
        let _g = self.lock.lock().map_err(|_| SignerError::Sign("signer lock poisoned".into()))?;
        // SAFETY: аргументы по значению; CANCEL_ALL_TIF_IMMEDIATE = 0, время 0,
        // NIL_MARKET_INDEX = 255 (все рынки, UNVERIFIED); результат освобождается в `take_tx`.
        let r = unsafe { (self.cancel_all)(0, 0, 255, 0, nonce, self.api_key_index, self.account_index) };
        self.take_tx(r)
    }

    /// Токен авторизации приватных запросов до `deadline_s` (Unix-секунды).
    pub fn auth_token(&self, deadline_s: i64) -> Result<SecretString, SignerError> {
        let _g = self.lock.lock().map_err(|_| SignerError::Sign("signer lock poisoned".into()))?;
        // SAFETY: аргументы по значению; обе строки результата освобождаются `take_string`.
        let r = unsafe { (self.auth_token)(deadline_s, self.api_key_index, self.account_index) };
        let tok = self.take_string(r.s);
        if let Some(e) = self.take_string(r.err) {
            return Err(SignerError::Sign(e));
        }
        tok.map(SecretString::from).ok_or_else(|| SignerError::Sign("empty auth token".into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_library_is_a_clean_error() {
        let e = LighterSigner::load(
            std::path::Path::new("/nonexistent/lighter-signer.so"),
            "https://testnet.zklighter.elliot.ai",
            &SecretString::from("00"),
            300,
            3,
            1,
        )
        .unwrap_err();
        assert!(matches!(e, SignerError::Library(_)));
        // ключ не попадает в текст ошибки
        assert!(!e.to_string().contains("00\""));
    }
}
