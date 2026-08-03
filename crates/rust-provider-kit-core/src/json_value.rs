use std::collections::BTreeMap;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::ProviderCoreError;

#[derive(Debug, Clone, PartialEq)]
pub enum ProviderJsonValue {
    Null,
    Bool(bool),
    Number(f64),
    String(String),
    Array(Vec<Self>),
    Object(BTreeMap<String, Self>),
}

impl ProviderJsonValue {
    pub const MAXIMUM_DEPTH: usize = 48;
    pub const MAXIMUM_NODES: usize = 65_536;
    pub const MAXIMUM_ENCODED_BYTES: usize = 4 * 1_024 * 1_024;
    pub const MAXIMUM_STRING_UTF8_BYTES: usize = 1_024 * 1_024;
    pub const MAXIMUM_KEY_UTF8_BYTES: usize = 1_024;

    #[must_use]
    pub fn object(entries: impl IntoIterator<Item = (String, Self)>) -> Self {
        Self::Object(entries.into_iter().collect())
    }

    #[must_use]
    pub fn array(values: impl IntoIterator<Item = Self>) -> Self {
        Self::Array(values.into_iter().collect())
    }

    #[must_use]
    pub fn as_object(&self) -> Option<&BTreeMap<String, Self>> {
        match self {
            Self::Object(value) => Some(value),
            _ => None,
        }
    }

    #[must_use]
    pub fn as_array(&self) -> Option<&[Self]> {
        match self {
            Self::Array(value) => Some(value),
            _ => None,
        }
    }

    #[must_use]
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::String(value) => Some(value),
            _ => None,
        }
    }

    #[must_use]
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Self::Bool(value) => Some(*value),
            _ => None,
        }
    }

    #[must_use]
    pub fn as_i64(&self) -> Option<i64> {
        const I64_MAX_EXCLUSIVE_AS_F64: f64 = 9_223_372_036_854_775_808.0;
        match self {
            Self::Number(value)
                if value.is_finite()
                    && value.fract() == 0.0
                    && *value >= i64::MIN as f64
                    && *value < I64_MAX_EXCLUSIVE_AS_F64 =>
            {
                Some(*value as i64)
            }
            _ => None,
        }
    }

    #[must_use]
    pub fn get(&self, key: &str) -> Option<&Self> {
        self.as_object()?.get(key)
    }

    #[must_use]
    pub fn at<'a>(&'a self, path: &[&str]) -> Option<&'a Self> {
        let mut value = self;
        for key in path {
            value = value.get(key)?;
        }
        Some(value)
    }

    pub fn validated(&self) -> Result<(), ProviderCoreError> {
        let mut nodes = 0usize;
        self.validate_at(0, &mut nodes)
    }

    fn validate_at(&self, depth: usize, nodes: &mut usize) -> Result<(), ProviderCoreError> {
        if depth > Self::MAXIMUM_DEPTH {
            return Err(ProviderCoreError::invalid_value(
                "provider JSON exceeds depth limit",
            ));
        }
        *nodes = nodes
            .checked_add(1)
            .ok_or_else(|| ProviderCoreError::invalid_value("provider JSON node count overflow"))?;
        if *nodes > Self::MAXIMUM_NODES {
            return Err(ProviderCoreError::invalid_value(
                "provider JSON exceeds node limit",
            ));
        }
        match self {
            Self::Null | Self::Bool(_) => Ok(()),
            Self::Number(value) => {
                if value.is_finite() {
                    Ok(())
                } else {
                    Err(ProviderCoreError::invalid_value(
                        "provider JSON number is not finite",
                    ))
                }
            }
            Self::String(value) => {
                if value.len() <= Self::MAXIMUM_STRING_UTF8_BYTES && !value.contains('\0') {
                    Ok(())
                } else {
                    Err(ProviderCoreError::invalid_value(
                        "provider JSON string is oversized or contains NUL",
                    ))
                }
            }
            Self::Array(values) => {
                for value in values {
                    value.validate_at(depth + 1, nodes)?;
                }
                Ok(())
            }
            Self::Object(values) => {
                for (key, value) in values {
                    if key.is_empty()
                        || key.len() > Self::MAXIMUM_KEY_UTF8_BYTES
                        || key.contains('\0')
                    {
                        return Err(ProviderCoreError::invalid_value(
                            "provider JSON key is invalid",
                        ));
                    }
                    value.validate_at(depth + 1, nodes)?;
                }
                Ok(())
            }
        }
    }

    pub fn encoded_vec(&self) -> Result<Vec<u8>, ProviderCoreError> {
        self.validated()?;
        let data = serde_json::to_vec(self).map_err(|error| {
            ProviderCoreError::invalid_value(format!("provider JSON encoding failed: {error}"))
        })?;
        if data.len() > Self::MAXIMUM_ENCODED_BYTES {
            return Err(ProviderCoreError::invalid_value(
                "provider JSON exceeds byte limit",
            ));
        }
        Ok(data)
    }

    pub fn decode(data: &[u8]) -> Result<Self, ProviderCoreError> {
        if data.len() > Self::MAXIMUM_ENCODED_BYTES {
            return Err(ProviderCoreError::invalid_value(
                "provider JSON exceeds byte limit",
            ));
        }
        let raw: serde_json::Value = serde_json::from_slice(data).map_err(|error| {
            ProviderCoreError::invalid_value(format!("provider JSON decoding failed: {error}"))
        })?;
        let value = Self::try_from(raw)?;
        value.validated()?;
        Ok(value)
    }
}

