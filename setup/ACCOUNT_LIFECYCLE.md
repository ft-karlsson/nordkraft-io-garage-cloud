# Account lifecycle: welcome mail, token-as-proof activation, self-service deletion

How a customer gets in, proves the address is theirs, and gets out again.
Three services take part: the **signup backend** (`cloud.nordkraft.io`, repo
`nordkraft-cloud-backend`, owns SMTP and the customer mails), the
**controller** (`container-api`, owns the tenant and the purge) and the
**CLI**. They share one Postgres database, `garage_cloud`.

Tables added by `setup/migration_account_lifecycle.sql` (applied by the
controller on every start, like the other migrations):

| table | one row per | survives the users row |
| --- | --- | --- |
| `email_verifications` | deletion code sent to an address (`purpose` = `delete`; `activate` is reserved), SHA-256 of the code, attempts, expiry | no |
| `account_deletions` | request to leave: requested, scheduled, cancelled, started, completed, attempts, last error | **yes** (that is the point) |

Codes are six digits, valid 30 minutes, five guesses, one per minute. Only
`sha256(id ":" code)` is stored (`container-api/src/services/verification.rs`).

## 1. Signup → the one mail

`POST /api/signup` creates the users row with the `NKINVITE-` token as before
and mails the customer the install one-liner and `nordkraft setup NKINVITE-…`.
**The token goes out by mail only.** The success page does not show it; it
says "check your inbox" and has a resend button (`POST /api/signup/<id>/resend`,
60 s cooldown, only while the token is unclaimed). The send is awaited, so the
page knows whether the mail left (`mail_sent`); with SMTP off (dev) the
response carries the command and the token is printed to the log.

Being able to run the command therefore proves the address: the token is a
random UUID, it works once, and nobody else has seen it. That is the whole
email verification, and the terminal never asks for anything.

## 2. Claim → active

```
nordkraft setup NKINVITE-…
   │ POST /api/claim {token, wireguard_public_key}
   ▼
users.wireguard_public_key = key, account_status = 'active'
container-api /admin/tenant/provision (peer + nftables)
founder notification "Kunde aktiveret"
CLI: write wg.conf, wg-quick up, verify, done.
```

No prompt, no second mail, no pending state. This is the flow as it was
before October 2026, with the token moved off the web page.

`scripts/install.sh` runs setup with stdin on `/dev/tty` when it is invoked
through `curl … | sh`, so any prompt a future setup might have, and sudo's,
still reach the terminal.

## 3. Leaving

```
nordkraft account delete [--now]
   │ POST /api/account/deletion/request          (WireGuard-authenticated)
   ▼
email_verifications (delete) + mail "Bekræft sletning af din konto" (code)
   │ sent by the backend: container-api → POST /api/internal/mail (admin key)
   ▼
   Code: ______
   │ POST /api/account/deletion/confirm {verification_id, code, immediate}
   ▼
account_deletions {scheduled_for}
   default:   03:00 Europe/Copenhagen, ACCOUNT_DELETION_GRACE_DAYS days ahead
              mail "Din konto slettes den 11. november 2026"
   --now:     scheduled_for = now; no "slettes den" mail (the last one follows
              in minutes); the runner is kicked after 10 s
founder note "Sletning planlagt" / "Konto slettes nu"  (push + mail)
users row unchanged: VPN, containers, everything keeps working

   nordkraft account restore   → POST /api/account/deletion/cancel
                                  cancelled_at set, mail "Sletningen er annulleret",
                                  founder note "Sletning annulleret"
   nordkraft account status    → GET /api/account (shows the pending deletion)

when scheduled_for passes (runner in services/account_deletion.rs, every
ACCOUNT_DELETION_INTERVAL seconds, ACCOUNT_DELETION_AUTO_PURGE=true by default):
   same purge as POST /api/admin/tenant/purge
   ▼
completed_at set, mail "Din konto er slettet" (the last one), then the
email and name are cleared from account_deletions. The record stays as the
proof that it happened. Founder note "Konto slettet".
```

03:00 local is 01:00 or 02:00 UTC, so the UTC calendar day equals the Danish
one and the date in the mail is the date it happens, at night.

The founder notes go through the backend's `POST /api/internal/notify`, which
fans out to ntfy and to `NOTIFY_TO` (default `frederik@nordkraft.io`). They
carry slot numbers and dates, never names or addresses.

The admin purge endpoint calls the same `finish_requested` step, so a
manually finished deletion also sends the final mail. A purge of an account
nobody asked to delete (a test account) sends nothing.

The purge path is unchanged from `services/tenant_purge.rs`; the admin
route and the runner share `account_deletion::execute_purge`. A failed
scheduled purge is retried on the next tick, three times, with a founder note
each time; the third says it is giving up and `last_error` stays filled in for
the admin purge. Because the purge suspends the account first, `restore` is
impossible once it has started, which is the intended point of no return.

With `ACCOUNT_DELETION_AUTO_PURGE=false` the runner only logs due deletions
once per tick and the admin purge finishes them by hand.

## Configuration

Controller (`/etc/nordkraft/controller.env`):

| variable | default | meaning |
| --- | --- | --- |
| `SIGNUP_API_URL` | `http://127.0.0.1:8000` | where the signup backend answers `/api/internal/mail` and `/api/internal/notify` |
| `ACCOUNT_DELETION_GRACE_DAYS` | `14` | days between confirmation and purge |
| `ACCOUNT_DELETION_AUTO_PURGE` | `true` | `false` turns the runner into a logger |
| `ACCOUNT_DELETION_INTERVAL` | `600` | seconds between runner ticks |

`ADMIN_API_KEY` must be the same in both units (it already is: the backend
uses it to call the controller).

Signup backend (`garage-cloud.service`):

| variable | default | meaning |
| --- | --- | --- |
| `SMTP_USER`, `SMTP_PASSWORD` | — | enable email (customer mail and founder mail) |
| `NOTIFY_TO` | `Frederik <frederik@nordkraft.io>` | founder notifications: signup, claim, every deletion step |
| `CUSTOMER_REPLY_TO` | `Frederik <frederik@nordkraft.io>` | Reply-To on customer mail |
| `PUBLIC_BASE_URL` | `https://cloud.nordkraft.io` | base of the install one-liner in the welcome mail |
| `NTFY_TOPIC` | — | push notifications for the same events |

Without SMTP the backend prints the token and the deletion codes to its log
(`🔑 (email disabled)`) so a dev setup can be walked through; production
always has SMTP.

## Rollout order

1. Controller first (it applies the migration and serves `/api/account/*`;
   the old backend keeps working against it). The runner purges due
   deletions by default; set `ACCOUNT_DELETION_AUTO_PURGE=false` in the unit
   to watch the first ones by hand.
2. Signup backend together with its static frontend (success page without
   the token). `scripts/install.sh` from this repo must be redeployed too,
   wherever `cloud.nordkraft.io/install.sh` is served from.
3. CLI release: adds `nordkraft account …` and `--now`. Old CLIs keep working:
   setup never changed on the wire.

## Not done (follow-ups)

- Abandoned signups (token never claimed) have no path out. A reaper that
  reminds at day 7 and removes unclaimed accounts at day 30 would cover most
  "deletions" in practice.
- Deletion needs a working CLI. Someone who lost their keys writes to
  frederik@nordkraft.io and the admin purge does the rest.
