# Custom Domains (bring your own domain)

Lets a customer point their own domain (e.g. `customer1.net`) at a container
running on NordKraft Garage Cloud, with automatic TLS. Builds on the pfSense +
HAProxy ingress from [INGRESS_PFSENSE.md](INGRESS_PFSENSE.md) — set that up
first.

> **Design goal: slow and 100 %, never fast and 95 %.** Everything
> failure-prone (DNS propagation, ACME, pfSense API) runs in a background
> reconciler with retries and read-back verification. `nordkraft domain add`
> returns instantly with DNS instructions; activation typically completes
> 5–30 minutes after the customer's DNS records propagate.

---

## How it works

```
nordkraft domain add customer1.net --container myapp
        │  (returns TXT + A record instructions immediately)
        ▼
 pending_dns ──► dns_verified ──► issuing_cert ──► cert_ready ──► activating ──► active
      ▲                                                                            │
      │   2 consecutive checks against the                        read-back        │
      │   domain's AUTHORITATIVE nameservers                      verified         │
      │                                                                            ▼
      └── customer publishes:                                        daily re-check:
          _nordkraft-challenge.customer1.net TXT "nk-verify=<token>"  - DNS still points here?
          customer1.net                      A   <INGRESS_PUBLIC_IP>  - cert renewal (30 d before expiry)
                                                                      - pfSense object drift repair
                                                                      - container IP drift repair
```

Traffic path once active:

```
Internet → pfSense :443 → https_frontend (SNI picks the customer1.net cert)
        → exact-match Host ACL → dedicated backend → 172.21.x.y:port
```

**Isolation guarantees** (same philosophy as platform ingress, enforced by
construction):

- Host ACLs are **exact match only** — never wildcards. No overlap between
  tenants is possible.
- **No routing object exists before verification**: the ACL/backend are only
  created after the TXT token verifies against authoritative DNS *and* Let's
  Encrypt has independently validated the domain via HTTP-01.
- `UNIQUE(domain)` in the database — one owner per hostname, ever.
- pfSense object names derive from the DB row id (`cd_<id>_<sanitized>`),
  never from raw user input.
- Domains equal to or under `INGRESS_BASE_DOMAIN` are rejected, as are IP
  literals, bare public suffixes (`co.uk`), wildcards, and non-normalized
  unicode (IDNA/punycode is applied first).
- If DNS stops pointing at the platform, the domain goes `degraded`; after
  `CUSTOM_DOMAINS_GRACE_DAYS` (default 7) routing and certificate are torn
  down automatically, so a future owner of the domain can never receive the
  old tenant's traffic.

**TLS:** the controller runs its own ACME client (`instant-acme`, HTTP-01)
and pushes issued certificates into the pfSense certificate store, bound to
the HTTPS frontend via SNI. One state machine, in one database — the pfSense
ACME package is NOT used for custom domains. Private keys are generated per
order and exist only in transit to pfSense; the database stores only the cert
refid + expiry.

---

## Prerequisites

1. Working platform ingress per [INGRESS_PFSENSE.md](INGRESS_PFSENSE.md)
   (frontends, wildcard cert, REST API v2).
2. **Spike checklist below verified once on your pfSense box** (two endpoint
   shapes could differ between REST API package versions).
3. Port 80 open — Let's Encrypt HTTP-01 validation arrives there.

## Spike checklist (run once before first deploy)

The custom-domain code uses two pfSense REST API v2 surfaces that were not
exercised by the existing ingress code. Verify both against your Netgate
(replace key/host):

```bash
# 1. Certificate upload — expect 200 and a "refid" in data
curl -sk -H "X-API-Key: $KEY" -H "Content-Type: application/json" \
  -X POST https://pfsense/api/v2/system/certificate \
  -d '{"descr":"nk-spike-test","crt":"'$(base64 -w0 test-cert.pem)'","prv":"'$(base64 -w0 test-key.pem)'"}'

# (generate a throwaway pair first:
#  openssl req -x509 -newkey rsa:2048 -keyout test-key.pem -out test-cert.pem -days 1 -nodes -subj "/CN=spike.test")

# 2. Frontend certificate child object — check the field name.
#    Look at your HTTPS frontend and find how "Additional certificates" appear:
curl -sk -H "X-API-Key: $KEY" \
  "https://pfsense/api/v2/services/haproxy/frontends" | python3 -m json.tool | less
# → find your https_frontend object; note the array holding extra certs
#   (expected: "ha_certificates" with entries carrying "ssl_certificate": "<refid>")

# 3. Try binding the spike cert:
curl -sk -H "X-API-Key: $KEY" -H "Content-Type: application/json" \
  -X POST https://pfsense/api/v2/services/haproxy/frontend/certificate \
  -d '{"parent_id": <https_frontend_id>, "ssl_certificate": "<refid-from-step-1>"}'

# 4. Clean up the spike cert afterwards (find id via /api/v2/system/certificates).
```

If step 2/3 show a different endpoint or field name, adjust the two constants
`FRONTEND_CERT_ENDPOINT` and `FRONTEND_CERT_FIELD` at the top of the
custom-domain section in `container-api/src/services/haproxy_client.rs` —
nothing else references them.

