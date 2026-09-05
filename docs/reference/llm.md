# nordkraft.io til AI-assistenter

Denne side er designet til at blive givet til en AI-assistent (ChatGPT, Claude, Copilot m.fl.) som kontekst, så den kan hjælpe dig med at deploye og administrere containere på nordkraft.io.

---

## Sådan bruger du denne side

Kopier og indsæt følgende prompt til din AI-assistent:

!!! example "Prompt til AI"

    ```
    Jeg bruger nordkraft.io — en dansk edge cloud platform.
    Her er den fulde CLI og API spec:
    https://docs.nordkraft.io/reference/llm/

    Hjælp mig med at [beskriv hvad du vil deploye].
    ```

Eller kopiér hele indholdet nedenfor direkte ind i din samtale.

---

## Maskinelæselig spec

````markdown
# nordkraft.io CLI + API Spec

## Platform
- Dansk edge cloud, CLI-first, IPv6-native, WireGuard-autentificeret
- API endpoint: `172.20.0.254:8001` (kun tilgængelig via WireGuard VPN)
- Authentication: WireGuard IP → public key → bruger (zero-trust, ingen API keys)
- Base URL: `http://172.20.0.254:8001/api`

---

## CLI — nordkraft

### Global flag
```
--json      Output som JSON (til scripting og jq)
--help      Vis hjælp
```

### Container kommandoer

#### deploy
```
nordkraft deploy IMAGE [FLAG]
nordkraft container deploy IMAGE [FLAG]

FLAG:
  -p, --port PORT           Port containeren lytter på (kan gentages)
  -e, --env KEY=VALUE       Miljøvariabel (kan gentages)
      --env-file FILE       Indlæs env fra fil
      --cpu FLOAT           CPU grænse (default: 0.5)
      --memory STRING       RAM grænse (default: 512m)
      --persistence         Aktiver persistent storage
      --volume-path PATH    Sti i containeren til persistent data (kræves med --persistence)
      --ipv6                Allokér global IPv6 adresse
      --name STRING         Brugerdefineret navn (kan bruges i alle efterfølgende kommandoer)
      --garage STRING       Target garage (fx ry)
      --hardware STRING     Hardware præference (optiplex, raspi, mac-mini)
      --command STRING      Override container startup kommando
```

#### list / ls
```
nordkraft list
nordkraft container list
```

#### inspect
```
nordkraft container inspect CONTAINER [--json]

JSON output felter:
  .container_id    Lang hex container ID
  .name            App navn (bruges til ingress, stop, rm etc.)
  .image           Image navn
  .status          running | stopped | error
  .container_ip    Intern IPv4 (172.21.x.x)
  .ipv6_address    Global IPv6 adresse (hvis aktiveret)
  .ports[]         Array af { port, protocol, access_url }
  .node_id         Node containeren kører på
  .runtime         io.containerd.kata.v2 (VM isolation)
  .persistence_enabled  Boolean
  .volume_mounts   Array af mount strings
```

#### logs
```
nordkraft container logs CONTAINER [--lines N] [--follow]
nordkraft logs CONTAINER [-n N]
```

#### stop / start / restart / rm
```
nordkraft container stop CONTAINER
nordkraft container start CONTAINER
nordkraft container restart CONTAINER
nordkraft container rm CONTAINER [--force]

# Shortcuts (uden 'container'):
nordkraft stop CONTAINER
nordkraft start CONTAINER
nordkraft rm CONTAINER
```

---

### Deklarative deployments (.nk specs)

NordKraft.io gemmer hver deployment som en TOML-fil i `~/.nordkraft/deployments/<name>.nk`. Brug specs til at versionere infrastruktur, redeploy med samme indstillinger, eller editere via CLI uden at åbne en editor.

