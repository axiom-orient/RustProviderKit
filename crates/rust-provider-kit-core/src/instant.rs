use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::ProviderCoreError;

/// Milliseconds since the Unix epoch. The value is deterministic on the wire and
/// avoids floating-point date encodings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ProviderInstant(i64);

impl ProviderInstant {
    #[must_use]
    pub const fn from_unix_milliseconds(value: i64) -> Self {
        Self(value)
    }

    #[must_use]
    pub const fn as_unix_milliseconds(self) -> i64 {
        self.0
    }

    pub fn from_system_time(value: SystemTime) -> Result<Self, ProviderCoreError> {
        match value.duration_since(UNIX_EPOCH) {
            Ok(duration) => Self::from_positive_duration(duration),
            Err(error) => {
                let duration = error.duration();
                let millis = duration.as_millis();
                let signed = i64::try_from(millis).map_err(|_| {
                    ProviderCoreError::invalid_value("provider instant is out of range")
                })?;
                Ok(Self(-signed))
            }
        }
    }

    fn from_positive_duration(duration: Duration) -> Result<Self, ProviderCoreError> {
        let millis = i64::try_from(duration.as_millis())
            .map_err(|_| ProviderCoreError::invalid_value("provider instant is out of range"))?;
        Ok(Self(millis))
    }

    pub fn checked_add(self, duration: Duration) -> Result<Self, ProviderCoreError> {
        let millis = i64::try_from(duration.as_millis())
            .map_err(|_| ProviderCoreError::invalid_value("provider duration is out of range"))?;
        self.0
            .checked_add(millis)
            .map(Self)
            .ok_or_else(|| ProviderCoreError::invalid_value("provider instant overflow"))
    }
}
