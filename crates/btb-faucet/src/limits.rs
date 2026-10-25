//! In-memory rate limits: one drip per address per cooldown, and at most `ip_max` drips per IP
//! per window. Entries expire; the maps are capped so a flood of fake addresses cannot grow
//! them without bound.

use std::collections::{HashMap, VecDeque};
use std::net::IpAddr;
use std::time::{Duration, Instant};

const MAX_ENTRIES: usize = 200_000;

#[derive(Debug, Clone)]
pub struct LimitConfig {
    pub address_cooldown: Duration,
    pub ip_max: usize,
    pub ip_window: Duration,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Denied {
    /// Retry after this many seconds.
    Address(u64),
    Ip(u64),
    /// The tables are full of live entries; try later.
    Busy,
}

#[derive(Default)]
pub struct Limits {
    by_address: HashMap<[u8; 32], Instant>,
    by_ip: HashMap<IpAddr, VecDeque<Instant>>,
}

/// IPv6 clients are limited per /64 (a single host usually owns a whole /64).
pub fn bucket(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return IpAddr::V4(v4);
            }
            let mut o = v6.octets();
            o[8..].fill(0);
            IpAddr::V6(o.into())
        }
        v4 => v4,
    }
}

fn secs_left(until: Instant, now: Instant) -> u64 {
    until.saturating_duration_since(now).as_secs() + 1
}

impl Limits {
    fn prune(&mut self, cfg: &LimitConfig, now: Instant) {
        self.by_address
            .retain(|_, t| now.duration_since(*t) < cfg.address_cooldown);
        self.by_ip.retain(|_, q| {
            while q
                .front()
                .is_some_and(|t| now.duration_since(*t) >= cfg.ip_window)
            {
                q.pop_front();
            }
            !q.is_empty()
        });
    }

    /// Count a drip against both limits, or say why not. Call `release` if the drip then fails.
    pub fn reserve(
        &mut self,
        cfg: &LimitConfig,
        addr: [u8; 32],
        ip: IpAddr,
        now: Instant,
    ) -> Result<(), Denied> {
        let ip = bucket(ip);
        if self.by_address.len() + self.by_ip.len() >= MAX_ENTRIES / 2 {
            self.prune(cfg, now);
            if self.by_address.len() + self.by_ip.len() >= MAX_ENTRIES {
                return Err(Denied::Busy);
            }
        }
        if let Some(t) = self.by_address.get(&addr) {
            let until = *t + cfg.address_cooldown;
            if now < until {
                return Err(Denied::Address(secs_left(until, now)));
            }
        }
        let q = self.by_ip.entry(ip).or_default();
        while q
            .front()
            .is_some_and(|t| now.duration_since(*t) >= cfg.ip_window)
        {
            q.pop_front();
        }
        if q.len() >= cfg.ip_max {
            let until = *q.front().expect("non-empty") + cfg.ip_window;
            return Err(Denied::Ip(secs_left(until, now)));
        }
        q.push_back(now);
        self.by_address.insert(addr, now);
        Ok(())
    }

    /// Undo a `reserve` whose drip did not happen.
    pub fn release(&mut self, addr: [u8; 32], ip: IpAddr) {
        self.by_address.remove(&addr);
        let ip = bucket(ip);
        if let Some(q) = self.by_ip.get_mut(&ip) {
            q.pop_back();
            if q.is_empty() {
                self.by_ip.remove(&ip);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> LimitConfig {
        LimitConfig {
            address_cooldown: Duration::from_secs(60),
            ip_max: 2,
            ip_window: Duration::from_secs(100),
        }
    }
    fn ip(n: u8) -> IpAddr {
        IpAddr::from([10, 0, 0, n])
    }

    #[test]
    fn address_cooldown_then_allowed_again() {
        let (mut l, t0) = (Limits::default(), Instant::now());
        assert!(l.reserve(&cfg(), [1; 32], ip(1), t0).is_ok());
        let d = l.reserve(&cfg(), [1; 32], ip(2), t0 + Duration::from_secs(10));
        assert!(matches!(d, Err(Denied::Address(s)) if (50..=51).contains(&s)));
        assert!(l
            .reserve(&cfg(), [1; 32], ip(2), t0 + Duration::from_secs(61))
            .is_ok());
    }

    #[test]
    fn ip_window_limit() {
        let (mut l, t0) = (Limits::default(), Instant::now());
        assert!(l.reserve(&cfg(), [1; 32], ip(1), t0).is_ok());
        assert!(l.reserve(&cfg(), [2; 32], ip(1), t0).is_ok());
        assert!(matches!(
            l.reserve(&cfg(), [3; 32], ip(1), t0),
            Err(Denied::Ip(_))
        ));
        // another IP is unaffected
        assert!(l.reserve(&cfg(), [3; 32], ip(2), t0).is_ok());
        // the window slides
        assert!(l
            .reserve(&cfg(), [4; 32], ip(1), t0 + Duration::from_secs(101))
            .is_ok());
    }

    #[test]
    fn release_gives_the_slot_back() {
        let (mut l, t0) = (Limits::default(), Instant::now());
        l.reserve(&cfg(), [1; 32], ip(1), t0).unwrap();
        l.release([1; 32], ip(1));
        assert!(l.reserve(&cfg(), [1; 32], ip(1), t0).is_ok());
    }

    #[test]
    fn ipv6_is_bucketed_per_64() {
        let a: IpAddr = "2001:db8::1".parse().unwrap();
        let b: IpAddr = "2001:db8::ffff".parse().unwrap();
        let c: IpAddr = "2001:db8:0:1::1".parse().unwrap();
        assert_eq!(bucket(a), bucket(b));
        assert_ne!(bucket(a), bucket(c));
        let mapped: IpAddr = "::ffff:10.0.0.1".parse().unwrap();
        assert_eq!(bucket(mapped), ip(1));
    }
}
