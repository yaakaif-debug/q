//! Секреты. На сервере хранятся только ключи, которые могут торговать, но не выводить
//! средства (SPEC.md, «Keys and secrets»). Один зашифрованный файл TOML расшифровывается
//! вне процесса: systemd `LoadCredentialEncrypted` (файл в `$CREDENTIALS_DIRECTORY`) или
//! внешняя команда (например, `age -d`, которая спросит пароль). В памяти значения лежат
//! в `SecretString` и обнуляются при освобождении; `Debug` печатает `[REDACTED]`.
//! Ошибки никогда не содержат содержимого файла.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use figment::Figment;
use figment::providers::{Format, Toml};
use secrecy::SecretString;
use serde::Deserialize;
use thiserror::Error;
use zeroize::Zeroizing;

/// Ошибка загрузки секретов. Тексты без значений секретов.
#[derive(Debug, Error)]
pub enum SecretsError {
    /// Неверная строка источника.
    #[error("bad secrets source '{0}': expected none, file:<path>, credential:<name> or command:<cmd>")]
    BadSource(String),
    /// Файл недоступен.
    #[error("cannot read secrets from {0}: {1}")]
    Read(String, String),
    /// Файл доступен группе или всем.
    #[error("secrets file {0} must not be readable by group or others (chmod 600)")]
    Permissions(String),
    /// Команда завершилась с ошибкой.
    #[error("secrets command failed with status {0}")]
    Command(String),
    /// Неверный формат (без содержимого).
    #[error("secrets file is not valid TOML for the expected schema: {0}")]
    Parse(String),
    /// `$CREDENTIALS_DIRECTORY` не задан.
    #[error("CREDENTIALS_DIRECTORY is not set; use systemd LoadCredentialEncrypted or another source")]
    NoCredentialsDir,
}

/// Источник секретов (`[secrets] source`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SecretsSource {
    /// Без секретов (запись, сканер, paper без приватных потоков).
    None,
    /// Незашифрованный файл с правами 600 (только для тестовых стендов).
    File(PathBuf),
    /// Учётные данные systemd: `$CREDENTIALS_DIRECTORY/<name>`.
    Credential(String),
    /// Команда, печатающая расшифрованный TOML в stdout (`age -d -i key.txt secrets.toml.age`).
    Command(String),
}

impl SecretsSource {
    /// Разбор строки `none`, `file:<path>`, `credential:<name>`, `command:<cmd>`.
    pub fn parse(s: &str) -> Result<Self, SecretsError> {
        let s = s.trim();
        if s == "none" {
            return Ok(Self::None);
        }
        let (kind, rest) = s.split_once(':').ok_or_else(|| SecretsError::BadSource(s.to_owned()))?;
        let rest = rest.trim();
        if rest.is_empty() {
            return Err(SecretsError::BadSource(s.to_owned()));
        }
        match kind {
            "file" => Ok(Self::File(PathBuf::from(rest))),
            "credential" => Ok(Self::Credential(rest.to_owned())),
            "command" => Ok(Self::Command(rest.to_owned())),
            _ => Err(SecretsError::BadSource(s.to_owned())),
        }
    }
}

/// Telegram-бот.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TelegramSecrets {
    /// Токен бота.
    pub bot_token: SecretString,
    /// Чат для оповещений.
    pub chat_id: String,
    /// Пользователи, которым разрешены команды (kill и др.).
    #[serde(default)]
    pub allowed_user_ids: Vec<i64>,
}

/// Hyperliquid: агент-кошелёк (API wallet) — торгует, вывести средства не может.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HyperliquidSecrets {
    /// Адрес основного аккаунта (или субаккаунта), на котором позиции.
    pub account_address: String,
    /// Приватный ключ агент-кошелька, одобренного основным аккаунтом.
    pub agent_private_key: SecretString,
}

/// Lighter: отдельный индекс API-ключа, ключ L1 на сервер не попадает.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LighterSecrets {
    /// Индекс аккаунта.
    pub account_index: i64,
    /// Индекс API-ключа.
    pub api_key_index: u8,
    /// Приватный ключ API (формат подписанта lighter-go).
    pub api_private_key: SecretString,
}

/// Aster: API-кошелёк (signer), авторизованный основным кошельком.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AsterSecrets {
    /// Адрес основного кошелька (`user`).
    pub user: String,
    /// Адрес API-кошелька (`signer`).
    pub signer: String,
    /// Приватный ключ API-кошелька.
    pub signer_private_key: SecretString,
}

/// Все секреты процесса.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Secrets {
    /// Telegram.
    pub telegram: Option<TelegramSecrets>,
    /// Hyperliquid.
    pub hyperliquid: Option<HyperliquidSecrets>,
    /// Lighter.
    pub lighter: Option<LighterSecrets>,
    /// Aster.
    pub aster: Option<AsterSecrets>,
}

