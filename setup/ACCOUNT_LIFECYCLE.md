# Account lifecycle: signup, activation, leaving

How a customer gets in, proves the address is theirs, and gets out again.
Three services take part: the **signup backend** (`cloud.nordkraft.io`, repo
`nordkraft-cloud-backend`, owns SMTP and the customer mails), the
**controller** (`container-api`, owns the tenant and the purge) and the
**CLI**. They share one Postgres database.

`setup/migration_account_lifecycle.sql` adds the two tables the flow needs
(one for e-mail confirmations, one for deletion requests). The controller
applies it on start, like the other migrations. Confirmation codes are
never stored in clear, and a deletion request outlives the users row so
there is a record that the purge finished.

## 1. Signup and activation

```
cloud.nordkraft.io  "Opret konto"
   ▼
mail "Aktivér din konto på Garage Cloud" with a link and a short code
page "Tjek din indbakke": type the code, or click the link anywhere
   ▼
account active
mail "Din konto er aktiveret: her er din installationskommando"
   curl -fsSL https://cloud.nordkraft.io/install.sh | sh -s NKINVITE-…
   + links to docs.nordkraft.io
page: the same command with a copy button, account details, next steps
```

The invite token is handed out only after the address is confirmed, on the
page and in the second mail. The signup page notices a click made elsewhere
and moves on by itself, so the customer stays in the tab they are in. The
page has a "send again" button, rate limited, and signing up again with an
address that never finished activation simply sends the activation mail
again.

## 2. Setup

```
nordkraft setup NKINVITE-…      (or the one-liner from the mail)
   ▼
keypair generated locally, token exchanged for the WireGuard config
controller provisions the peer, tunnel comes up
founder notification "Kunde aktiveret"
```

Setup asks for nothing. `scripts/install.sh` runs it with stdin on
`/dev/tty` when invoked through `curl … | sh`, so sudo and any future prompt
still reach the terminal.

## 3. Leaving

```
nordkraft account delete [--now]        (over the customer's own tunnel)
   ▼
mail "Bekræft sletning af din konto" with a short code
   Code: ______
   ▼
deletion scheduled
   default:   03:00 Europe/Copenhagen, ACCOUNT_DELETION_GRACE_DAYS days ahead
              mail "Din konto slettes den 11. november 2026"
   --now:     within minutes; no grace period, no "slettes den" mail
everything keeps working until then

   nordkraft account restore   cancels it; mail "Sletningen er annulleret"
   nordkraft account status    shows a pending deletion

when the moment comes (runner in the controller):
   same purge as the admin tenant purge
   ▼
mail "Din konto er slettet" (the last one); afterwards the address and name
are cleared from the request record, which stays as proof it happened
```

03:00 local is 01:00 or 02:00 UTC, so the UTC calendar day equals the Danish
one and the date in the mail is the date it happens, at night.

The founder gets a push and a mail at every step (scheduled, immediate,
cancelled, purged, failed), carrying slot numbers and dates only. A failed
purge is retried a few times and then left for the admin purge, with the
error kept on the record. Once a purge has started the account is suspended,
so `restore` no longer applies: that is the point of no return.

With `ACCOUNT_DELETION_AUTO_PURGE=false` the runner only logs due deletions
and the admin purge finishes them by hand.

## Configuration

Controller (`/etc/nordkraft/controller.env`):

| variable | default | meaning |
| --- | --- | --- |
| `SIGNUP_API_URL` | `http://127.0.0.1:8000` | the signup backend, which sends the customer mails on the controller's behalf |
| `ACCOUNT_DELETION_GRACE_DAYS` | `14` | days between confirmation and purge |
| `ACCOUNT_DELETION_AUTO_PURGE` | `true` | `false` turns the runner into a logger |
| `ACCOUNT_DELETION_INTERVAL` | `600` | seconds between runner ticks |

Both services use the same admin key for their calls to each other.

Signup backend (`garage-cloud.service`):

| variable | default | meaning |
| --- | --- | --- |
| `SMTP_USER`, `SMTP_PASSWORD` | — | enable email (customer mail and founder mail) |
| `NOTIFY_TO` | `Frederik <frederik@nordkraft.io>` | founder notifications: signup, activation, setup, every deletion step |
| `CUSTOMER_REPLY_TO` | `Frederik <frederik@nordkraft.io>` | Reply-To on customer mail |
| `PUBLIC_BASE_URL` | `https://cloud.nordkraft.io` | base of the links in customer mail |
| `NTFY_TOPIC` | — | push notifications for the same events |

Production always has SMTP; without it the backend logs what it would have
mailed so a dev setup can be walked through.

## Rollout order

1. Controller first (it applies the migration and serves the account
   routes; the old backend keeps working against it). The runner purges due
   deletions by default; set `ACCOUNT_DELETION_AUTO_PURGE=false` in the unit
   to watch the first ones by hand.
2. Signup backend together with its static frontend. `scripts/install.sh`
   from this repo must be redeployed too, wherever
   `cloud.nordkraft.io/install.sh` is served from.
3. CLI release: adds `nordkraft account …` and `--now`. Old CLIs keep
   working: setup never changed on the wire.

## Not done (follow-ups)

- Abandoned signups (never activated, or token never claimed) have no path
  out. A reaper that reminds after a week and removes them after a month
  would cover most "deletions" in practice.
- Deletion needs a working CLI. Someone who lost their keys writes to
  frederik@nordkraft.io and the admin purge does the rest.
