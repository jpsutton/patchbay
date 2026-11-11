//! Which addresses a Patchbay listener binds.
//!
//! The mesh and the daemon's web UI expose a lot (agents, terminals, files), so no listener may
//! sit on a public interface unless the user asked for that address by name. A wildcard
//! (`0.0.0.0`, `[::]`) is not such a request: it means "every private, VPN and loopback address
//! this computer has". Only a specific IP in the setting (`203.0.113.7:41890`) binds that IP, even
//! when it is public.
//!
//! [`Binder`] keeps one listener per chosen address and re-reads the interface list when asked
//! (a VPN that comes up after start gets its listener, and an address that goes away loses it).

use std::collections::{BTreeSet, HashMap, HashSet};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::Duration;

use anyhow::{Result, anyhow};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

/// How often a long-running listener calls [`Binder::sync`] to follow interface changes.
pub const RESCAN: Duration = Duration::from_secs(10);

/// True for an address that is routable on the internet. Loopback, private (RFC 1918), carrier-grade
/// NAT (`100.64.0.0/10`, which Tailscale uses), link-local and IPv6 unique-local addresses are not.
pub fn is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_public_v4(v4),
        IpAddr::V6(v6) => is_public_v6(v6),
    }
}

fn is_public_v4(ip: Ipv4Addr) -> bool {
    let o = ip.octets();
    let cgnat = o[0] == 100 && (o[1] & 0xc0) == 64;
    !(ip.is_loopback() || ip.is_private() || ip.is_link_local() || ip.is_unspecified() || cgnat)
}

fn is_v6_link_local(ip: Ipv6Addr) -> bool {
    (ip.segments()[0] & 0xffc0) == 0xfe80
}

fn is_public_v6(ip: Ipv6Addr) -> bool {
    if let Some(v4) = ip.to_ipv4_mapped() {
        return is_public_v4(v4);
    }
    let unique_local = (ip.segments()[0] & 0xfe00) == 0xfc00;
    !(ip.is_loopback() || ip.is_unspecified() || unique_local || is_v6_link_local(ip))
}

/// The addresses to bind for `want`, given this computer's addresses `local`.
///
/// A specific address is returned as it is. A wildcard becomes one entry per non-public local
/// address: `0.0.0.0` only IPv4, `[::]` both families. IPv6 link-local addresses are left out,
/// because binding one needs an interface scope the setting cannot carry.
pub fn targets(want: SocketAddr, local: &[IpAddr]) -> Vec<SocketAddr> {
    if !want.ip().is_unspecified() {
        return vec![want];
    }
    let v6 = want.ip().is_ipv6();
    let chosen: BTreeSet<IpAddr> = local
        .iter()
        .copied()
        .filter(|ip| !is_public(*ip))
        .filter(|ip| v6 || ip.is_ipv4())
        .filter(|ip| !matches!(ip, IpAddr::V6(v) if is_v6_link_local(*v)))
        .collect();
    chosen.into_iter().map(|ip| SocketAddr::new(ip, want.port())).collect()
}

/// This computer's interface addresses. When they can't be listed, only IPv4 loopback is
/// returned, so the failure leaves a listener on this computer alone and never a public one.
pub fn local_ips() -> Vec<IpAddr> {
    match if_addrs::get_if_addrs() {
        Ok(list) if !list.is_empty() => list.into_iter().map(|i| i.ip()).collect(),
        Ok(_) => vec![IpAddr::V4(Ipv4Addr::LOCALHOST)],
        Err(e) => {
            tracing::warn!("cannot list network interfaces, listening on loopback only: {e}");
            vec![IpAddr::V4(Ipv4Addr::LOCALHOST)]
        }
    }
}

/// The listeners for one configured address. Dropping it stops them all.
pub struct Binder {
    want: SocketAddr,
    /// The port of the first listener. With port 0 every other listener reuses it.
    port: Option<u16>,
    bound: HashMap<SocketAddr, JoinHandle<()>>,
    /// Addresses that failed to bind, so a retry every [`RESCAN`] doesn't repeat the warning.
    failed: HashSet<SocketAddr>,
}

impl Binder {
    pub fn new(want: SocketAddr) -> Self {
        Self { want, port: None, bound: HashMap::new(), failed: HashSet::new() }
    }

    /// The port the listeners use, once one is bound.
    pub fn port(&self) -> Option<u16> {
        self.port
    }

    /// Bind the addresses that are missing and stop the ones that are gone. `spawn` takes a new
    /// listener and returns the task that serves it. Fails only when nothing at all is bound.
    pub async fn sync(&mut self, spawn: impl FnMut(TcpListener) -> JoinHandle<()>) -> Result<()> {
        self.sync_with(&local_ips(), spawn).await
    }