impl Secrets {
    /// Разбор TOML из буфера (буфер обнуляется вызывающим).
    pub fn from_toml(text: &str) -> Result<Self, SecretsError> {
        // сообщение figment может процитировать значение — отдаём только путь ключа
        Figment::from(Toml::string(text)).extract().map_err(|e| {
            let path = e.path.join(".");
            SecretsError::Parse(if path.is_empty() {
                e.kind.to_string().chars().take(80).collect()
            } else {
                format!("at '{path}'")
            })
        })
    }

    /// Загружает секреты из источника.
    pub fn load(source: &SecretsSource) -> Result<Self, SecretsError> {
        match source {
            SecretsSource::None => Ok(Self::default()),
            SecretsSource::File(p) => {
                check_permissions(p)?;
                Self::from_toml(&read_zeroizing(p)?)
            }
            SecretsSource::Credential(name) => {
                let dir = std::env::var_os("CREDENTIALS_DIRECTORY").ok_or(SecretsError::NoCredentialsDir)?;
                Self::from_toml(&read_zeroizing(&Path::new(&dir).join(name))?)
            }
            SecretsSource::Command(cmd) => {
                let out = Command::new("sh")
                    .arg("-c")
                    .arg(cmd)
                    .stdin(Stdio::inherit())
                    .stderr(Stdio::inherit())
                    .stdout(Stdio::piped())
                    .output()
                    .map_err(|e| SecretsError::Read("command".into(), e.to_string()))?;
                let stdout = Zeroizing::new(out.stdout);
                if !out.status.success() {
                    return Err(SecretsError::Command(out.status.to_string()));
                }
                let text = Zeroizing::new(
                    String::from_utf8(stdout.to_vec()).map_err(|_| SecretsError::Parse("not UTF-8".into()))?,
                );
                Self::from_toml(&text)
            }
        }
    }
}

fn read_zeroizing(p: &Path) -> Result<Zeroizing<String>, SecretsError> {
    let mut f =
        std::fs::File::open(p).map_err(|e| SecretsError::Read(p.display().to_string(), e.kind().to_string()))?;
    let mut s = Zeroizing::new(String::new());
    f.read_to_string(&mut s).map_err(|e| SecretsError::Read(p.display().to_string(), e.kind().to_string()))?;
    Ok(s)
}

#[cfg(unix)]
fn check_permissions(p: &Path) -> Result<(), SecretsError> {
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(p)
        .map_err(|e| SecretsError::Read(p.display().to_string(), e.kind().to_string()))?
        .permissions()
        .mode();
    if mode & 0o077 != 0 {
        return Err(SecretsError::Permissions(p.display().to_string()));
    }
    Ok(())
}

#[cfg(not(unix))]
fn check_permissions(_p: &Path) -> Result<(), SecretsError> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use secrecy::ExposeSecret;

    const TOML: &str = r#"
[telegram]
bot_token = "123:SECRET-TOKEN"
chat_id = "-100"
allowed_user_ids = [42]

[hyperliquid]
account_address = "0xabc"
agent_private_key = "0xdeadbeefSECRET"
"#;

    #[test]
    fn parses_and_redacts() {
        let s = Secrets::from_toml(TOML).unwrap();
        let tg = s.telegram.as_ref().unwrap();
        assert_eq!(tg.bot_token.expose_secret(), "123:SECRET-TOKEN");
        let dbg = format!("{s:?}");
        assert!(!dbg.contains("SECRET"), "{dbg}");
        assert!(s.lighter.is_none());
    }

    #[test]
    fn parse_errors_do_not_leak_values() {
        let err =
            Secrets::from_toml("[hyperliquid]\naccount_address = 1\nagent_private_key = \"TOPSECRET\"\n").unwrap_err();
        assert!(!err.to_string().contains("TOPSECRET"), "{err}");
        let err = Secrets::from_toml("[unknown]\nkey = \"TOPSECRET\"\n").unwrap_err();
        assert!(!err.to_string().contains("TOPSECRET"), "{err}");
    }

    #[test]
    fn sources() {
        assert_eq!(SecretsSource::parse("none").unwrap(), SecretsSource::None);
        assert_eq!(SecretsSource::parse("credential:secrets").unwrap(), SecretsSource::Credential("secrets".into()));
        assert!(matches!(SecretsSource::parse("command:age -d x").unwrap(), SecretsSource::Command(_)));
        assert!(SecretsSource::parse("file:").is_err());
        assert!(SecretsSource::parse("vault:x").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn file_permissions_and_command() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("sh-secrets-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("s.toml");
        std::fs::write(&p, TOML).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(matches!(Secrets::load(&SecretsSource::File(p.clone())), Err(SecretsError::Permissions(_))));
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(Secrets::load(&SecretsSource::File(p.clone())).unwrap().hyperliquid.is_some());
        let cmd = format!("cat {}", p.display());
        assert!(Secrets::load(&SecretsSource::Command(cmd)).unwrap().telegram.is_some());
        assert!(matches!(Secrets::load(&SecretsSource::Command("exit 3".into())), Err(SecretsError::Command(_))));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
