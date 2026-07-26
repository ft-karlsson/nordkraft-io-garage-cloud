// src/services/domain_validation.rs
//
// Normalization + validation for customer-supplied custom domains.
//
// SECURITY: every domain passes through normalize_and_validate() BEFORE it
// touches the database, DNS lookups, ACME, or any pfSense object. The rules:
//   - lowercase + IDNA/punycode normalization (kills homograph tricks)
//   - reject IP literals (v4 and v6)
//   - reject bare public suffixes ("co.uk") and anything without a
//     registrable domain per the Public Suffix List
//   - reject the platform base domain and anything under it — a customer
//     must never claim "admin.<INGRESS_BASE_DOMAIN>" as a "custom" domain
//   - RFC 1035 label rules (length, charset, hyphen placement)
//
// pfSense object names are NEVER derived from the raw domain — see
// object_name_component() which produces a strictly [a-z0-9_] token that is
// always combined with the DB row id (cd_<id>_<token>) so two domains that
// sanitize identically ("a-b.com" vs "a.b.com") can never collide.

/// Maximum hostnames one could reasonably need; enforced per user via config.
pub const MAX_DOMAIN_LEN: usize = 253;

#[derive(Debug, PartialEq, Eq)]
pub enum DomainValidationError {
    Empty,
    TooLong,
    NotAscii(String),
    IpLiteral,
    NoRegistrableDomain,
    UnderPlatformDomain,
    BadLabel(String),
    WildcardNotAllowed,
}

impl std::fmt::Display for DomainValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => write!(f, "Domain is empty"),
            Self::TooLong => write!(f, "Domain exceeds {} characters", MAX_DOMAIN_LEN),
            Self::NotAscii(e) => write!(f, "Domain could not be punycode-normalized: {}", e),
            Self::IpLiteral => write!(f, "IP addresses are not allowed — use a DNS hostname"),
            Self::NoRegistrableDomain => write!(
                f,
                "Not a registrable domain (bare TLDs / public suffixes are not allowed)"
            ),
            Self::UnderPlatformDomain => write!(
                f,
                "This hostname is under the platform's own domain — use 'nordkraft ingress enable' for platform subdomains"
            ),
            Self::BadLabel(l) => write!(f, "Invalid DNS label: '{}'", l),
            Self::WildcardNotAllowed => write!(
                f,
                "Wildcard hostnames are not supported — add each hostname explicitly (exact-match only, by design)"
            ),
        }
    }
}

impl std::error::Error for DomainValidationError {}

/// Normalize a user-supplied domain and validate it for use as a custom domain.
///
/// Returns the normalized (lowercase, punycode) hostname on success.
/// `platform_base_domain` is INGRESS_BASE_DOMAIN — anything equal to it or
/// underneath it is rejected.
pub fn normalize_and_validate(
    raw: &str,
    platform_base_domain: &str,
) -> Result<String, DomainValidationError> {
    let trimmed = raw.trim().trim_end_matches('.');

    if trimmed.is_empty() {
        return Err(DomainValidationError::Empty);
    }
    if trimmed.contains('*') {
        return Err(DomainValidationError::WildcardNotAllowed);
    }
    // Reject IPv6 literals / anything with a colon or brackets before IDNA
    if trimmed.contains(':') || trimmed.contains('[') || trimmed.contains(']') {
        return Err(DomainValidationError::IpLiteral);
    }

    // IDNA (UTS-46) normalization: unicode → punycode, lowercasing included.
    let ascii = idna::domain_to_ascii(trimmed)
        .map_err(|e| DomainValidationError::NotAscii(format!("{:?}", e)))?;

    if ascii.len() > MAX_DOMAIN_LEN {
        return Err(DomainValidationError::TooLong);
    }

    // IPv4 literal check after normalization ("192.168.1.1")
    if ascii.parse::<std::net::Ipv4Addr>().is_ok() {
        return Err(DomainValidationError::IpLiteral);
    }

    // Per-label RFC 1035 checks (idna is lenient about hyphen placement)
    for label in ascii.split('.') {
        if label.is_empty()
            || label.len() > 63
            || label.starts_with('-')
            || label.ends_with('-')
            || !label
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        {
            return Err(DomainValidationError::BadLabel(label.to_string()));
        }
    }

    // Must have a registrable domain under the Public Suffix List.
    // Rejects bare TLDs ("com"), bare suffixes ("co.uk"), and unknown roots.
    match psl::domain_str(&ascii) {
        Some(registrable) if registrable.contains('.') => {}
        _ => return Err(DomainValidationError::NoRegistrableDomain),
    }

    // Never allow claiming the platform's own namespace.
    let base = platform_base_domain
        .trim()
        .trim_end_matches('.')
        .to_lowercase();
    if !base.is_empty() && (ascii == base || ascii.ends_with(&format!(".{}", base))) {
        return Err(DomainValidationError::UnderPlatformDomain);
    }

    Ok(ascii)
}

