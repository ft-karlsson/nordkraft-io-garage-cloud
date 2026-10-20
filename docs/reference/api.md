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
| `/containers/{id}/exec` | POST | Kør én kommando, få output og exit-kode |
| `/containers/{id}/stream` | GET (WebSocket) | Interaktiv shell / streamet exec |
| `/containers/{id}/wait` | POST | Vent på at hovedprocessen slutter |
| `/auth/verify` | GET | Verificer auth |

`GET /containers` tager `?label=KEY=VALUE` (kan gentages) og returnerer kun containere med alle de angivne labels.

### Deploy: livscyklus og labels

`POST /containers/deploy` accepterer ud over de kendte felter:

```json
{
  "image": "python:3.12",
  "command": ["python", "worker.py"],
  "lifecycle": {"restart": "never", "auto_stop": "15m", "auto_delete": "1h", "max_lifetime": "24h"},
  "labels": {"workspace": "ws-42"}
}
```

Varigheder: `90s`, `15m`, `2h`, `1d` eller sekunder; `0` = aldrig. Samme felter kan sendes til `PUT /containers/{id}/upgrade`. Se [Sandboxes](../guides/sandboxes.md).

### Exec, shell og wait

**`POST /containers/{id}/exec`** — ikke-interaktiv, bufferet.

```json
{"cmd": ["sh", "-c", "echo hej"], "cwd": "/tmp", "env": {"X": "1"}, "user": "1000:1000",
 "stdin": "tekst til stdin", "timeout_s": 60}
```

Svar: `{"exit_code": 0, "stdout": "hej\n", "stderr": "", "duration_ms": 41, "truncated": false}`.
`exit_code` er `null` og `error` sat hvis processen ikke kunne startes eller blev dræbt ved timeout.
Output klippes ved 1 MiB (`truncated: true`). `timeout_s` default 120, max 3600.

Udelades `stdin`, læser processen EOF fra `/dev/null` med det samme (som `kubectl exec` uden
`-i`). `"stdin": ""` giver en tilknyttet, tom stdin der lukkes straks.

**`GET /containers/{id}/stream`** — opgraderes til WebSocket.

Query: `tty=true|false`, `stdin=true|false` (default `true`), `cmd=` (gentages, default `/bin/sh`),
`cwd=`, `env=K=V` (gentages), `user=`, `cols=`, `rows=`.

`stdin=false` tilknytter ingen stdin: processen får EOF fra `/dev/null`, stdin-frames ignoreres,
og noden lukker aldrig stdin gennem containerd. Brug det til kommandoer der ikke læser input —
på Kata Containers kan output skrevet efter en stdin-lukning ellers gå tabt
(se [kendte problemer](../troubleshooting/known-issues.md)).

Frames er binære; første byte er kanalen (samme protokol som Kubernetes' `v4.channel.k8s.io`):

| Kanal | Retning | Indhold |
|-------|---------|---------|
| `0` | klient → container | stdin (en tom frame = EOF) |
| `1` | container → klient | stdout |
| `2` | container → klient | stderr (bruges ikke med tty) |
| `3` | container → klient | status, JSON `{"exit_code": 7}` eller `{"error": "…"}` — sendes præcis én gang til sidst |
| `4` | klient → container | resize, JSON `{"width": 132, "height": 43}` |

Sessionen afsluttes når processen slutter, klienten lukker, efter 30 min uden trafik eller efter 12 timer.
Processen i containeren dræbes altid når sessionen slutter — ingen forældreløse shells.

**`POST /containers/{id}/wait?timeout_s=300`** — long-poll. Svar `{"exit_code": 3, "timed_out": false}`;
`exit_code` er `null` hvis containeren ikke har nogen kørende proces (aldrig startet eller allerede fjernet).
Kald igen mens `timed_out` er `true`. Max 3600 sekunder pr. kald.

Alle tre virker ens uanset hvilken node containeren kører på: controlleren videresender
strømmen uændret til noden over WireGuard.

### Sandboxes med nordkraft-sandboxd

`POST /containers/deploy` med `"sandbox": true` gør **nordkraft-sandboxd** til containerens
entrypoint. Daemonen kører `command` (hvis angivet) som sit barn — uden `command` venter
sandboxen bare på exec — og serverer exec, strømme, terminaler og filer fra *indersiden* af
micro-VM'en på containerens egen adresse. `exec`, `stream` og `shell` bruger automatisk
daemonen for sådan en container; intet går gennem runtimens exec-vej, og stdin, output efter
EOF og store uploads virker fuldt ud også på Kata. Svaret på deploy har `"sandbox": true`.

**`PUT /containers/{id}/files?path=/abs/sti[&mode=755]`** — kun sandboxes. Body er filens
indhold (op til 256 MiB). Mapper oprettes, filen skrives atomisk. Svar: `{"path": "...", "bytes": 3000000}`.

**`GET /containers/{id}/files?path=/abs/sti`** — streamer filen tilbage. `404` hvis den ikke findes.

Daemonen autentificerer controlleren med et token pr. container, som controlleren opretter ved
deploy og gemmer i containerens config (`GET /containers/{id}/config` viser det til ejeren).
Den lytter på port 7777 inde i containeren; porten er kun nåelig fra WireGuard-nettet.
