# Account lifecycle: welcome mail, email-verified activation, self-service deletion

How a customer gets in, proves the address is theirs, and gets out again.
Three services take part: the **signup backend** (`cloud.nordkraft.io`, repo
`nordkraft-cloud-backend`, owns SMTP and the customer mails), the
**controller** (`container-api`, owns the tenant and the purge) and the
**CLI**. They share one Postgres database, `garage_cloud`.

Tables added by `setup/migration_account_lifecycle.sql` (applied by the
controller on every start, like the other migrations):

| table | one row per | survives the users row |
| --- | --- | --- |
| `email_verifications` | code sent to an address (`purpose` = `activate` or `delete`), SHA-256 of the code, attempts, expiry | no |
| `account_deletions` | request to leave: requested, scheduled, cancelled, started, completed, attempts, last error | **yes** (that is the point) |

Codes are six digits, valid 30 minutes, five guesses, one per minute. Only
`sha256(id ":" code)` is stored. Both services carry the same
`verification.rs`.

## 1. Signup → welcome mail

`POST /api/signup` creates the users row with the `NKINVITE-` token as
before and now also mails the customer: the same install one-liner and
`nordkraft setup NKINVITE-…` the success page shows, plus a heads-up that a
code will follow. A bounced address shows up in the relay log.

## 2. Claim → code → active

```
nordkraft setup NKINVITE-…
   │ POST /api/claim {token, wireguard_public_key}
   ▼
users.wireguard_public_key = key, account_status = 'pending'
email_verifications (activate) + mail "Din kode til Garage Cloud: 123456"
   │ response carries verification_id, email_verification_required = true
   ▼
CLI writes wg0.conf + connection.json {pending_verification: id}, then asks:
   Code: ______        (Enter = "I clicked the link", r = resend)
   │ POST /api/verify {verification_id, code}
   │   or the link https://cloud.nordkraft.io/verify?id=…&code=… (SPA page, same call)
   │   GET /api/verify/<id>  is what the CLI polls after Enter
   │   POST /api/verify/<id>/resend  new code, 60 s cooldown
   ▼
users.account_status = 'active' → container-api /admin/tenant/provision
founder notification "Kunde aktiveret" (moved here from claim)
CLI: wg-quick up, verify, done.
```

While the account is `pending` the peer reconciler does not add the peer and
the API guard refuses the key, so nothing is reachable until the code is in.
An interrupted setup resumes with `nordkraft connect` (or
`nordkraft connect --code 123456` without a prompt); the pending state lives
in `~/.nordkraft/connection.json`. Accounts activated before this change are
untouched: they are already `active`.

## 3. Leaving

```
nordkraft account delete
   │ POST /api/account/deletion/request          (WireGuard-authenticated)
   ▼
email_verifications (delete) + mail "Bekræft sletning af din konto" (code)
   │ sent by the backend: container-api → POST /api/internal/mail (admin key)
   ▼
   Code: ______
   │ POST /api/account/deletion/confirm {verification_id, code}
   ▼
account_deletions {scheduled_for = now + ACCOUNT_DELETION_GRACE_DAYS}
mail "Din konto slettes den 10. november 2026"
users row unchanged: VPN, containers, everything keeps working

   nordkraft account restore   → POST /api/account/deletion/cancel
                                  cancelled_at set, mail "Sletningen er annulleret"
   nordkraft account status    → GET /api/account (shows the pending deletion)

when scheduled_for passes (runner in services/account_deletion.rs, every
ACCOUNT_DELETION_INTERVAL seconds):
   ACCOUNT_DELETION_AUTO_PURGE=true  → same purge as POST /api/admin/tenant/purge
   otherwise                         → one warn line per tick listing due user ids;
                                       the admin purge finishes them by hand
   ▼
completed_at set, mail "Din konto er slettet" (the last one), then the
email and name are cleared from account_deletions. The record stays as the
proof that it happened.
```

The admin purge endpoint calls the same `finish_requested` step, so a
manually finished deletion also sends the final mail. A purge of an account
nobody asked to delete (a test account) sends nothing.

The purge path is unchanged from `services/tenant_purge.rs`; the admin
route and the runner now share `account_deletion::execute_purge`. A failed
scheduled purge is retried on the next tick, three times, then left for the
admin with `last_error` filled in. Because the purge suspends the account
first, `restore` is impossible once it has started, which is the intended
point of no return.

## Configuration

Controller (`/etc/nordkraft/controller.env`):

| variable | default | meaning |
| --- | --- | --- |
| `SIGNUP_API_URL` | `http://127.0.0.1:8000` | where the signup backend answers `/api/internal/mail` |
| `ACCOUNT_DELETION_GRACE_DAYS` | `14` | days between confirmation and purge |
| `ACCOUNT_DELETION_AUTO_PURGE` | `false` | `true` lets the runner purge due deletions itself |
| `ACCOUNT_DELETION_INTERVAL` | `600` | seconds between runner ticks |

`ADMIN_API_KEY` must be the same in both units (it already is: the backend
uses it to call the controller).

Signup backend (`garage-cloud.service`):

| variable | default | meaning |
| --- | --- | --- |
| `SMTP_USER`, `SMTP_PASSWORD` | — | enable email (customer mail and founder mail) |
| `NOTIFY_TO` | — | founder notifications; optional now |
| `CUSTOMER_REPLY_TO` | `Frederik <frederik@nordkraft.io>` | Reply-To on customer mail |
| `PUBLIC_BASE_URL` | `https://cloud.nordkraft.io` | base of the link in the activation mail |

Without SMTP the backend prints the codes to its log (`🔑 (email disabled)`)
so a dev setup can be walked through; production always has SMTP.

## Rollout order

1. Controller first (it applies the migration and serves `/api/account/*`;
   the old backend keeps working against it).
2. Signup backend (claim now leaves accounts `pending`; needs the controller
   to accept `/admin/tenant/provision` from `/api/verify`, which it always did).
3. CLI release: an old CLI against the new backend writes its config and
   runs `wg-quick up`, which then times out because the peer is not there yet;
   `nordkraft connect` with the new CLI finishes the verification.
