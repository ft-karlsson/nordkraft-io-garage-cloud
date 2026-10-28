# Account lifecycle: activation by mail, the command on the page, self-service deletion

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

Codes are six digits and allow five guesses. Activation codes live 24 hours,
deletion codes 30 minutes. Only `sha256(id ":" code)` is stored (the backend's
`verification.rs` and container-api's `services/verification.rs` are the same
module).

## 1. Signup → activation mail

```
cloud.nordkraft.io  "Opret konto"
   │ POST /api/signup
   ▼
users row, account_status = 'pending', NKINVITE- token in wireguard_public_key
email_verifications (activate) + mail "Aktivér din konto på Garage Cloud: 123456"
   link  https://cloud.nordkraft.io/activate?id=<verification>&code=123456
   code  123456
page "Tjek din indbakke": a code field, and it polls GET /api/signup/<id>/status
```

Nothing but the activation mail leaves at this point; the invite token is not
shown anywhere. The send is awaited, so the page knows whether the mail left
(`mail_sent`) and offers "Send mailen igen" (`POST /api/signup/<id>/resend`,
60 s cooldown). A second signup with the same, still pending address sends a
fresh activation mail instead of a "taken" error, so a lost mail is never a
dead end. With SMTP off (dev) the code is printed to the log and returned to
the page.

## 2. Activation → the command, in the page and by mail

```
link clicked (any device) or code typed on the page
   │ POST /api/activate {verification_id, code}
   ▼
users.account_status = 'active'; verified_at set
mail "Din konto er aktiveret: her er din installationskommando"
   curl -fsSL https://cloud.nordkraft.io/install.sh | sh -s NKINVITE-…
   nordkraft setup NKINVITE-…
   + links to docs.nordkraft.io (getting started, deploy your app, CLI)
founder note "E-mail bekræftet"
page (the tab that signed up, via the poll, or the tab the link opened):
   "Din konto er aktiveret!"  the same command with a copy button, account
   details, what happens next, the docs links
```

The customer stays in the tab they are in: the signup tab moves on by itself
within three seconds of the link being clicked elsewhere, and the link opens
the same page. The command is in the mail too, for later.

```
nordkraft setup NKINVITE-…
   │ POST /api/claim {token, wireguard_public_key}   (403 while 'pending')
   ▼
users.wireguard_public_key = key
container-api /admin/tenant/provision (peer + nftables)
founder notification "Kunde aktiveret"
CLI: write wg.conf, wg-quick up, verify, done.
```

Setup itself asks nothing: the token was handed out only after the address
was confirmed. Once claimed, `/status` carries no token and `/resend` answers
409. `scripts/install.sh` runs setup with stdin on `/dev/tty` when it is
invoked through `curl … | sh`, so any prompt a future setup might have, and
sudo's, still reach the terminal.

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
| `NOTIFY_TO` | `Frederik <frederik@nordkraft.io>` | founder notifications: signup, e-mail confirmed, claim, every deletion step |
| `CUSTOMER_REPLY_TO` | `Frederik <frederik@nordkraft.io>` | Reply-To on customer mail |
| `PUBLIC_BASE_URL` | `https://cloud.nordkraft.io` | base of the activation link and of the install one-liner |
| `NTFY_TOPIC` | — | push notifications for the same events |

Without SMTP the backend prints the activation code, the token and the
deletion codes to its log (`🔑 (email disabled)`) so a dev setup can be walked
through; production always has SMTP.

## Rollout order

1. Controller first (it applies the migration and serves `/api/account/*`;
   the old backend keeps working against it). The runner purges due
   deletions by default; set `ACCOUNT_DELETION_AUTO_PURGE=false` in the unit
   to watch the first ones by hand.
2. Signup backend together with its static frontend (activation page,
   `/activate` route). `scripts/install.sh` from this repo must be redeployed
   too, wherever `cloud.nordkraft.io/install.sh` is served from.
3. CLI release: adds `nordkraft account …` and `--now`. Old CLIs keep working:
   setup never changed on the wire (a pending account now gets a 403 with a
   sentence saying to click the link first).

## Not done (follow-ups)

- Abandoned signups (never activated, or token never claimed) have no path
  out. A reaper that reminds at day 7 and removes them at day 30 would cover
  most "deletions" in practice.
- Someone can sign up with another person's address; the token is then shown
  in the tab that signed up once that person clicks the link. The mail says
  not to click if you did not sign up, and the claim provisions a peer under
  the victim's name only after that click. Showing the command only on the
  tab that clicked would close this at the cost of the "stay in your tab"
  flow; not done.
- Deletion needs a working CLI. Someone who lost their keys writes to
  frederik@nordkraft.io and the admin purge does the rest.