---

## Setup

### 1. Apply the database migration

```bash
psql -U garage_user -d garage_cloud -f setup/migration_custom_domains.sql
```

### 2. Configure container-api

```bash
# Enable the feature (requires INGRESS_ENABLED=true)
export CUSTOM_DOMAINS_ENABLED=true

# ACME / Let's Encrypt
export ACME_CONTACT_EMAIL=you@example.dk   # required
export ACME_STAGING=false                  # true = LE staging CA (testing; untrusted certs)

# Where HAProxy forwards ACME HTTP-01 challenges: container-api's listener,
# reachable FROM pfSense (controller LAN/VPN IP + BIND_PORT).
# Default: CONTROLLER_INTERNAL_IP:BIND_PORT
export CUSTOM_DOMAINS_CHALLENGE_ADDR=10.0.0.200:8001

# Optional tuning (defaults shown)
export CUSTOM_DOMAINS_MAX_PER_USER=5
export CUSTOM_DOMAINS_RECONCILE_INTERVAL=60      # seconds between reconciler passes
export CUSTOM_DOMAINS_GRACE_DAYS=7               # degraded → auto-teardown
export CUSTOM_DOMAINS_RENEW_BEFORE_DAYS=30       # cert renewal lead time
export CUSTOM_DOMAINS_RECHECK_SECONDS=86400      # steady-state health re-check
```

Restart `container-api`. On startup (controller/hybrid) it idempotently
creates the shared ACME challenge plumbing on HAProxy:

- backend `nk_acme_challenge` → `CUSTOM_DOMAINS_CHALLENGE_ADDR`
- path ACL `nk_acme_path` (`path_starts_with /.well-known/acme-challenge/`)
  + `use_backend` action on **both** frontends (both, because an HTTP→HTTPS
  redirect makes Let's Encrypt retry the challenge on 443).

Check logs:

```bash
journalctl -u nordkraft -n 50 | grep -E "(Custom domains|ACME|Domain reconciler)"
# Expect: "🌍 Custom domains enabled …", "✅ ACME challenge routing bootstrapped",
#         "🌍 Domain reconciler started …"
```

> **Note:** `BIND_ADDRESS` must make the API reachable from pfSense (not
> `127.0.0.1`) for the challenge forwarding to work. The API itself remains
> WireGuard-only for authenticated endpoints; the only route exposed through
> HAProxy is `/.well-known/acme-challenge/<token>`, which serves in-memory
> tokens that exist only while an order is in flight.

### 3. Customer flow

```bash
# Customer registers their domain
nordkraft domain add customer1.net --container myapp
# → prints the TXT + A records to add at their DNS provider

# Customer checks progress any time
nordkraft domain status customer1.net
nordkraft domain verify customer1.net    # force a re-check right now

# When status = active:
# https://customer1.net → myapp, valid Let's Encrypt certificate
```

Apex domains use an A record; subdomains (`www.customer1.net`) may instead
CNAME to any platform hostname. `www` and the apex are separate hostnames —
add each explicitly (exact-match only, by design).

---

## Operational notes

**Everything is retried with backoff.** DNS checks run every reconciler pass;
ACME failures back off 15 min → 6 h (Let's Encrypt allows 5 validation
failures per hostname per hour — the backoff keeps you far under). `last_error`
on the row (shown by `nordkraft domain status`) always says what's blocking.

**'active' is never optimistic.** A domain only reaches `active` after the
backend, ACL, action, and cert binding have been read back from pfSense.
The daily steady-state pass re-verifies all of it and repairs drift —
including pfSense reboots losing static routes and container redeploys
changing IPs.

**Renewals** happen 30 days before expiry: new cert issued and bound *before*
the old one is unbound (no TLS gap), then the old one is deleted.

**Rate limits:** Let's Encrypt production allows 50 certs/registered domain
/week — irrelevant at our volumes, but use `ACME_STAGING=true` when testing
repeatedly against the same domain.

**Removal** (`nordkraft domain remove`) tears down in reverse order:
action → ACL → backend → cert unbind → cert delete → static route (only if no
other route still uses the same container IP).

## Troubleshooting

### Stuck in pending_dns
`nordkraft domain verify <domain>` shows which of the two records is missing.
Remember the TXT check runs against the domain's **authoritative**
nameservers — a propagated-looking record in your local resolver cache is not
enough, and conversely you don't have to wait for full global propagation.

### issuing_cert fails repeatedly
Let's Encrypt could not fetch the challenge. Verify port 80 reaches HAProxy,
and the challenge routing exists (Services → HAProxy → Frontend: the
`nk_acme_path` ACL) and points at a reachable `CUSTOM_DOMAINS_CHALLENGE_ADDR`.
Test from outside:
`curl http://customer1.net/.well-known/acme-challenge/test` → expect 404 from
container-api (not a pfSense error page).

### Domain shows degraded
The domain stopped resolving to `INGRESS_PUBLIC_IP`. Restore the A/CNAME
record within the grace period and it re-activates automatically.
