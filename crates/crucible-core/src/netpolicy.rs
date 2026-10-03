//! Which upstreams the platform may talk to on a user's behalf.
//!
//! A user supplies the model endpoint, so the meter must not become a way to
//! reach the runner's own network (cloud metadata, the Docker bridge,
//! localhost services). Rules: https only, no credentials/query/fragment in
//! the URL, and every address the host resolves to must be public unicast.
//! The address check here is pure; the meter applies it to each DNS answer
//! right before connecting (see `crucible-meter`).

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use url::{Host, Url};

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum EndpointError {
    #[error("upstream endpoint is not a valid URL")]
    Invalid,
    #[error("upstream endpoint must be https")]
    Scheme,
    #[error("upstream endpoint must not carry credentials")]
    Credentials,
    #[error("upstream endpoint must not have a query or fragment")]
    QueryOrFragment,
    #[error("upstream endpoint has no host")]
    NoHost,
    #[error("upstream endpoint is a non-public address")]
    NonPublicAddress,
}

/// Parse and check a user-supplied endpoint (base URL, e.g.
/// `https://api.example.com/v1`). `allow_insecure` permits http and
/// non-public IP literals; it exists for local tests only.
pub fn validate_endpoint(raw: &str, allow_insecure: bool) -> Result<Url, EndpointError> {
    let raw = raw.trim();
    // WHATWG parsing turns "https:///v1" into host "v1"; an empty authority
    // is a typo or a trick, refuse it before the parser "fixes" it.
    if let Some((_, rest)) = raw.split_once("://")
        && (rest.is_empty() || rest.starts_with('/'))
    {
        return Err(EndpointError::NoHost);
    }
    let url = Url::parse(raw).map_err(|_| EndpointError::Invalid)?;
    match url.scheme() {
        "https" => {}
        "http" if allow_insecure => {}
        _ => return Err(EndpointError::Scheme),
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(EndpointError::Credentials);
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err(EndpointError::QueryOrFragment);
    }
    let ip = match url.host() {
        None => return Err(EndpointError::NoHost),
        Some(Host::Domain("")) => return Err(EndpointError::NoHost),
        Some(Host::Domain(_)) => None,
        Some(Host::Ipv4(v4)) => Some(IpAddr::V4(v4)),
        Some(Host::Ipv6(v6)) => Some(IpAddr::V6(v6)),
    };
    // HTTP clients skip DNS for IP literals, so the resolver-side check
    // would never see them: check here.
    if let Some(ip) = ip
        && !allow_insecure
        && !is_public_ip(ip)
    {
        return Err(EndpointError::NonPublicAddress);
    }
    Ok(url)
}

/// Public unicast only. Conservative: anything special-purpose is refused.
pub fn is_public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_public_v4(v4),
        IpAddr::V6(v6) => is_public_v6(v6),
    }
}

fn is_public_v4(ip: Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    let blocked = a == 0                                  // 0.0.0.0/8 "this network"
        || a == 10                                        // private
        || a == 127                                       // loopback
        || (a == 100 && (64..128).contains(&b))           // 100.64/10 CGNAT
        || (a == 169 && b == 254)                         // link-local, cloud metadata
        || (a == 172 && (16..32).contains(&b))            // private
        || (a == 192 && b == 0 && c == 0)                 // IETF protocol assignments
        || (a == 192 && b == 0 && c == 2)                 // TEST-NET-1
        || (a == 192 && b == 168)                         // private
        || (a == 198 && (18..20).contains(&b))            // benchmarking
        || (a == 198 && b == 51 && c == 100)              // TEST-NET-2
        || (a == 203 && b == 0 && c == 113)               // TEST-NET-3
        || a >= 224; // multicast, reserved, broadcast
    !blocked
}

fn is_public_v6(ip: Ipv6Addr) -> bool {
    let seg = ip.segments();
    // IPv4 embedded in IPv6 (mapped ::ffff:a.b.c.d, NAT64 64:ff9b::/96):
    // judge by the IPv4 address it reaches.
    if let Some(v4) = ip.to_ipv4_mapped() {
        return is_public_v4(v4);
    }
    if seg[0] == 0x64 && seg[1] == 0xff9b && seg[2..6] == [0, 0, 0, 0] {
        let v4 = Ipv4Addr::new(
            (seg[6] >> 8) as u8,
            seg[6] as u8,
            (seg[7] >> 8) as u8,
            seg[7] as u8,
        );
        return is_public_v4(v4);
    }
    // Global unicast is 2000::/3; within it refuse the special blocks:
    // 2001::/23 (IETF: Teredo, ORCHID, ...), 2001:db8::/32 (docs),
    // 2002::/16 (6to4, can embed private IPv4).
    let in_2000_3 = (seg[0] & 0xe000) == 0x2000;
    let ietf = seg[0] == 0x2001 && seg[1] < 0x0200;
    let doc = seg[0] == 0x2001 && seg[1] == 0x0db8;
    let six_to_four = seg[0] == 0x2002;
    in_2000_3 && !ietf && !doc && !six_to_four
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn non_public_addresses() {
        for s in [
            "0.0.0.0",
            "127.0.0.1",
            "127.255.0.1",
            "10.0.0.1",
            "172.16.0.1",
            "172.31.250.1",
            "192.168.1.1",
            "169.254.169.254",
            "100.64.0.1",
            "100.127.255.255",
            "192.0.2.1",
            "198.18.0.1",
            "224.0.0.1",
            "255.255.255.255",
            "::",
            "::1",
            "fc00::1",
            "fd12:3456::1",
            "fe80::1",
            "fec0::1",
            "ff02::1",
            "::ffff:127.0.0.1",
            "::ffff:10.1.2.3",
            "64:ff9b::a9fe:a9fe",
            "2001:db8::1",
            "2002:7f00:1::1",
        ] {
            assert!(!is_public_ip(ip(s)), "{s} must be refused");
        }
    }

    #[test]
    fn public_addresses() {
        for s in [
            "1.1.1.1",
            "8.8.8.8",
            "172.32.0.1",
            "100.128.0.1",
            "2606:4700:4700::1111",
            "::ffff:1.1.1.1",
        ] {
            assert!(is_public_ip(ip(s)), "{s} must be allowed");
        }
    }

    #[test]
    fn endpoint_validation() {
        // Same cases as the prototype's UpstreamPolicyTest.
        for bad in [
            "http://api.example.com/v1",
            "https://user:pw@api.example.com/v1",
            "https://api.example.com/v1?key=x",
            "https://api.example.com/v1#frag",
            "ftp://x",
            "https:///v1",
            "https://",
            "not a url",
            "https://127.0.0.1/v1",
            "https://[::1]/v1",
            "https://169.254.169.254/latest",
            "https://10.0.0.1/v1",
        ] {
            assert!(validate_endpoint(bad, false).is_err(), "{bad}");
        }
        let u = validate_endpoint(" https://api.example.com/api/paas/v4/ ", false).unwrap();
        assert_eq!(u.host_str(), Some("api.example.com"));
        assert_eq!(u.path(), "/api/paas/v4/");
        // The test-only escape hatch allows the local fake upstream.
        validate_endpoint("http://127.0.0.1:9/v1", true).unwrap();
        assert_eq!(
            validate_endpoint("https://u@127.0.0.1/v1", true),
            Err(EndpointError::Credentials)
        );
    }
}
