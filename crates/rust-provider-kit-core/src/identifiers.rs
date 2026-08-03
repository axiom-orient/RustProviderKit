use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::ProviderCoreError;

fn validate_provider(value: &str) -> Result<(), ProviderCoreError> {
    let valid = !value.is_empty()
        && value.len() <= 64
        && value.trim() == value
        && value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'.' | b'_')
        });
    if valid {
        Ok(())
    } else {
        Err(ProviderCoreError::invalid_identifier(
            "provider identifier is empty, oversized, or contains unsupported characters",
        ))
    }
}

fn validate_opaque(value: &str, maximum_utf8_bytes: usize) -> Result<(), ProviderCoreError> {
    let valid = !value.is_empty()
        && value.len() <= maximum_utf8_bytes
        && value.trim() == value
        && value
            .bytes()
            .all(|byte| (33..=126).contains(&byte) && !matches!(byte, b'"' | b'\'' | b'\\'));
    if valid {
        Ok(())
    } else {
        Err(ProviderCoreError::invalid_identifier(
            "provider identifier is empty, oversized, or contains unsupported characters",
        ))
    }
}

macro_rules! identifier {
    ($name:ident, provider) => {
        #[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(String);

        impl $name {
            pub fn new(value: impl Into<String>) -> Result<Self, ProviderCoreError> {
                let value = value.into();
                validate_provider(&value)?;
                Ok(Self(value))
            }

            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }

            #[must_use]
            pub fn into_string(self) -> String {
                self.0
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.debug_tuple(stringify!($name)).field(&self.0).finish()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl Serialize for $name {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: Serializer,
            {
                serializer.serialize_str(&self.0)
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                let value = String::deserialize(deserializer)?;
                Self::new(value).map_err(serde::de::Error::custom)
            }
        }

        impl TryFrom<&str> for $name {
            type Error = ProviderCoreError;
            fn try_from(value: &str) -> Result<Self, Self::Error> {
                Self::new(value)
            }
        }
    };
    ($name:ident, opaque, $max:expr) => {
        #[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(String);

        impl $name {
            pub fn new(value: impl Into<String>) -> Result<Self, ProviderCoreError> {
                let value = value.into();
                validate_opaque(&value, $max)?;
                Ok(Self(value))
            }

            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }

            #[must_use]
            pub fn into_string(self) -> String {
                self.0
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.debug_tuple(stringify!($name)).field(&self.0).finish()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl Serialize for $name {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: Serializer,
            {
                serializer.serialize_str(&self.0)
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                let value = String::deserialize(deserializer)?;
                Self::new(value).map_err(serde::de::Error::custom)
            }
        }

        impl TryFrom<&str> for $name {
            type Error = ProviderCoreError;
            fn try_from(value: &str) -> Result<Self, Self::Error> {
                Self::new(value)
            }
        }
    };
}

identifier!(ProviderId, provider);
identifier!(ProviderAccountId, opaque, 192);
identifier!(ProviderModelId, opaque, 192);
identifier!(ProviderRequestId, opaque, 128);
identifier!(ProviderCredentialReference, opaque, 128);
identifier!(ProviderConformanceReceiptId, opaque, 128);

#[derive(Debug, Clone, Copy)]
pub struct BuiltInProviderId;

impl BuiltInProviderId {
    fn required(value: &'static str) -> ProviderId {
        // The literals below are compile-time controlled and satisfy the same
        // validation contract as `ProviderId::new`.
        ProviderId(value.to_owned())
    }

    #[must_use]
    pub fn codex() -> ProviderId {
        Self::required("codex")
    }
    #[must_use]
    pub fn open_ai() -> ProviderId {
        Self::required("openai")
    }
    #[must_use]
    pub fn anthropic() -> ProviderId {
        Self::required("anthropic")
    }
    #[must_use]
    pub fn gemini() -> ProviderId {
        Self::required("gemini")
    }
    #[must_use]
    pub fn open_router() -> ProviderId {
        Self::required("openrouter")
    }
    #[must_use]
    pub fn deep_seek() -> ProviderId {
        Self::required("deepseek")
    }
    #[must_use]
    pub fn qwen() -> ProviderId {
        Self::required("qwen")
    }
    #[must_use]
    pub fn kimi() -> ProviderId {
        Self::required("kimi")
    }
    #[must_use]
    pub fn zai() -> ProviderId {
        Self::required("zai")
    }
    #[must_use]
    pub fn mini_max() -> ProviderId {
        Self::required("minimax")
    }

    #[must_use]
    pub fn all() -> [ProviderId; 10] {
        [
            Self::codex(),
            Self::open_ai(),
            Self::anthropic(),
            Self::gemini(),
            Self::open_router(),
            Self::deep_seek(),
            Self::qwen(),
            Self::kimi(),
            Self::zai(),
            Self::mini_max(),
        ]
    }
}
