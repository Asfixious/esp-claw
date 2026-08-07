use core::fmt;
use core::str::FromStr;

/// A validated RPC address in `group.method` form.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RpcAddress(Box<str>);

impl RpcAddress {
    /// Parses and validates an RPC address.
    ///
    /// # Errors
    ///
    /// Returns [`RpcAddressError::InvalidFormat`] unless the address contains
    /// exactly two non-empty ASCII identifier segments separated by one dot.
    pub fn parse(value: &str) -> Result<Self, RpcAddressError> {
        let mut segments = value.split('.');
        let group = segments.next();
        let method = segments.next();
        if segments.next().is_some()
            || !group.is_some_and(is_valid_segment)
            || !method.is_some_and(is_valid_segment)
        {
            return Err(RpcAddressError::InvalidFormat(value.into()));
        }
        Ok(Self(value.into()))
    }

    /// Returns the validated address as text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

fn is_valid_segment(segment: &str) -> bool {
    !segment.is_empty()
        && segment
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

impl fmt::Display for RpcAddress {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for RpcAddress {
    type Err = RpcAddressError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse(value)
    }
}

impl AsRef<str> for RpcAddress {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

/// Failure returned while parsing an [`RpcAddress`].
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum RpcAddressError {
    /// The value is not in validated `group.method` form.
    #[error("invalid RPC address: {0}")]
    InvalidFormat(Box<str>),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_accepts_group_and_method_identifiers() {
        let address = RpcAddress::parse("scheduler.create-2");
        assert_eq!(
            address.as_ref().map(RpcAddress::as_str),
            Ok("scheduler.create-2")
        );
    }

    #[test]
    fn parse_rejects_missing_or_extra_segments() {
        assert!(RpcAddress::parse("scheduler").is_err());
        assert!(RpcAddress::parse("scheduler.create.extra").is_err());
        assert!(RpcAddress::parse("scheduler.").is_err());
    }
}
