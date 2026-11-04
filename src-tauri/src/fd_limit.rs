//! The open-file limit this process runs under.
//!
//! A desktop app launched from the Finder, or a shell, starts at a soft limit of 256 on macOS.
//! A mesh host runs sessions, agents, terminals, databases, and a socket per peer and phone,
//! and can run through that. Raising the soft limit up to the hard one is the process's own
//! call to make, so both entry points make it at startup.

/// Where the soft limit is raised to, when the hard limit allows. Plenty for the app, and below
/// macOS's `OPEN_MAX` (10240), which `setrlimit` refuses to exceed.
#[cfg(unix)]
pub const WANT: libc::rlim_t = 8192;

/// The soft open-file limit before and after a raise; equal when it was already enough.
/// (`rlim_t` is `u64` on every unix target this builds for.)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Raised {
    pub before: u64,
    pub after: u64,
}

/// Raise the soft open-file limit to `WANT`, or to the hard limit if that is lower.
#[cfg(unix)]
pub fn raise() -> std::io::Result<Raised> {
    let mut lim = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
    // SAFETY: `lim` is a valid, writable rlimit for the duration of both calls.
    unsafe {
        if libc::getrlimit(libc::RLIMIT_NOFILE, &mut lim) != 0 {
            return Err(std::io::Error::last_os_error());
        }
        let before = lim.rlim_cur;
        let want = WANT.min(lim.rlim_max);
        if lim.rlim_cur >= want {
            return Ok(Raised { before, after: before });
        }
        lim.rlim_cur = want;
        if libc::setrlimit(libc::RLIMIT_NOFILE, &lim) != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(Raised { before, after: want })
    }
}

/// Raise the limit at startup and say what happened, once. Best effort: a failure is logged
/// and nothing else. No such limit on Windows.
pub fn raise_at_startup() {
    #[cfg(unix)]
    match raise() {
        Ok(r) if r.after > r.before => tracing::info!("open-file limit raised from {} to {}", r.before, r.after),
        Ok(r) => tracing::debug!("open-file limit left at {}", r.before),
        Err(e) => tracing::warn!("could not raise the open-file limit: {e}"),
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn raises_the_soft_limit_to_want_or_the_hard_limit() {
        let r = raise().unwrap();
        let mut lim = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
        // SAFETY: `lim` is a valid, writable rlimit.
        assert_eq!(unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut lim) }, 0);
        assert!(r.after >= r.before);
        assert_eq!(r.after, lim.rlim_cur);
        // At least the target (a shell may already have set more).
        assert!(lim.rlim_cur >= WANT.min(lim.rlim_max), "{} < {}", lim.rlim_cur, WANT.min(lim.rlim_max));
        // Already there: the second raise changes nothing.
        let again = raise().unwrap();
        assert_eq!(again, Raised { before: r.after, after: r.after });
    }
}
