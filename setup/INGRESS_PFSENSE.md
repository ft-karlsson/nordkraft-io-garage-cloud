# Ingress: pfSense + HAProxy + ACME

This guide sets up HTTPS ingress for NordKraft Garage Cloud using pfSense with the HAProxy and ACME packages. Ingress allows containers to be reached at public `https://myapp.yourdomain.com` URLs, with TLS terminated at the edge.

> **Tested on:** Netgate 8200 running pfSense CE/Plus. Any pfSense installation with the HAProxy and ACME packages installed should work, but only the Netgate setup has been verified with `container-api`.
>
> **Simpler alternative coming:** If you don't already run pfSense, wait for the Caddy ingress implementation in MARK II — it will handle the same routing without a separate appliance.

---

## How it works

```
Internet → pfSense (80/443) → HAProxy frontend
                                     ↓
                              Host: myapp.yourdomain.com
                                     ↓
                              HAProxy backend → 172.21.N.M:PORT (container IP)
```

`container-api` manages HAProxy backends and ACLs dynamically via the pfSense REST API v2. When you run `nordkraft ingress enable myapp --subdomain myapp`, the API:

1. Creates a HAProxy backend pointing at the container's IP
2. Creates an ACL matching the `Host:` header
3. Adds a routing rule in the shared HTTPS frontend
4. Creates a static route in pfSense so the container IP is reachable from pfSense

TLS is handled by a **wildcard certificate** (`*.yourdomain.com`) already bound to the frontend — no per-subdomain ACME issuance needed.

---

## Prerequisites

- pfSense with packages installed: `HAProxy`, `ACME`
- A domain with DNS pointing `*.yourdomain.com` → your public IP
- pfSense REST API v2 enabled (package or built-in on pfSense Plus 23.09+)
- Your containers are reachable from pfSense (WireGuard routing in place)

---

## Step 1: Issue wildcard certificate with ACME

In pfSense: **Services → ACME Certificates → Certificates → Add**

| Field | Value |
|-------|-------|
| Name | `wildcard-yourdomain` |
| Domain | `*.yourdomain.com` |
| Challenge type | DNS-01 (required for wildcard) |
| ACME server | Let's Encrypt Production |

Configure your DNS provider credentials for the DNS-01 challenge, then click **Issue/Renew**. The certificate will auto-renew.

> DNS-01 is required for wildcards — HTTP-01 won't work. Most major DNS providers are supported.

---

## Step 2: Create HAProxy frontends

You need two frontends: one for HTTP (port 80) and one for HTTPS (port 443). `container-api` expects these to exist before it can create routes.

In pfSense: **Services → HAProxy → Frontend → Add**

### HTTP frontend

| Field | Value |
|-------|-------|
| Name | `http_frontend` |
| Listen address | WAN address, port 80 |
| Type | `http / https (offloading)` |
| Default backend | (leave empty or set a catch-all) |

Add a shared action: redirect all HTTP to HTTPS (optional but recommended).

### HTTPS frontend

| Field | Value |
|-------|-------|
| Name | `https_frontend` |
| Listen address | WAN address, port 443 |
| Type | `http / https (offloading)` |
| SSL offloading | Enabled |
| Certificate | `wildcard-yourdomain` (the one you issued above) |
| Default backend | (leave empty) |

The exact frontend names (`http_frontend`, `https_frontend`) must match your environment variables below.

---

## Step 3: Enable pfSense REST API

On pfSense Plus 23.09+, the REST API is built-in. On CE, install the `pfSense-pkg-API` package.

**System → REST API** (or the package settings page):

- Enable the API
- Create an API key
- Note the key — it goes in `PFSENSE_API_KEY`

Test access:

```bash
curl -k -H "X-API-Key: your-key" https://your-pfsense-ip/api/v2/status/system
# Should return JSON with system info
```

---

## Step 4: Configure container-api

Add these environment variables to your `container-api` service:

```bash
# Enable ingress
export INGRESS_ENABLED=true
export INGRESS_BASE_DOMAIN=yourdomain.com
export INGRESS_PUBLIC_IP=your.public.ip

# pfSense REST API
export PFSENSE_API_URL=https://your-pfsense-ip
export PFSENSE_API_KEY=your-api-key

# Frontend names (must match what you created in Step 2)
export HAPROXY_HTTP_FRONTEND=http_frontend
export HAPROXY_HTTPS_FRONTEND=https_frontend
```

Restart `container-api`. On startup it will verify the HAProxy connection. Check logs:

```bash
journalctl -u nordkraft -n 50 | grep -E "(HAProxy|ingress|pfSense)"
# Should show: ✅ HAProxy client initialized for ingress: *.yourdomain.com
```

---

## Step 5: Test ingress

Deploy a container and enable ingress:

```bash
nordkraft deploy nginx:alpine --name test-ingress
nordkraft ingress enable test-ingress --subdomain test --port 80
```

Then visit `https://test.yourdomain.com`. You should see the nginx welcome page with a valid TLS certificate.

---

## Known limitations

**pfSense static route IDs are not persistent across reboots.** `container-api` handles this by always looking up routes by destination IP rather than stored ID — never by the ID returned at creation time. This is the correct behaviour; do not modify the route lookup logic.

**The pfSense API can be slow.** Creating an ingress route involves 3-4 sequential API calls. This is normal — the `nordkraft ingress enable` command will return in 2-5 seconds.

**Self-signed certificate warning on pfSense.** The `PFSENSE_API_URL` is accessed with TLS verification. If your pfSense uses a self-signed cert, you may need to disable verification in the HAProxy client config or install a trusted cert on pfSense.

**TCP ingress allocates ports from a pool.** The database table `ingress_port_pool` (populated by the schema) contains the available TCP port range. The default range is 10000–10999. Firewall rules for TCP ingress are created automatically via the pfSense API.

---

## Troubleshooting

### `nordkraft ingress enable` returns an error

Check `container-api` logs first. Common causes:
- pfSense API key incorrect or expired
- Frontend names don't match (`HAPROXY_HTTP_FRONTEND` / `HAPROXY_HTTPS_FRONTEND`)
- Container IP not routable from pfSense (missing static route or WireGuard routing issue)

### Certificate shows as invalid

The wildcard cert covers `*.yourdomain.com` — single level only. `sub.sub.yourdomain.com` won't be covered. All container subdomains are one level deep by design.

### Container unreachable after pfSense reboot

Static routes are recreated automatically on the next `nordkraft ingress enable` call. For existing routes, the reconciler does not currently re-create static routes after a pfSense reboot — this is a known gap. Workaround: disable and re-enable ingress for affected containers.
