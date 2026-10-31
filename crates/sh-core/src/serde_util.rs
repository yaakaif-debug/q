//! Мягкий разбор необязательных чисел: площадки присылают `null`, пустую строку
//! (Lighter: `"mid_price": ""` при одностороннем стакане), строку или число.
//! Неразборчивое значение даёт `None`, а не ошибку всего сообщения.

use std::str::FromStr;

use serde::{Deserialize, Deserializer};

/// `Option<T>` из `null`, `""`, строки или числа; некорректное значение → `None`.
pub fn lenient_opt<'de, D, T>(d: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: FromStr,
{
    let v = Option::<serde_json::Value>::deserialize(d)?;
    Ok(match v {
        Some(serde_json::Value::String(s)) => {
            let s = s.trim();
            if s.is_empty() { None } else { s.parse().ok() }
        }
        Some(serde_json::Value::Number(n)) => n.to_string().parse().ok(),
        _ => None,
    })
}

/// То же для `rust_decimal::Decimal` с поддержкой экспоненциальной записи.
pub fn lenient_decimal<'de, D>(d: D) -> Result<Option<rust_decimal::Decimal>, D::Error>
where
    D: Deserializer<'de>,
{
    let v = Option::<serde_json::Value>::deserialize(d)?;
    let s = match v {
        Some(serde_json::Value::String(s)) => s,
        Some(serde_json::Value::Number(n)) => n.to_string(),
        _ => return Ok(None),
    };
    let s = s.trim();
    if s.is_empty() {
        return Ok(None);
    }
    Ok(rust_decimal::Decimal::from_str(s).or_else(|_| rust_decimal::Decimal::from_scientific(s)).ok())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::num::Px;
    use rust_decimal::dec;

    #[derive(Deserialize)]
    struct T {
        #[serde(default, deserialize_with = "lenient_opt")]
        px: Option<Px>,
        #[serde(default, deserialize_with = "lenient_decimal")]
        r: Option<rust_decimal::Decimal>,
    }

    fn parse(s: &str) -> T {
        serde_json::from_str(s).unwrap()
    }

    #[test]
    fn accepts_all_shapes() {
        assert_eq!(parse(r#"{"px":"1.5","r":"1e-5"}"#).px, Some(Px(dec!(1.5))));
        assert_eq!(parse(r#"{"px":"1.5","r":"1e-5"}"#).r, Some(dec!(0.00001)));
        assert_eq!(parse(r#"{"px":"","r":""}"#).px, None);
        assert_eq!(parse(r#"{"px":null,"r":null}"#).r, None);
        assert_eq!(parse(r#"{"px":2.25,"r":3}"#).px, Some(Px(dec!(2.25))));
        assert_eq!(parse(r#"{"px":"abc"}"#).px, None);
        assert_eq!(parse(r#"{}"#).px, None);
    }
}
