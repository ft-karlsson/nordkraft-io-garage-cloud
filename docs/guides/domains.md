# Custom Domæner & HTTPS

!!! info "Kommer snart"
    Denne guide er under udarbejdelse.

## Quick start - Subdomain

```bash
# Aktiver HTTPS på subdomain
nordkraft ingress enable myapp --subdomain coolsite

# Resultat: https://coolsite.nordkraft.cloud
```

Automatisk TLS certifikat via Let's Encrypt.

## IPv6 Direct Access

```bash
# Åbn direkte IPv6 adgang
nordkraft ipv6 open myapp

# Resultat: http://[2a05:f6c3:444e::xxxx]/
```

Global IPv6 adresse uden NAT.