```
nordkraft init CONTAINER
  # Generér .nk spec fra kørende container. Uden arg: interaktiv vælger.

nordkraft specs
  # List alle gemte specs. Grøn prik = ny (r0), cyan = opgraderet (r1+).

nordkraft diff CONTAINER
  # Sammenlign spec mod live container. Viser changed/unchanged felter.

nordkraft upgrade CONTAINER [--yes]
  # Applicer spec-ændringer til kørende container.

nordkraft edit CONTAINER
  # Åbn .nk fil i $EDITOR (default: vim).
```

#### Rediger spec-felter fra CLI (uden editor)

```
nordkraft spec-set CONTAINER KEY VALUE [--apply]
  # KEY er dotted path: table.field
  # Kendte tables: deployment, resources, network, storage, placement
  # Typer coerces automatisk: int → float → bool → string
  # Valideres før save — typos fanges med det samme
  # --apply: applicer til kørende container i samme kald

# Array operationer: prefix værdien
nordkraft spec-set CONTAINER network.ports +8080   # tilføj
nordkraft spec-set CONTAINER network.ports -8080   # fjern

nordkraft spec-unset CONTAINER KEY
  # Fjern felt fra spec.

nordkraft spec-delete CONTAINER [--yes]
  # Slet .nk fil. Rører IKKE den kørende container.
  # -y/--yes: spring confirm prompt over.
```

#### Typiske spec-set eksempler

```
nordkraft spec-set web resources.cpu 2
nordkraft spec-set web resources.memory 512m
nordkraft spec-set web deployment.image nginx:1.27 --apply
nordkraft spec-set web network.ipv6 true
nordkraft spec-set web network.ports +3001
nordkraft spec-unset web network.ipv6
nordkraft spec-delete web --yes
```

#### .nk fil format

```toml
# NordKraft deployment spec — auto-generated
[deployment]
name = "app-uuid"
image = "registry/image:tag"
revision = 3
created = "2026-04-05T21:05:24Z"
updated = "2026-04-05T21:28:57Z"

[resources]
cpu = 0.5
memory = "512m"

[network]
ports = [3000]
ipv6 = false

[placement]
garage = "ry"
```

---

### Ingress kommandoer (HTTPS, *.nordkraft.cloud)

```
nordkraft ingress enable CONTAINER -s SUBDOMAIN [-p PORT] [-m MODE]
  # CONTAINER = .name fra inspect (IKKE .container_id)
  # MODE: https (default) | http | tcp
  # Resultat: https://SUBDOMAIN.nordkraft.cloud

nordkraft ingress disable CONTAINER
nordkraft ingress status CONTAINER
nordkraft ingress list
```

---

### IPv6 kommandoer

```
nordkraft ipv6 open CONTAINER
nordkraft ipv6 close CONTAINER
nordkraft ipv6 status CONTAINER
nordkraft ipv6 list
nordkraft ipv6 ports CONTAINER PORT [PORT ...]
```

---

### Registry kommandoer

```
nordkraft registry init          # Opsæt privat OCI registry
nordkraft registry status        # Vis registry status
nordkraft registry list          # List images
nordkraft registry destroy       # Fjern registry

nordkraft push IMAGE             # Push til privat registry
# Deploy fra privat registry:
nordkraft deploy registry://myapp:v1 --port 3000
```

---

### Auth, system og setup

```
nordkraft auth login             # Verificer VPN + auth
nordkraft auth status
nordkraft auth whoami

nordkraft status                 # Systemstatus
nordkraft nodes                  # List cluster nodes

nordkraft setup NKINVITE-token   # Første gangs opsætning
nordkraft connect                # Opret WireGuard forbindelse
nordkraft disconnect             # Afbryd WireGuard

nordkraft alias set ALIAS CONTAINER
nordkraft alias list
nordkraft alias rm ALIAS

nordkraft update                 # Opdatér CLI
nordkraft update --check         # Tjek for opdateringer
nordkraft reset [--force]        # Slet al lokal config
```

---

## Scripting med --json og jq

```bash
# Hent intern IP fra kørende container
IP=$(nordkraft container inspect NAVN --json | jq -r '.container_ip')

# Hent container name (bruges til ingress)
NAME=$(nordkraft container inspect NAVN --json | jq -r '.name')

# Hent IPv6 adresse
IPV6=$(nordkraft container inspect NAVN --json | jq -r '.ipv6_address')
```