    async fn sync_with(&mut self, local: &[IpAddr], mut spawn: impl FnMut(TcpListener) -> JoinHandle<()>) -> Result<()> {
        let port = if self.want.port() == 0 { self.port.unwrap_or(0) } else { self.want.port() };
        let wanted = targets(SocketAddr::new(self.want.ip(), port), local);
        self.bound.retain(|addr, task| {
            let keep = wanted.contains(addr) && !task.is_finished();
            if !keep {
                task.abort();
                tracing::info!("stopped listening on {addr}");
            }
            keep
        });
        self.failed.retain(|addr| wanted.contains(addr));
        let mut last_err = None;
        for t in wanted {
            // The first bind on port 0 picks the port; the others take the same one.
            let t = if t.port() == 0 { SocketAddr::new(t.ip(), self.port.unwrap_or(0)) } else { t };
            if self.bound.contains_key(&t) {
                continue;
            }
            match TcpListener::bind(t).await {
                Ok(listener) => {
                    let actual = listener.local_addr().unwrap_or(t);
                    self.port.get_or_insert(actual.port());
                    self.failed.remove(&t);
                    tracing::info!("listening on {actual}");
                    self.bound.insert(actual, spawn(listener));
                }
                Err(e) => {
                    if self.failed.insert(t) {
                        tracing::warn!("cannot listen on {t}: {e}");
                    }
                    last_err = Some(anyhow::Error::new(e).context(format!("listening on {t}")));
                }
            }
        }
        if self.bound.is_empty() {
            return Err(last_err.unwrap_or_else(|| {
                anyhow!("{} matches no private or loopback address on this computer", self.want)
            }));
        }
        Ok(())
    }
}

impl Drop for Binder {
    fn drop(&mut self) {
        for task in self.bound.values() {
            task.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    fn sa(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    #[test]
    fn public_and_private_addresses() {
        for private in [
            "127.0.0.1", "10.1.2.3", "172.16.0.5", "172.31.255.1", "192.168.1.20", "169.254.3.4", "100.64.0.7",
            "100.127.255.254", "::1", "fd7a:115c:a1e0::1", "fc00::1", "fe80::1", "::ffff:192.168.1.1",
        ] {
            assert!(!is_public(ip(private)), "{private} is not public");
        }
        for public in [
            "8.8.8.8", "203.0.113.7", "100.63.255.255", "100.128.0.1", "172.32.0.1", "11.0.0.1", "2001:db8::1",
            "2606:4700::1111", "::ffff:8.8.8.8",
        ] {
            assert!(is_public(ip(public)), "{public} is public");
        }
    }

    #[test]
    fn ipv4_wildcard_skips_public_and_ipv6() {
        let local = [ip("127.0.0.1"), ip("192.168.1.20"), ip("100.64.0.7"), ip("203.0.113.7"), ip("::1"), ip("2606:4700::1111")];
        let got = targets(sa("0.0.0.0:41890"), &local);
        assert_eq!(got, vec![sa("100.64.0.7:41890"), sa("127.0.0.1:41890"), sa("192.168.1.20:41890")]);
    }

    #[test]
    fn ipv6_wildcard_takes_both_families_without_public_or_link_local() {
        let local = [
            ip("127.0.0.1"),
            ip("203.0.113.7"),
            ip("::1"),
            ip("fd7a:115c:a1e0::1"),
            ip("fe80::1"),
            ip("2606:4700::1111"),
        ];
        let got = targets(sa("[::]:41890"), &local);
        assert_eq!(got, vec![sa("127.0.0.1:41890"), sa("[::1]:41890"), sa("[fd7a:115c:a1e0::1]:41890")]);
    }

    #[test]
    fn a_named_address_is_kept_even_when_public() {
        let local = [ip("203.0.113.7")];
        assert_eq!(targets(sa("203.0.113.7:41890"), &local), vec![sa("203.0.113.7:41890")]);
        assert_eq!(targets(sa("127.0.0.1:0"), &local), vec![sa("127.0.0.1:0")]);
    }

    #[test]
    fn only_public_addresses_leave_nothing_to_bind() {
        assert!(targets(sa("0.0.0.0:41890"), &[ip("203.0.113.7")]).is_empty());
    }

    fn idle(listener: TcpListener) -> JoinHandle<()> {
        tokio::spawn(async move {
            let _keep = listener;
            std::future::pending::<()>().await;
        })
    }

    #[tokio::test]
    async fn wildcard_binds_loopback_only_and_is_stable_across_syncs() {
        let local = [ip("127.0.0.1"), ip("203.0.113.7")];
        let mut b = Binder::new(sa("0.0.0.0:0"));
        b.sync_with(&local, idle).await.unwrap();
        let port = b.port().unwrap();
        assert_eq!(b.bound.keys().copied().collect::<Vec<_>>(), vec![SocketAddr::new(ip("127.0.0.1"), port)]);
        b.sync_with(&local, idle).await.unwrap();
        assert_eq!(b.bound.len(), 1);
        assert_eq!(b.port(), Some(port));
    }

    #[tokio::test]
    async fn an_address_that_leaves_loses_its_listener() {
        // 127.0.0.2 is also loopback on Linux; skip where it can't bind.
        if std::net::TcpListener::bind("127.0.0.2:0").is_err() {
            return;
        }
        let mut b = Binder::new(sa("0.0.0.0:0"));
        b.sync_with(&[ip("127.0.0.1"), ip("127.0.0.2")], idle).await.unwrap();
        assert_eq!(b.bound.len(), 2);
        b.sync_with(&[ip("127.0.0.1")], idle).await.unwrap();
        assert_eq!(b.bound.len(), 1);
    }

    #[tokio::test]
    async fn nothing_to_bind_is_an_error() {
        let mut b = Binder::new(sa("0.0.0.0:0"));
        let err = b.sync_with(&[ip("203.0.113.7")], idle).await.unwrap_err();
        assert!(err.to_string().contains("no private or loopback address"), "{err}");
    }
}
