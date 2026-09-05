# API Reference

!!! info "Kommer snart"
    Denne side er under udarbejdelse.

Se [CLI Reference](cli.md) for kommandolinje-dokumentation.

---

## Quick overview

API'en er tilgængelig på `http://172.20.0.254:8001/api` via WireGuard VPN.

### Authentication

Alle requests autentificeres via din WireGuard VPN-forbindelse. Ingen API keys nødvendige.

### Endpoints

| Endpoint | Metode | Beskrivelse |
|----------|--------|-------------|
| `/containers/deploy` | POST | Deploy ny container |
| `/containers` | GET | List containere |
| `/containers/{id}/stop` | POST | Stop container |
| `/containers/{id}/start` | POST | Start container |
| `/containers/{id}` | DELETE | Slet container |
| `/containers/{id}/logs` | GET | Hent logs |
| `/auth/verify` | GET | Verificer auth |

Fuld dokumentation kommer snart.
