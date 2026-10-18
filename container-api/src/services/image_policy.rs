//! Image source policy: which registries a tenant may deploy from.
//!
//! Image pulls run on the agent host, not inside the tenant's network, so the image
//! reference itself has to be checked. An image whose registry is an IP address must
//! lie inside the requesting tenant's own container subnet. Hostname registries
//! (docker.io, ghcr.io, a company registry) are allowed. Loopback and other literal
//! addresses are refused.
//!
//! Enforced in the controller's deploy/upgrade routes (before any destructive step)
//! and again in the agent's container manager.

use std::net::Ipv4Addr;

/// Validate that `image` may be pulled on behalf of a tenant whose container subnet
/// is `tenant_subnet` (e.g. "172.21.14.0/24").
pub fn validate_image_source(image: &str, tenant_subnet: &str) -> Result<(), String> {
    let Some(host_port) = image_host(image) else {
        // No registry component → Docker Hub by hostname. Allowed.
        return Ok(());
    };

    let (host, _port) = split_host_port(host_port);

    if host.eq_ignore_ascii_case("localhost") || host.starts_with('[') {
        return Err(format!(
            "Image registry '{host_port}' is not allowed. Use your own private registry \
             (registry://name:tag) or a public registry by hostname."
        ));
    }

    let Ok(ip) = host.parse::<Ipv4Addr>() else {
        // A DNS name. Allowed.
        return Ok(());
    };

    match ipv4_in_cidr(ip, tenant_subnet) {
        Ok(true) => Ok(()),
        Ok(false) => Err(format!(
            "Image registry '{host_port}' is outside your container subnet {tenant_subnet}. \
             Only your own private registry (registry://name:tag) or a public registry by \
             hostname is allowed."
        )),
        Err(e) => Err(format!("Cannot evaluate image policy: {e}")),
    }
}

/// The registry component of an image reference ("host" or "host:port"), if any.
///
/// Follows the Docker reference rule: the first path segment is a registry only when
/// it is followed by a '/' and contains a '.' or ':' or equals "localhost".
/// `nginx:alpine` and `myorg/app:v1` have no registry component.
pub fn image_host(image: &str) -> Option<&str> {
    let (first, _rest) = image.split_once('/')?;
    if first.contains('.') || first.contains(':') || first.eq_ignore_ascii_case("localhost") {
        Some(first)
    } else {
        None
    }
}

fn split_host_port(host_port: &str) -> (&str, Option<u16>) {
    // IPv6 literals are bracketed; everything else splits on the last ':'.
    if host_port.starts_with('[') {
        return (host_port, None);
    }
    match host_port.rsplit_once(':') {
        Some((host, port)) => (host, port.parse().ok()),
        None => (host_port, None),
    }
}

fn ipv4_in_cidr(ip: Ipv4Addr, cidr: &str) -> Result<bool, String> {
    let (net, prefix) = cidr
        .split_once('/')
        .ok_or_else(|| format!("subnet '{cidr}' is not in CIDR form"))?;
    let net: Ipv4Addr = net
        .parse()
        .map_err(|e| format!("subnet '{cidr}' has a bad network address: {e}"))?;
    let prefix: u32 = prefix
        .parse()
        .map_err(|e| format!("subnet '{cidr}' has a bad prefix length: {e}"))?;
    if prefix > 32 {
        return Err(format!("subnet '{cidr}' has a prefix longer than 32"));
    }
    let mask: u32 = if prefix == 0 {
        0
    } else {
        u32::MAX << (32 - prefix)
    };
    Ok((u32::from(ip) & mask) == (u32::from(net) & mask))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SUBNET: &str = "172.21.14.0/24";

    #[test]
    fn own_registry_is_allowed() {
        assert!(validate_image_source("172.21.14.3:5001/app:v1", SUBNET).is_ok());
        assert!(validate_image_source("172.21.14.250:5001/org/app@sha256:abc", SUBNET).is_ok());
    }

    #[test]
    fn other_tenant_registry_is_refused() {
        let err =
            validate_image_source("172.21.1.3:5001/garagecloud-dk:latest", SUBNET).unwrap_err();
        assert!(
            err.contains("outside your container subnet 172.21.14.0/24"),
            "{err}"
        );
        assert!(validate_image_source("172.21.15.3:5001/app", SUBNET).is_err());
    }

    #[test]
    fn lan_and_loopback_are_refused() {
        assert!(validate_image_source("10.0.0.200:5000/app", SUBNET).is_err());
        assert!(validate_image_source("172.20.0.254:8001/app", SUBNET).is_err());
        assert!(validate_image_source("127.0.0.1:5001/app", SUBNET).is_err());
        assert!(validate_image_source("localhost:5001/app", SUBNET).is_err());
        assert!(validate_image_source("[::1]:5001/app", SUBNET).is_err());
    }

    #[test]
    fn hostname_registries_are_allowed() {
        for image in [
            "nginx:alpine",
            "library/nginx",
            "myorg/app:v1",
            "docker.io/library/nginx:alpine",
            "ghcr.io/ft-karlsson/oci-registry:0.2.0",
            "registry.k8s.io/pause:3.9",
            "registry.gitlab.com:443/group/app:latest",
        ] {
            assert!(validate_image_source(image, SUBNET).is_ok(), "{image}");
        }
    }

    #[test]
    fn image_host_follows_docker_reference_rules() {
        assert_eq!(image_host("nginx:alpine"), None);
        assert_eq!(image_host("myorg/app:v1"), None);
        assert_eq!(image_host("172.21.14.3:5001/app"), Some("172.21.14.3:5001"));
        assert_eq!(image_host("ghcr.io/x/y"), Some("ghcr.io"));
        assert_eq!(image_host("localhost/x"), Some("localhost"));
    }

    #[test]
    fn bad_subnet_is_an_error_not_a_pass() {
        assert!(validate_image_source("172.21.14.3:5001/app", "not-a-cidr").is_err());
        assert!(validate_image_source("172.21.14.3:5001/app", "172.21.14.0/40").is_err());
    }
}