/// Sanitized token for pfSense object names. ALWAYS prefix with the DB id
/// (e.g. `cd_{id}_{token}`) — the token alone is not collision-free.
pub fn object_name_component(domain: &str) -> String {
    let mut out: String = domain
        .chars()
        .map(|c| {
            if c.is_ascii_lowercase() || c.is_ascii_digit() {
                c
            } else {
                '_'
            }
        })
        .collect();
    // pfSense object names have length limits; keep the component short.
    out.truncate(40);
    out
}

/// The DNS record name where the customer must publish the verification TXT.
pub fn challenge_record_name(domain: &str) -> String {
    format!("_nordkraft-challenge.{}", domain)
}

/// The TXT record value for a given verification token.
pub fn challenge_record_value(token: &str) -> String {
    format!("nk-verify={}", token)
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: &str = "nordkraft.cloud";

    #[test]
    fn accepts_normal_domains() {
        assert_eq!(
            normalize_and_validate("customer1.net", BASE).unwrap(),
            "customer1.net"
        );
        assert_eq!(
            normalize_and_validate("www.Customer1.NET", BASE).unwrap(),
            "www.customer1.net"
        );
        assert_eq!(
            normalize_and_validate("  app.example.co.uk. ", BASE).unwrap(),
            "app.example.co.uk"
        );
    }

    #[test]
    fn punycode_normalizes_unicode() {
        assert_eq!(
            normalize_and_validate("møllehøj.dk", BASE).unwrap(),
            "xn--mllehj-byae.dk"
        );
    }

    #[test]
    fn rejects_ip_literals() {
        assert_eq!(
            normalize_and_validate("192.168.1.1", BASE),
            Err(DomainValidationError::IpLiteral)
        );
        assert_eq!(
            normalize_and_validate("[2001:db8::1]", BASE),
            Err(DomainValidationError::IpLiteral)
        );
        assert_eq!(
            normalize_and_validate("2001:db8::1", BASE),
            Err(DomainValidationError::IpLiteral)
        );
    }

    #[test]
    fn rejects_public_suffixes_and_tlds() {
        assert_eq!(
            normalize_and_validate("com", BASE),
            Err(DomainValidationError::NoRegistrableDomain)
        );
        assert_eq!(
            normalize_and_validate("co.uk", BASE),
            Err(DomainValidationError::NoRegistrableDomain)
        );
    }

    #[test]
    fn rejects_platform_namespace() {
        assert_eq!(
            normalize_and_validate("nordkraft.cloud", BASE),
            Err(DomainValidationError::UnderPlatformDomain)
        );
        assert_eq!(
            normalize_and_validate("admin.nordkraft.cloud", BASE),
            Err(DomainValidationError::UnderPlatformDomain)
        );
        assert_eq!(
            normalize_and_validate("deep.sub.nordkraft.cloud", BASE),
            Err(DomainValidationError::UnderPlatformDomain)
        );
        // ...but a domain merely containing the base string is fine
        assert!(normalize_and_validate("notnordkraft.cloud", BASE).is_ok());
    }

    #[test]
    fn rejects_wildcards_and_bad_labels() {
        assert_eq!(
            normalize_and_validate("*.customer1.net", BASE),
            Err(DomainValidationError::WildcardNotAllowed)
        );
        assert!(matches!(
            normalize_and_validate("-bad.customer1.net", BASE),
            Err(DomainValidationError::BadLabel(_))
        ));
        assert!(matches!(
            normalize_and_validate("bad-.customer1.net", BASE),
            Err(DomainValidationError::BadLabel(_))
        ));
    }

    #[test]
    fn rejects_empty_and_too_long() {
        assert_eq!(
            normalize_and_validate("   ", BASE),
            Err(DomainValidationError::Empty)
        );
        let long = format!("{}.com", "a".repeat(300));
        assert!(normalize_and_validate(&long, BASE).is_err());
    }

    #[test]
    fn object_names_are_sanitized() {
        assert_eq!(object_name_component("customer1.net"), "customer1_net");
        assert_eq!(object_name_component("a-b.com"), "a_b_com");
        // collision with a.b.com is expected — the DB id prefix disambiguates
        assert_eq!(object_name_component("a.b.com"), "a_b_com");
    }

    #[test]
    fn challenge_record_helpers() {
        assert_eq!(
            challenge_record_name("customer1.net"),
            "_nordkraft-challenge.customer1.net"
        );
        assert_eq!(challenge_record_value("abc123"), "nk-verify=abc123");
    }
}
