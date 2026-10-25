# Child peers (sub-users with scoped WireGuard access)

> Status: **phases 1 and 3 implemented** (reconciler + generated ruleset,
> `/api/peers`, `nordkraft peer`), 2026-10-25. Read
> [SETUP_WIREGUARD.md](SETUP_WIREGUARD.md) first.

A customer (the **owner**) can hand out extra WireGuard peers to people and
devices — a colleague's laptop, a phone, a CI runner, a Raspberry Pi in the
living room. Each child peer reaches **only** the owner's containers, and
optionally only *some* of them, on *some* ports. A child peer can **never**
reach the container API, the controller host, other tenants, or other peers.

A child peer is a network identity, not an account. It has no API access, no
CLI, no `nordkraft` commands. If sub-users later need to *manage* containers
(deploy, logs, exec), that is a separate feature (delegated API identities)
and is deliberately out of scope here.

---

## 1. What exists today (and what the design builds on)

```
owner laptop ── wg ──► controller (wg0, 172.20.0.254/16) ── LAN ──► agent node ── macvlan ──► 172.21.S.x
172.20.0.S/32                 │ nft forward chain                                 (containers of slot S)
                              └─ container-api :8001 bound on 172.20.0.254
```

| Thing | Today | Relevant because |
|---|---|---|
| Peer per account | one, `172.20.0.S/32` where S = `users.user_slot` (1–255) | identity is the socket IP → peer cache → `users.wireguard_public_key` |
| Server-side `allowed-ips` | `172.20.0.S/32, 172.21.S.0/24` | the `172.21.S.0/24` entry is legacy and does nothing useful; **do not copy it** to child peers |
| Client `AllowedIPs` | `172.20.0.254/32, 172.21.S.0/24` (from the signup backend's claim response) | child configs must not contain `172.20.0.254/32` |
| nft | `inet filter forward`, **policy drop** on production: one `accept` per tenant, `ct state established,related accept`, explicit drops | a child's packets would hit that drop too, so the generated ruleset has to own this chain, not sit beside it (§5) |
| Convergence | was `boot_recovery_from_db` + `WgReconciler` (5 min, add-only) | both one-way; replaced by the `PeerReconciler` (§5) |
| Agent nodes | return route `172.20.0.0/16 via controller`; no filtering | already covers the child address range, so **no agent-side work** |
| API auth | `AuthenticatedUser`: IP → pubkey → `users` row, else 401 | a child pubkey is never in `users`, so it already fails closed; §4 adds explicit layers |

---

## 2. Addressing

The VPN range is `172.20.0.0/16`. Owners live in `172.20.0.0/24`. Everything
else in the /16 is unused, and slots are capped at 255. So:

```
172.20.0.S       owner of slot S (unchanged)
172.20.S.N       child peer N of slot S, N = 2..254   (up to 253 per tenant)
172.21.S.0/24    containers of slot S (unchanged)
```

Why this shape:

- `ip saddr 172.20.S.0/24` means "every child of tenant S" in one nft match.
- `172.20.1.0-172.20.255.255` means "every child of every tenant" — one rule
  keeps all of them off the controller host.
- No new allocation table: `(user_slot, peer_index)` is the address.
- The controller's `wg0` is `172.20.0.254/16` and agents route
  `172.20.0.0/16` back to the controller, so return traffic already works.
  `SETUP_WIREGUARD.md` shows `172.20.0.1/24` in its hand-written example —
  that must become `/16` or children get no return path.
- Freed slots follow the existing 30-day cool-off (account deletion design), so
  a child range is never re-issued while a stale client config may still exist.

Child peers are IPv4-only inside the tunnel, like owners.

---

## 3. Data model

```sql
CREATE TABLE tenant_peers (
    id              VARCHAR(36)  PRIMARY KEY,
    user_id         VARCHAR(36)  NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    name            VARCHAR(63)  NOT NULL,           -- "anna-laptop", "ci-runner"
    peer_index      SMALLINT     NOT NULL CHECK (peer_index BETWEEN 2 AND 254),
    public_key      TEXT         NOT NULL UNIQUE,    -- WireGuard key, never an invite placeholder
    scope           VARCHAR(20)  NOT NULL CHECK (scope IN ('all', 'containers')),
    status          VARCHAR(20)  NOT NULL DEFAULT 'active' CHECK (status IN ('active', 'disabled')),
    created_at      TIMESTAMPTZ  NOT NULL DEFAULT now(),
    updated_at      TIMESTAMPTZ  NOT NULL DEFAULT now(),
    UNIQUE (user_id, name),
    UNIQUE (user_id, peer_index)
);

-- Only read when scope = 'containers'. Keyed by container NAME on purpose:
-- the reconciler resolves name → containers.internal_ip on every run, so a
-- redeploy that changes the IP, or a new container with the same name,
-- keeps the child's access without anyone re-issuing anything.
CREATE TABLE tenant_peer_targets (
    peer_id         VARCHAR(36)  NOT NULL REFERENCES tenant_peers(id) ON DELETE CASCADE,
    container_name  VARCHAR(100) NOT NULL,
    ports           INTEGER[]    NULL,               -- NULL = every port; else tcp dports
    PRIMARY KEY (peer_id, container_name)
);

ALTER TABLE plans ADD COLUMN max_peers INTEGER NOT NULL DEFAULT 5;
```

Rules:

- **Private keys are never stored or seen by the server.** Either the owner's
  CLI generates the keypair locally and uploads only the public key, or the
  sub-user generates their own and hands the owner the public key.
- `scope = 'all'` ⇒ the child may reach `172.21.S.0/24` entirely.
  `scope = 'containers'` ⇒ exactly the rows in `tenant_peer_targets`, resolved
  to `/32`s. A target whose container does not currently exist yields no rule
  (and shows as *dangling* in `nordkraft peer show`).
- `users` rows are never created for children. The `AuthenticatedUser` lookup
  stays `WHERE wireguard_public_key = $1` against `users` only.
- `ON DELETE CASCADE` from `users` means a tenant purge drops the rows and the
  next reconcile removes the peers and rules (§5). `tenant_purge.rs` should list
  child peers in its inventory so the dry run shows them.

---

## 4. The invariant: a child never reaches the API

Enforced in three independent layers; any one of them alone is sufficient.

1. **nftables, input hook (the real one).** On the controller:
   `iifname "wg0" ip saddr 172.20.1.0-172.20.255.255 counter drop`.
   Children cannot talk to the controller host at all — not :8001, not SSH,
   not NATS :4222, not ICMP. Only *forwarded* traffic is ever considered.
2. **Application guard.** `ClientIP` outside `172.20.0.0/24` is refused in
   `AuthenticatedUser` before any lookup, with a distinct log line
   (`child peer <ip> tried the API`). Plus the existing fail-closed path: the
   pubkey is not in `users` → 401.
3. **Client config.** The generated child `.conf` lists only
   `172.21.S.0/24` in `AllowedIPs`; `172.20.0.254` is not routed into the
   tunnel. This is convenience, not security — the owner could edit it — which
   is why layers 1 and 2 exist.

Why nft `ip saddr` matching is trustworthy here: WireGuard's cryptokey routing
drops any packet from a peer whose source address is not in that peer's
`allowed-ips`. A child provisioned with `allowed-ips 172.20.S.N/32` cannot
spoof `172.20.0.S`, `172.20.0.254`, or anything else.

---

## 5. Convergence: one declarative reconciler

The add-only `WgReconciler` + `boot_recovery_from_db` + the grep-for-handle
`nft insert/delete` helpers are replaced by one two-way `PeerReconciler`
(`container-api/src/services/peer_reconciler.rs`, the reconciliation model
from the account-deletion design):

```
desired  = owners (users: active, slot set, real key)
         ∪ children (tenant_peers: status active, owner active)
observed = wg show wg0 dump
```

**WireGuard:** `wg set wg0 peer <key> allowed-ips <ip>/32` for every desired
peer whose key is missing or whose allowed-ips differ; `wg set wg0 peer <key>
remove` for every observed peer not in `desired`. (Removal is new. It is the
correct behaviour; a manually added test peer will disappear within one
interval, and that is a feature.)

**nftables:** generate the *complete* desired ruleset from the DB and apply it
atomically with `nft -f`. No incremental inserts, no handle grepping, no
"does this rule already exist" string matching. The golden copies live in
`tests/snapshots/nft-ruleset-{drop,audit}.nft` (CI dry-runs them through the
real `nft -c`); `GET /api/admin/tenants/ruleset` renders the live one.

```
table inet filter {                       # create if missing, (re)set the policy
	chain forward { type filter hook forward priority filter; policy drop; }
	chain nordkraft_input { type filter hook input priority -10; policy accept; }
}
flush chain inet filter forward           # same file → one transaction
flush chain inet filter nordkraft_input

table inet filter {
	chain forward {
		ct state established,related accept
		# owners
		iifname "wg0" ip saddr 172.20.0.5  ip daddr 172.21.5.0/24  accept comment "tenant-5"
		iifname "wg0" ip saddr 172.20.0.12 ip daddr 172.21.12.0/24 accept comment "tenant-12"
		# child peers
		iifname "wg0" ip saddr 172.20.5.2 ip daddr 172.21.5.0/24 accept comment "peer-5-2 anna-laptop all"
		iifname "wg0" ip saddr 172.20.5.3 ip daddr 172.21.5.20   accept comment "peer-5-3 ci-runner db"
		iifname "wg0" ip saddr 172.20.5.3 ip daddr 172.21.5.17 tcp dport { 80, 443 } accept comment "peer-5-3 ci-runner web"
		# anything else arriving from the VPN
		iifname "wg0" counter drop comment "vpn-default-deny"
	}
	chain nordkraft_input {
		iifname "wg0" ip saddr 172.20.1.0-172.20.255.255 counter drop comment "children-never-reach-host"
	}
}
```

Notes on the ruleset:

- **The controller owns `inet filter forward`.** It is the chain the installer
  creates and production has carried by hand; because it ends in `drop`, a
  child's traffic would die there whatever a second table said. So the
  reconciler rewrites that chain (and its own `nordkraft_input`) and leaves
  everything else in the table alone. A rule added to either chain by hand is
  gone within one interval: put it in another chain.
- **Deny mode.** `VPN_DENY_MODE=drop` (default; what production already ran)
  gives `policy drop` and a final `counter drop`. `VPN_DENY_MODE=audit` is for
  a self-hosted node migrating from the installer's old accept-by-default
  chain: `policy accept`, final rule `counter log prefix "nk-vpn-deny "` only,
  so the journal shows what a flip to `drop` would cut.
- **Container → peer initiated traffic is not allowed.** Only replies
  (`established,related`) flow back. This is deliberate and can be relaxed per
  peer later if someone needs a container to call home.
- **Children cannot reach each other or the owner** over the hub: there is no
  accept with a `172.20.x.x` destination.
- **A target outside the owner's subnet never becomes a rule**, even if a bad
  row says so; the renderer refuses it with a comment in the ruleset.
- **The container subnet comes from `garage_container_allocations`**, not from
  the slot number. Today every row is `172.21.<slot>.0/24`; a garage that
  allocates `/20`s per tenant needs no change in this feature. Child addresses
  stay `172.20.<slot>.<n>` whatever the container range is. A tenant without an
  allocation row falls back to the `/24` with a warning.

**Triggers:** boot, every 5 minutes, and a `tokio::sync::Notify` kick
(`peer_reconciler::kick()`) from every write path that changes the desired
state: container deploy (after `track_container_deployment` assigns the IP),
container delete (controller and reaper), tenant purge, and in phase 3 the
peer endpoints. Adding a peer takes effect in well under a second, not in five
minutes. `POST /admin/tenant/provision` runs a pass synchronously and returns
its report.

**Side effects worth knowing:** a suspended owner loses the VPN peer within
one pass (today the peer lingered until deprovision). A WireGuard peer the DB
does not know is removed; `WG_KEEP_STRAY_PEERS=true` keeps them and logs
instead.

**Generator is a pure function** `render_ruleset(&DesiredState, iface, mode)`
with golden-file tests (`UPDATE_SNAPSHOTS=1 cargo test` rewrites them).
Ordering is deterministic (slot, peer_index, container name) so diffs are
readable.

**Dev mode** (`--dev`, no WireGuard): the DB and API work, the reconciler
logs the ruleset it would apply and does nothing else. This keeps the
`e2e-dev` CI job meaningful for the API layer.

---

## 6. API (owner-authenticated over WireGuard, like everything else)

| Endpoint | Body / result |
|---|---|
| `POST /api/peers` | `{name, public_key, scope, targets:[{container, ports?}]}` → `{name, vpn_ip, server_public_key, server_endpoint, allowed_ips, scope, targets}` |
| `GET /api/peers` | list with `vpn_ip`, `scope`, `status`, `last_handshake`, `transfer_rx/tx` (from the existing peer cache), `dangling` targets |
| `GET /api/peers/<name>` | one peer, same shape |
| `PATCH /api/peers/<name>` | `scope`, `targets` (replace), `add_targets`, `remove_targets` |
| `POST /api/peers/<name>/enable` · `/disable` | `disabled` ⇒ removed from WireGuard and nft, row kept |
| `POST /api/peers/<name>/rotate` | `{public_key}` → same `vpn_ip`, new key |
| `DELETE /api/peers/<name>` | row gone, next reconcile removes peer + rules |

Validation on every write: `name` is `[a-z0-9-]{1,63}`; `public_key` passes
`is_wireguard_key` (reuse from `tenant_purge.rs`); every `targets[].container`
must be a non-deleted container **of this user** (`containers.user_id`);
`count(peers) < plans.max_peers`; `peer_index` is the lowest free in 2..254
for the user. Server endpoint and public key come from `garages` for the
user's `primary_garage_id`, exactly as the signup backend does on claim.

The signup backend (`nordkraft-cloud-backend`) is **not involved**: children
are provisioned by the controller directly, because the controller already
owns WireGuard and nft and already authenticates the owner.

---

## 7. CLI

```
nordkraft peer add anna-laptop --all                        # keypair generated locally, config written
nordkraft peer add ci-runner --container web --ports 80,443 --container db
nordkraft peer add phone --all --qr                         # QR for the WireGuard app (qrencode if present)
nordkraft peer add anna-laptop --pubkey 'K7f…='             # sub-user made their own key; no private key anywhere
nordkraft peer ls
nordkraft peer show ci-runner                               # scope, targets (+ current IPs), last handshake, dangling
nordkraft peer allow ci-runner --container cache            # add target
nordkraft peer revoke ci-runner --container db
nordkraft peer set ci-runner --all | --only web,db
nordkraft peer disable|enable ci-runner
nordkraft peer rotate ci-runner [--pubkey …]
nordkraft peer rm ci-runner
```

`peer add` writes `~/.config/nordkraft/peers/<name>.conf` (mode 0600) and
prints it once. The generated config:

```
[Interface]
PrivateKey = <generated locally, or omitted when --pubkey was used>
Address = 172.20.5.3/32

[Peer]
PublicKey = <garage server key>
Endpoint = <garage endpoint>
AllowedIPs = 172.21.5.0/24
PersistentKeepalive = 25

# Reachable from this peer (enforced by Nordkraft, not by this file):
#   web  172.21.5.17  tcp 80,443
#   db   172.21.5.20
```

`AllowedIPs` is **always** the owner's whole container /24, regardless of
scope. Scope changes therefore never require re-issuing the config; nft on the
controller is the only thing that changes. No `DNS =` line (see
`SETUP_WIREGUARD.md`).

---

## 8. The installer's chain had no deny; production did

Checked on rasp2 2026-10-25: the live `inet filter forward` chain has
`policy drop`, the per-tenant accepts, `ct state established,related accept`
and explicit drops towards `10.0.0.0/8` and `172.21.0.0/16`. Tenants were
isolated. `scripts/install-node.sh`, however, created the chain without a
policy (so: accept), and `provision_tenant`'s accepts restricted nothing on a
fresh self-hosted node. Both are now fixed the same way: the installer's
`PostUp` sets `policy drop`, and the reconciler re-asserts the policy and
every rule from the DB on each pass. A self-hoster upgrading a node that ran
with the old accept chain can start with `VPN_DENY_MODE=audit` and read the
`nk-vpn-deny` journal lines before switching to `drop`.

Related: NATS listens on `0.0.0.0:4222` without auth on the controller, but
rasp2's hand-written `inet filter input` chain (policy drop) only accepts
4222 from `10.0.0.0/24`, and the API only from `172.20.0.0/24` to
`172.20.0.254:8001`. So owners cannot reach NATS over wg0, and that input
chain is a fourth layer in front of children. The reconciler leaves it alone.

Deployed 2026-10-25 (v0.3.48): first pass on rasp2 rendered 8 owners, 0
children, changed no WireGuard peer, applied the ruleset; the
`vpn-default-deny` counter stayed at 0 and owner traffic to containers kept
working.

---

## 9. Phases

1. **Done — reconciler + ruleset** (controller only, no new API): migration
   `setup/migration_child_peers.sql` (applied on every start),
   `render_ruleset` with golden tests, `PeerReconciler` replacing
   `WgReconciler`/boot recovery/`nft_*_tenant_rule`, `VPN_DENY_MODE`,
   child peers in the purge inventory, the auth guard refusing child-range
   IPs. With zero children the first pass on rasp2 rewrites the forward chain
   to the same accepts it has today (plus `ct` first), adds
   `nordkraft_input`, and removes any WireGuard peer not in `users`.
   **Before deploying:** `wg show wg0 peers` against `users` to see which
   peers that would be; `WG_KEEP_STRAY_PEERS=true` if unsure.
2. **Verify on rasp2:** `nft list table inet filter`, the `last_reconcile`
   field of `GET /api/admin/tenants/status`, owners still reach their
   containers, `journalctl -u container-api | grep "peer reconciler"`.
3. **Done — API + CLI** (§6, §7): `container-api/src/routes/peers.rs`,
   `nordkraft peer add|ls|show|allow|revoke|set|disable|enable|rotate|rm`.
   Every write runs one reconcile pass before answering. `plans.max_peers`
   is enforced only where `QUOTA_ENFORCED=true`, like the other quotas.
   `scripts/smoke-dev.sh` step 4c exercises the whole set on the dev node
   (rows only: dev mode has no WireGuard).
4. **Docs**: a Danish guide `docs/guides/peers.md` ("Giv andre adgang til dine
   containere"), rows in `docs/reference/api.md` and `docs/reference/cli.md`,
   fix the `/24` in `SETUP_WIREGUARD.md`.
5. Later, if asked: per-peer "container may call home", IPv6 inside the
   tunnel, delegated API identities for sub-users who should deploy.

## 10. Acceptance checks (manual, on production after phase 3)

- Owner adds `phone --all`; the phone reaches `http://172.21.S.x` for every
  container; `curl 172.20.0.254:8001/api/status` from the phone times out;
  `nordkraft ps` from the phone fails with "cannot connect".
- Owner adds `ci --container web --ports 80`; CI reaches `web:80`, not
  `web:22`, not `db`, not a container of another tenant.
- `nordkraft peer disable phone` ⇒ handshake stops within seconds;
  `wg show wg0` no longer lists the key; `enable` restores it.
- Redeploy `web` so its IP changes ⇒ CI still reaches it after the next
  reconcile kick (sub-second).
- Admin purge of the owner (dry run) lists the peers; the real purge leaves no
  `172.20.S.*` peer in `wg show` and no `peer-S-*` rule.
- `journalctl -u container-api | grep nk-vpn-deny` shows the drops, and the
  `counter` on the default-deny rule moves when a child probes the API.
