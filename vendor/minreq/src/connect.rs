//! sats patch: an unreachable DNS address must not consume the entire
//! request deadline while other addresses are still available.

use std::io;
use std::net::SocketAddr;
use std::time::Duration;

const ADDRESS_CONNECT_LIMIT: Duration = Duration::from_secs(2);

/// Only TCP establishment falls through to another address. No HTTP bytes
/// have been sent at this point, so this cannot replay a broadcast request.
/// The caller supplies the remaining *original* request deadline each time.
pub(crate) fn connect_addrs<T>(
    addrs: impl ExactSizeIterator<Item = SocketAddr>,
    mut remaining: impl FnMut() -> io::Result<Option<Duration>>,
    mut attempt: impl FnMut(&SocketAddr, Option<Duration>) -> io::Result<T>,
) -> io::Result<T> {
    let count = addrs.len();
    let mut last_error = io::Error::new(io::ErrorKind::AddrNotAvailable, "no resolved addresses");
    for (index, addr) in addrs.enumerate() {
        let timeout = remaining()?.map(|left| {
            if index + 1 < count {
                left.min(ADDRESS_CONNECT_LIMIT)
            } else {
                // Preserve the full remaining budget for a single-address
                // host or the last candidate. Never reset the deadline.
                left
            }
        });
        if timeout == Some(Duration::ZERO) {
            return Err(io::Error::new(io::ErrorKind::TimedOut, "request timed out"));
        }
        match attempt(&addr, timeout) {
            Ok(stream) => return Ok(stream),
            Err(error) => last_error = error,
        }
    }
    Err(last_error)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    fn addresses() -> std::array::IntoIter<SocketAddr, 3> {
        ["[::1]:80", "127.0.0.1:80", "127.0.0.2:80"]
            .map(|addr| addr.parse().unwrap())
            .into_iter()
    }

    #[test]
    fn a_stalled_address_falls_through_without_spending_the_request_budget() {
        let left = Cell::new(Duration::from_secs(30));
        let mut attempts = Vec::new();
        let result = connect_addrs(
            addresses(),
            || Ok(Some(left.get())),
            |addr, timeout| {
                let timeout = timeout.unwrap();
                attempts.push((*addr, timeout));
                if attempts.len() == 1 {
                    left.set(left.get() - timeout);
                    Err(io::ErrorKind::TimedOut.into())
                } else {
                    Ok(*addr)
                }
            },
        )
        .unwrap();
        assert_eq!(result, "127.0.0.1:80".parse().unwrap());
        assert_eq!(
            attempts.len(),
            2,
            "stop immediately after a connection succeeds"
        );
        assert_eq!(attempts[0].1, Duration::from_secs(2));
        assert_eq!(left.get(), Duration::from_secs(28));
    }

    #[test]
    fn all_failed_addresses_share_one_deadline_and_preserve_the_last_error() {
        let left = Cell::new(Duration::from_secs(5));
        let mut timeouts = Vec::new();
        let result: io::Result<()> = connect_addrs(
            addresses(),
            || Ok(Some(left.get())),
            |_, timeout| {
                let timeout = timeout.unwrap();
                timeouts.push(timeout);
                left.set(left.get() - timeout);
                Err(io::ErrorKind::ConnectionRefused.into())
            },
        );
        assert_eq!(
            timeouts,
            [
                Duration::from_secs(2),
                Duration::from_secs(2),
                Duration::from_secs(1)
            ]
        );
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::ConnectionRefused);
        assert_eq!(left.get(), Duration::ZERO);
    }

    #[test]
    fn expired_deadline_never_starts_another_connection() {
        let left = Cell::new(Duration::from_millis(100));
        let mut calls = 0;
        let result: io::Result<()> = connect_addrs(
            addresses(),
            || Ok(Some(left.get())),
            |_, timeout| {
                calls += 1;
                assert_eq!(timeout, Some(Duration::from_millis(100)));
                left.set(Duration::ZERO);
                Err(io::ErrorKind::TimedOut.into())
            },
        );
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::TimedOut);
        assert_eq!(calls, 1);
    }

    #[test]
    fn single_address_and_unbounded_callers_keep_their_existing_limits() {
        let addr = "127.0.0.1:80".parse().unwrap();
        connect_addrs(
            [addr].into_iter(),
            || Ok(Some(Duration::from_secs(30))),
            |_, timeout| {
                assert_eq!(timeout, Some(Duration::from_secs(30)));
                Ok(())
            },
        )
        .unwrap();
        let mut calls = 0;
        connect_addrs(
            addresses(),
            || Ok(None),
            |_, timeout| {
                calls += 1;
                assert_eq!(timeout, None);
                if calls == 1 {
                    Err(io::ErrorKind::ConnectionRefused.into())
                } else {
                    Ok(())
                }
            },
        )
        .unwrap();
        assert_eq!(calls, 2);
    }
}
