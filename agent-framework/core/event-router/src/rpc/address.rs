use core::fmt;

/// A validated RPC address in `group.method` form.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RpcAddress(Box<str>);

impl TryFrom<&str> for RpcAddress {
    type Error = RpcAddressError;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
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
}

fn is_valid_segment(segment: &str) -> bool {
    !segment.is_empty()
        && segment
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

impl fmt::Display for RpcAddress {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl AsRef<str> for RpcAddress {
    fn as_ref(&self) -> &str {
        &self.0
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
        let address = RpcAddress::try_from("scheduler.create-2");
        assert_eq!(
            address.as_ref().map(AsRef::as_ref),
            Ok("scheduler.create-2")
        );
    }

    #[test]
    fn parse_rejects_missing_or_extra_segments() {
        assert!(RpcAddress::try_from("scheduler").is_err());
        assert!(RpcAddress::try_from("scheduler.create.extra").is_err());
        assert!(RpcAddress::try_from("scheduler.").is_err());
    }
}
