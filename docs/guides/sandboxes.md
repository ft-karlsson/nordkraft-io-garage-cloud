# Sandboxes: shell, exec og livscyklus

En container på Nordkraft kan bruges som en *sandbox*: et sted hvor et program
— eller en AI-agent — kører kommandoer, og som rydder op efter sig selv. Tre
ting gør det muligt: en interaktiv shell, et `exec`-kald til scripts og
orkestratorer, og en livscyklus der er en del af containerens spec.

Det hele kører over din WireGuard-forbindelse. Der er ingen ekstra login:
netværket er adgangskontrollen, præcis som for resten af platformen.

## Shell

```bash
nordkraft shell myapp            # /bin/sh i containeren
nordkraft shell myapp -- bash -l # et andet program
```

Du får en rigtig terminal (pty): piletaster, `top`, `vim`, resize af vinduet —
alt virker som med `kubectl exec -it`. `exit` lukker sessionen, og exit-koden
fra shellen bliver CLI'ens exit-kode.

## Exec

```bash
nordkraft exec myapp -- ls -la /data
nordkraft exec myapp --cwd /app -e DEBUG=1 -- ./migrate.sh
echo "SELECT 1" | nordkraft exec db --stdin -- psql -U app
nordkraft exec myapp --timeout 30 -- ./slow-job   # exit 124 ved timeout
```

`exec` kører én kommando, skriver stdout og stderr igennem og afslutter med
kommandoens exit-kode. Det gør den brugbar i scripts (`if nordkraft exec …`)
og som byggesten for orkestratorer, der vil have et REST-kald i stedet:

```bash
curl -X POST http://172.20.0.254:8001/api/containers/myapp/exec \
  -H 'Content-Type: application/json' \
  -d '{"cmd":["sh","-c","echo hej"],"cwd":"/tmp","env":{"X":"1"},"timeout_s":60}'
# {"exit_code":0,"stdout":"hej\n","stderr":"","duration_ms":41,"truncated":false}
```

Output er begrænset til 1 MiB pr. kald (`truncated: true` hvis der blev
klippet). Brug `logs` eller en fil i containeren til mere.

## Livscyklus

Livscyklussen er en del af `.nk`-spec'en, så den følger containeren gennem
`deploy`, `init` og `upgrade`:

```toml
[lifecycle]
restart      = "never"   # always | unless-stopped | on-failure | never
auto_stop    = "15m"     # stop efter 15 min uden shell/exec-aktivitet
auto_delete  = "1h"      # slet 1 time efter containeren stoppede
max_lifetime = "24h"     # stop senest 24 timer efter oprettelse

[labels]
workspace = "ws-42"
kind      = "crawler"
```

Varigheder skrives som `90s`, `15m`, `2h`, `1d` eller sekunder. `0` betyder
aldrig. De samme felter findes som flag på `deploy`:

```bash
nordkraft deploy python:3.12 --command "python worker.py" \
  --restart never --auto-stop 15m --auto-delete 1h \
  --label workspace=ws-42
```

Hvad der sker, og hvornår:

| Timer | Tæller fra | Handling |
|-------|-----------|----------|
| `auto_stop` | seneste shell/exec-session, ellers containerens start | stop (0 kr. compute, volume bevares) |
| `max_lifetime` | oprettelse | stop |
| `auto_delete` | containeren stoppede | slet container og volume |

En stoppet container koster ingen compute; `auto_delete` er til sandboxes
der ikke skal komme tilbage. Noden tjekker hvert 30. sekund, så en timer kan
udløse op til 30 sekunder sent.

### Labels

Labels er frie nøgle/værdi-par som du selv bestemmer over. De vises i
`nordkraft list` og kan bruges til at filtrere:

```bash
nordkraft list --label workspace=ws-42
nordkraft list --label workspace=ws-42 --label kind=crawler
```

Det er sådan en orkestrator finder "sine" containere igen.

### Vent på at et job bliver færdigt

```bash
nordkraft deploy alpine --command "sh -c 'do-work; exit 3'" --restart never --auto-delete 1h
nordkraft wait app-1234            # blokerer til processen slutter, exit 3
```

`wait` returnerer containerens exit-kode (124 hvis `--timeout` løber ud
først). Sæt `restart = "never"` på jobs — ellers starter platformen dem igen.

## Rigtige sandboxes: `--sandbox`

```bash
nordkraft deploy python:3.12-alpine --sandbox --auto-stop 15m --auto-delete 1h
nordkraft exec <navn> -- python3 -c 'print(40+2)'
echo "data" | nordkraft exec <navn> --stdin -- sh -c 'cat; echo done'
```

Med `--sandbox` bliver containerens PID 1 **nordkraft-sandboxd**, en lille daemon der kører
dit `--command` som sit barn (uden `--command` venter sandboxen bare) og serverer exec,
shell, terminaler og filer fra indersiden af micro-VM'en. Forskellen mærkes på Kata-noder:
stdin, output efter EOF og store uploads virker fuldt ud, fordi intet går gennem runtimens
exec-vej. Filer flyttes med `PUT/GET /containers/{id}/files` (se [API](../reference/api.md#sandboxes-med-nordkraft-sandboxd)).
Almindelige deploys er uændrede.

## For orkestratorer (Mastra, Daytona-stil)

Begreberne er valgt så de passer direkte på hvad en sandbox-orkestrator
forventer: `create` → `POST /containers/deploy` med `lifecycle` og `labels`,
`exec` → `POST /containers/{id}/exec`, `stop`/`destroy` → de eksisterende
endpoints, `wait` → `POST /containers/{id}/wait`, og en interaktiv terminal
→ WebSocket på `/containers/{id}/stream`. Se [API-referencen](../reference/api.md#exec-shell-og-wait).

Til Mastra findes en færdig provider, `@nordkraft/mastra`, så en agent kan bruge en
Nordkraft-container som sin sandbox. Se [Mastra-guiden](mastra.md).
