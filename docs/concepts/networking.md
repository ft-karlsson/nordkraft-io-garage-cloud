# Netværk

!!! info "Kommer snart"
    Denne guide er under udarbejdelse.

## Netværksoversigt

```
Internet
    │
    ├── IPv4: ingress via HAProxy (*.nordkraft.cloud)
    │
    └── IPv6: direkte adgang (2a05:f6c3:444e::/64)

WireGuard VPN (172.20.0.0/16)
    │
    └── 172.20.0.254 → API Server (controller)
            │
            └── 172.21.x.0/24 → Dine containere
```

## IP-allokering

Hver bruger får:

- **VPN IP:** 172.20.1.x (din identitet)
- **Container subnet:** 172.21.x.0/24 (dine containere)
- **IPv6 range:** Adresser fra vores /64 prefix

## Adgang til containere

### Via VPN (privat)
```bash
curl http://172.21.5.15
```

### Via HTTPS (offentlig)
```bash
nordkraft ingress enable myapp --subdomain test
# https://test.nordkraft.cloud
```

### Via IPv6 (offentlig)
```bash
nordkraft ipv6 open myapp
# http://[2a05:f6c3:444e::xxxx]/
```