impl TryFrom<serde_json::Value> for ProviderJsonValue {
    type Error = ProviderCoreError;

    fn try_from(value: serde_json::Value) -> Result<Self, Self::Error> {
        match value {
            serde_json::Value::Null => Ok(Self::Null),
            serde_json::Value::Bool(value) => Ok(Self::Bool(value)),
            serde_json::Value::Number(value) => {
                let number = value.as_f64().ok_or_else(|| {
                    ProviderCoreError::invalid_value("provider JSON number is out of range")
                })?;
                if !number.is_finite() {
                    return Err(ProviderCoreError::invalid_value(
                        "provider JSON number is not finite",
                    ));
                }
                Ok(Self::Number(number))
            }
            serde_json::Value::String(value) => Ok(Self::String(value)),
            serde_json::Value::Array(values) => values
                .into_iter()
                .map(Self::try_from)
                .collect::<Result<Vec<_>, _>>()
                .map(Self::Array),
            serde_json::Value::Object(values) => values
                .into_iter()
                .map(|(key, value)| Self::try_from(value).map(|value| (key, value)))
                .collect::<Result<BTreeMap<_, _>, _>>()
                .map(Self::Object),
        }
    }
}

impl TryFrom<ProviderJsonValue> for serde_json::Value {
    type Error = ProviderCoreError;

    fn try_from(value: ProviderJsonValue) -> Result<Self, Self::Error> {
        match value {
            ProviderJsonValue::Null => Ok(Self::Null),
            ProviderJsonValue::Bool(value) => Ok(Self::Bool(value)),
            ProviderJsonValue::Number(value) => serde_json::Number::from_f64(value)
                .map(Self::Number)
                .ok_or_else(|| {
                    ProviderCoreError::invalid_value("provider JSON number is not finite")
                }),
            ProviderJsonValue::String(value) => Ok(Self::String(value)),
            ProviderJsonValue::Array(values) => values
                .into_iter()
                .map(Self::try_from)
                .collect::<Result<Vec<_>, _>>()
                .map(Self::Array),
            ProviderJsonValue::Object(values) => values
                .into_iter()
                .map(|(key, value)| Self::try_from(value).map(|value| (key, value)))
                .collect::<Result<serde_json::Map<_, _>, _>>()
                .map(Self::Object),
        }
    }
}

impl Serialize for ProviderJsonValue {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            Self::Null => serializer.serialize_none(),
            Self::Bool(value) => serializer.serialize_bool(*value),
            Self::Number(value) => {
                if value.is_finite() {
                    serializer.serialize_f64(*value)
                } else {
                    Err(serde::ser::Error::custom(
                        "provider JSON number is not finite",
                    ))
                }
            }
            Self::String(value) => serializer.serialize_str(value),
            Self::Array(values) => values.serialize(serializer),
            Self::Object(values) => values.serialize(serializer),
        }
    }
}

impl<'de> Deserialize<'de> for ProviderJsonValue {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = serde_json::Value::deserialize(deserializer)?;
        let value = Self::try_from(raw).map_err(serde::de::Error::custom)?;
        value.validated().map_err(serde::de::Error::custom)?;
        Ok(value)
    }
}

impl From<bool> for ProviderJsonValue {
    fn from(value: bool) -> Self {
        Self::Bool(value)
    }
}
impl From<i64> for ProviderJsonValue {
    fn from(value: i64) -> Self {
        Self::Number(value as f64)
    }
}
impl From<i32> for ProviderJsonValue {
    fn from(value: i32) -> Self {
        Self::Number(f64::from(value))
    }
}
impl From<String> for ProviderJsonValue {
    fn from(value: String) -> Self {
        Self::String(value)
    }
}
impl From<&str> for ProviderJsonValue {
    fn from(value: &str) -> Self {
        Self::String(value.to_owned())
    }
}
impl From<Vec<ProviderJsonValue>> for ProviderJsonValue {
    fn from(value: Vec<ProviderJsonValue>) -> Self {
        Self::Array(value)
    }
}
impl From<BTreeMap<String, ProviderJsonValue>> for ProviderJsonValue {
    fn from(value: BTreeMap<String, ProviderJsonValue>) -> Self {
        Self::Object(value)
    }
}
