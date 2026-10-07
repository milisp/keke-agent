use serde::{Deserialize, Serialize};

/// Resource budgets applied before an image becomes model-visible history.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ImageLimits {
    /// Maximum bytes read from a source file.
    pub read_bytes: u64,
    /// Maximum source pixel count, checked before decoding pixel data.
    pub decoded_pixels: u64,
    /// Maximum width or height sent to the model.
    pub max_dimension: u32,
    /// Maximum encoded image bytes, before base64 expansion.
    pub encoded_bytes: u64,
}

impl Default for ImageLimits {
    fn default() -> Self {
        Self {
            read_bytes: 20 * 1024 * 1024,
            decoded_pixels: 40_000_000,
            max_dimension: 2048,
            encoded_bytes: 3 * 1024 * 1024,
        }
    }
}

impl ImageLimits {
    /// Refuse unbounded budgets and values that cannot hold a useful image.
    pub fn check(self) -> Result<(), String> {
        for (name, value, min, max) in [
            ("read_bytes", self.read_bytes, 1024, 100 * 1024 * 1024),
            ("decoded_pixels", self.decoded_pixels, 1, 100_000_000),
            ("max_dimension", u64::from(self.max_dimension), 1, 8192),
            ("encoded_bytes", self.encoded_bytes, 1024, 20 * 1024 * 1024),
        ] {
            if !(min..=max).contains(&value) {
                return Err(format!(
                    "images.{name} must be between {min} and {max}, got {value}"
                ));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn budgets_cannot_be_disabled_or_unbounded() {
        assert!(ImageLimits::default().check().is_ok());
        assert!(
            ImageLimits {
                read_bytes: 0,
                ..ImageLimits::default()
            }
            .check()
            .is_err()
        );
        assert!(
            ImageLimits {
                decoded_pixels: u64::MAX,
                ..ImageLimits::default()
            }
            .check()
            .is_err()
        );
    }
}