---

## Vigtige noter til AI

- `--name` sætter et menneskevenligt navn der bruges i ALLE efterfølgende kommandoer
- `ingress enable` tager `.name` (fx `mattermost`), IKKE `.container_id`
- `--persistence` kræver altid `--volume-path PATH` — de to flag hører sammen
- Interne container IPs er på `172.21.x.x` subnets (én per bruger)
- API server er på `172.20.0.254:8001` (kun via VPN)
- Containere kører i Kata VM isolation (ikke bare namespaces)
- Ingen separat auth header nødvendig — WireGuard IP er identity
- Efter første deploy: brug `nordkraft init <container>` + `nordkraft spec-set` til at ændre indstillinger deklarativt i stedet for at redeploye fra bunden

---

## Eksempel: Deploy Mattermost (multi-container)

```bash
# 1. Deploy Postgres
nordkraft deploy postgres:16-alpine \
  -p 5432 \
  --persistence --volume-path /var/lib/postgresql/data \
  -e POSTGRES_USER=mmuser \
  -e POSTGRES_PASSWORD=mmpassword \
  -e POSTGRES_DB=mattermost \
  --name mattermost-db

# 2. Verificér Postgres er klar
nordkraft container logs mattermost-db --lines 20

# 3. Hent Postgres IP
POSTGRES_IP=$(nordkraft container inspect mattermost-db --json | jq -r '.container_ip')

# 4. Deploy Mattermost
nordkraft deploy mattermost/mattermost-team-edition:latest \
  -p 8065 \
  --persistence --volume-path /mattermost/data \
  -e MM_SQLSETTINGS_DRIVERNAME=postgres \
  -e "MM_SQLSETTINGS_DATASOURCE=postgres://mmuser:mmpassword@${POSTGRES_IP}/mattermost?sslmode=disable" \
  -e MM_SERVICESETTINGS_SITEURL=https://mattermost.nordkraft.cloud \
  --name mattermost

# 5. Aktiver HTTPS
nordkraft ingress enable mattermost --subdomain mattermost --port 8065

# 6. Verificér
nordkraft container logs mattermost --lines 20
```

## Eksempel: Deploy Campfire (enkelt container)

```bash
nordkraft deploy ghcr.io/basecamp/once-campfire:latest \
  -p 80 \
  --persistence --volume-path /rails/storage \
  -e SECRET_KEY_BASE=$(openssl rand -hex 64) \
  --name campfire

nordkraft ingress enable campfire --subdomain campfire --port 80
```
````

## Eksempel: Opdatér en kørende container med spec-set

```bash
# Generér spec fra kørende container (en gang)
nordkraft init campfire

# Skaler op
nordkraft spec-set campfire resources.cpu 2 --apply
nordkraft spec-set campfire resources.memory 1g --apply

# Skift image uden at redeploye fra bunden
nordkraft spec-set campfire deployment.image ghcr.io/basecamp/once-campfire:v2 --apply

# Tilføj en ekstra port
nordkraft spec-set campfire network.ports +8443 --apply

# Se diff før du applicerer
nordkraft spec-set campfire resources.cpu 4     # uden --apply
nordkraft diff campfire
nordkraft upgrade campfire --yes
```

---

## Hvad AI-assistenter kan hjælpe med

- Generere deploy-kommandoer til enhver Docker-kompatibel applikation
- Sætte korrekte miljøvariabler for kendte apps (Postgres, Redis, WordPress, etc.)
- Skrive bash-scripts der kombinerer `--json` + `jq` til automatisering
- Debugge fejl fra `nordkraft container logs`
- Finde det rigtige `--volume-path` for en given applikation

!!! tip "Tips til bedre resultater"
    - Fortæl AI hvilken app du vil deploye og hvad den skal gøre
    - Del log-output direkte hvis noget fejler
    - Spørg AI om at generere et samlet bash-script du kan køre linje for linje
