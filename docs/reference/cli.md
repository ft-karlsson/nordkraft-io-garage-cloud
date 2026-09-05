# CLI Reference

Komplet oversigt over alle `nordkraft` kommandoer.

---

## Grundlæggende brug

```bash
nordkraft [KOMMANDO] [ARGUMENTER] [FLAG]
```

Få hjælp til enhver kommando:

```bash
nordkraft help
nordkraft [KOMMANDO] --help
```

---

## Container kommandoer

### deploy

Deploy en ny container.

```bash
nordkraft deploy IMAGE [FLAG]
```

**Argumenter:**

| Argument | Beskrivelse |
|----------|-------------|
| `IMAGE` | Container image (f.eks. `nginx:alpine`, `ghcr.io/user/app:v1`) |

**Flag:**

| Flag | Kort | Default | Beskrivelse |
|------|------|---------|-------------|
| `--port` | `-p` | - | Port(s) containeren lytter på (kan gentages) |
| `--env` | `-e` | - | Miljøvariabel (kan gentages) |
| `--cpu` | | `0.5` | CPU grænse i cores ⚠️ |
| `--memory` | `-m` | `512m` | Hukommelsesgrænse ⚠️ |
| `--persistence` | | `false` | Aktiver persistent storage |
| `--volume-path` | | - | Sti til persistent storage (påkrævet med --persistence) |
| `--ipv6` | | `false` | Allokér global IPv6 adresse |
| `--name` | `-n` | auto | Brugerdefineret container navn |
| `--garage` | | auto | Target garage (ry, aarhus, etc.) |
| `--hardware` | | auto | Hardware præference (optiplex, raspi, mac-mini) |

!!! warning "Kendte problemer"
    **CPU/Memory limits:** Disse flag er implementeret men bliver ikke anvendt endnu (bug #1). Dette bliver rettet i næste opdatering.

**Eksempler:**

```bash
# Simpel webserver
nordkraft deploy nginx:alpine --port 80

# Med miljøvariabler
nordkraft deploy myapp:v1 --port 3000 \
  --env NODE_ENV=production \
  --env API_KEY=secret123

# Med ressourcegrænser
nordkraft deploy myapp:v1 --port 3000 \
  --cpu 1.0 \
  --memory 1g

# Database med persistent storage
nordkraft deploy postgres:15 --port 5432 --persistent
```

---

### list

Vis alle dine containere.

```bash
nordkraft list [FLAG]
```

**Flag:**

| Flag | Beskrivelse |
|------|-------------|
| `--all` | Vis også stoppede containere |
| `--json` | Output som JSON |

**Eksempel output:**

```
NAME                              IMAGE           STATUS    IP            PORTS
app-8ade4622-fdd6-411b-afc9...   nginx:alpine    running   172.21.5.15   80/tcp
db-3fa85f64-5717-4562-b3fc...    postgres:15     running   172.21.5.16   5432/tcp
```

---

### logs

Vis logs fra en container.

```bash
nordkraft logs CONTAINER [FLAG]
```

**Flag:**

| Flag | Kort | Default | Beskrivelse |
|------|------|---------|-------------|
| `--lines` | `-n` | `100` | Antal linjer |
| `--follow` | `-f` | `false` | Følg logs i realtid |

**Eksempler:**

```bash
# Sidste 100 linjer
nordkraft logs myapp

# Sidste 50 linjer
nordkraft logs myapp --lines 50

# Følg logs live
nordkraft logs myapp --follow
```

---

### stop

Stop en kørende container.

```bash
nordkraft stop CONTAINER
```

Containeren stoppes men slettes ikke. Data bevares.

---

### start

Start en stoppet container.

```bash
nordkraft start CONTAINER
```

---

### restart

Genstart en container (stop + start).

```bash
nordkraft restart CONTAINER
```

!!! note "Kommer snart"
    Restart kommandoen er under udvikling. I mellemtiden: brug `nordkraft stop NAVN` efterfulgt af `nordkraft start NAVN`.

---

### rm

Slet en container permanent.

```bash
nordkraft rm CONTAINER [FLAG]
```

**Flag:**

| Flag | Beskrivelse |
|------|-------------|
| `--force` | Slet uden bekræftelse |

!!! warning "Advarsel"
    Dette sletter containeren og frigiver IP-adressen. Persistent data backuppes automatisk.

---

## Deklarative deployments (.nk specs)

NordKraft.io gemmer hver deployment som en `.nk` spec-fil (TOML-format) i `~/.nordkraft/deployments/`. Du kan redigere dem direkte, diff'e mod kørende containere og applicere ændringer — ligesom Kubernetes manifests, men enklere.

### init

Generér en `.nk` spec fra en kørende container.

```bash
nordkraft init <container>
```

Bruger du uden argument, får du en interaktiv vælger over kørende containere.

**Flag:**

| Flag | Beskrivelse |
|------|-------------|
| `--from-server` | Byg specen fra den config containeren blev deployet med, i stedet for fra den kørende container |

```bash
nordkraft init web --from-server
```

`--from-server` henter den gemte deploy-config fra controller'en. Det er den nøjagtige konfiguration — inklusive `volume_size`, som ikke kan aflæses fra runtime — og specen beholder serverens revisionsnummer i stedet for at starte forfra på 0.

Containere der blev deployet før config-tracking blev indført, har ingen gemt config. Der siger kommandoen det, og du bruger `nordkraft init <container>` uden flaget i stedet.

!!! tip "Mistet dine specs?"
    Se [Ny maskine / mistet config](../troubleshooting/recovery.md) for at bygge hele `~/.nordkraft` op igen.

### specs

List alle gemte deployment specs. Grøn prik = ny deployment (r0), cyan prik = opgraderet mindst én gang.

```bash
nordkraft specs
```

### diff

Sammenlign en `.nk` spec mod den kørende container og vis forskelle.

```bash
nordkraft diff <container>
```

### upgrade

Applicer spec-ændringer til den kørende container. Genbygger containeren hvis nødvendigt.

```bash
nordkraft upgrade <container>
nordkraft upgrade <container> --yes   # spring bekræftelse over
```

### edit

Åbn `.nk` spec-filen i `$EDITOR` (default: `vim`).

```bash
nordkraft edit <container>
```

### spec-set

Redigér et felt i en `.nk` spec direkte fra kommandolinjen — uden at åbne en editor. Værdier type-coerces automatisk (int → float → bool → string), og filen valideres før den gemmes, så typos fanges med det samme.

```bash
nordkraft spec-set <container> <key> <value> [--apply]
```

**Dotted key syntax:** `table.field`, fx `resources.cpu`, `deployment.image`, `network.ipv6`.

**Array operationer:** Prefix værdien med `+` for at tilføje, eller `-` for at fjerne:

```bash
# Sæt et simpelt felt
nordkraft spec-set web resources.cpu 2
nordkraft spec-set web deployment.image nginx:1.27
nordkraft spec-set web network.ipv6 true

# Tilføj/fjern fra arrays
nordkraft spec-set web network.ports +8080
nordkraft spec-set web network.ports -8080

# Redigér og applicer i én kommando
nordkraft spec-set web resources.memory 512m --apply
```

| Flag | Beskrivelse |
|------|-------------|
| `--apply` | Applicer ændringen til den kørende container med det samme |

### spec-unset

Fjern et felt fra en `.nk` spec.

```bash
nordkraft spec-unset <container> <key>
```

**Eksempel:**
```bash
nordkraft spec-unset web network.ipv6
```

### spec-delete

Slet en `.nk` spec-fil. **Dette påvirker IKKE den kørende container** — brug `nordkraft rm` separat hvis du også vil slette containeren.

```bash
nordkraft spec-delete <container>
nordkraft spec-delete <container> --yes   # spring bekræftelse over
```

| Flag | Beskrivelse |
|------|-------------|
| `-y, --yes` | Spring bekræftelsesprompt over |

!!! tip "Hvorfor bruge specs?"
    Specs er perfekte til at versionere din infrastruktur i git, genudrulle en container med de samme indstillinger, eller dele opsætninger med kollegaer. Filen `~/.nordkraft/deployments/<name>.nk` er ren TOML — commit den gerne.

---

## Netværk kommandoer

### network info

Vis dine netværksoplysninger.

```bash
nordkraft network info
```

**Output:**

```
Garage:           ry
Container subnet: 172.21.5.0/24
VPN IP:           172.20.1.5
API server:       172.20.0.254
```

---

## Ingress kommandoer (HTTPS)

### ingress enable

Aktiver HTTPS adgang til en container.

```bash
nordkraft ingress enable CONTAINER [FLAG]
```

**Flag:**

| Flag | Påkrævet | Beskrivelse |
|------|----------|-------------|
| `--subdomain` | Ja | Subdomain (f.eks. `myapp` → `myapp.nordkraft.cloud`) |
| `--port` | Nej | Target port (default: containerens port) |

**Eksempel:**

```bash
nordkraft ingress enable myapp --subdomain coolsite
# Resultat: https://coolsite.nordkraft.cloud
```

---

### ingress disable

Deaktiver HTTPS adgang.

```bash
nordkraft ingress disable CONTAINER
```

---

### ingress status

Vis ingress status for en container.

```bash
nordkraft ingress status CONTAINER
```

---

### ingress list

Vis alle dine ingress routes.

```bash
nordkraft ingress list
```

---

## IPv6 kommandoer

### ipv6 open

Åbn firewall for containerens IPv6 adresse.

```bash
nordkraft ipv6 open CONTAINER
```

Gør containeren tilgængelig på en global IPv6 adresse.

---

### ipv6 close

Luk firewall for IPv6.

```bash
nordkraft ipv6 close CONTAINER
```

---

### ipv6 status

Vis IPv6 status.

```bash
nordkraft ipv6 status CONTAINER
```

---

### ipv6 list

Vis alle IPv6 allokeringer.

```bash
nordkraft ipv6 list
```

---

## Auth kommandoer

### auth login

Bekræft din authentication og forbindelse.

```bash
nordkraft auth login
```

**Output:**

```
✓ Sikker forbindelse etableret til Dit Navn!
```

---

### auth status

Vis detaljeret auth status.

```bash
nordkraft auth status
```

---

## System kommandoer

### status

Vis systemstatus.

```bash
nordkraft status
```

---

### nodes

Vis tilgængelige nodes.

```bash
nordkraft nodes
```

---

### version

Vis CLI version.

```bash
nordkraft --version
```

---

## Alias kommandoer

Containere får automatisk UUID-navne som `app-3ada50d2-5e43-4863-b0fc-eec1f0fd56cc`. Et alias er et kort navn du selv vælger, og det kan bruges alle steder hvor en kommando forventer en container.

### alias set

```bash
nordkraft alias set <alias> <container>
```

```bash
nordkraft alias set web app-3ada50d2-5e43-4863-b0fc-eec1f0fd56cc
nordkraft logs web
nordkraft diff web
```

### alias list

```bash
nordkraft alias list
```

### alias rm

```bash
nordkraft alias rm web
```

Fjerner kun aliaset — containeren røres ikke.

!!! warning "Aliaser findes kun på din maskine"
    De ligger i `~/.nordkraft/aliases.json`, og hverken controller'en eller noderne kender dem. Mister du filen, kan de ikke genskabes — se [Ny maskine / mistet config](../troubleshooting/recovery.md).

---

## Registry kommandoer

Dit eget private image-registry kører som en container i dit subnet. Det lader dig deploye images du selv har bygget, uden at lægge dem på Docker Hub.

### registry init

```bash
nordkraft registry init
```

Deployer registry-containeren og gemmer adressen i `~/.nordkraft/registry.json`.

!!! warning "Kør kun én gang"
    Kender CLI'en ikke dit registry — fx på en ny maskine — deployer `registry init` et **nyt** ved siden af det du allerede har. Genskab i stedet `registry.json`, se [Ny maskine / mistet config](../troubleshooting/recovery.md).

### registry status

```bash
nordkraft registry status
```

Viser adresse, container og hvilke images der ligger i registry'et.

### registry list

```bash
nordkraft registry list
```

### push

```bash
nordkraft push <image>
```

Sender et lokalt image til dit registry. `nordkraft registry push` gør det samme.

```bash
docker build --platform linux/amd64 -t myapp:v1 .
nordkraft push myapp:v1
nordkraft deploy 172.21.<dit-slot>.<ip>:5001/myapp:v1 --port 3000
```

!!! tip "Byg til den rigtige arkitektur"
    Noderne kører x86_64. Bygger du på en Apple Silicon Mac uden `--platform linux/amd64`, får du et arm64-image der fejler med "exec format error" når containeren starter.

### registry destroy

```bash
nordkraft registry destroy [--force]
```

Fjerner registry-containeren og alle images i den.

---

## Forbindelse

### setup

```bash
nordkraft setup NKINVITE-...
```

Førstegangsopsætning: genererer et nyt WireGuard nøglepar lokalt, claimer dit token, skriver konfigurationen og rejser tunnelen. Se [Installation](../installation.md).

### connect / disconnect

```bash
nordkraft connect
nordkraft disconnect
```

Rejser eller lukker tunnelen med den config der allerede ligger i `~/.nordkraft/wg.conf`.

### reset

```bash
nordkraft reset [--force]
```

!!! danger "Fjerner al lokal konfiguration"
    `reset` lukker tunnelen og sletter `~/.nordkraft` — inklusive din private nøgle, dine specs og dine aliaser. Dine containere bliver ikke rørt, men du skal bruge en ny token for at komme på igen. Tag en kopi først.

---

## Opdatering

### update

```bash
nordkraft update
nordkraft update --check
```

Henter nyeste release og erstatter binary'en. `--check` viser kun om der er en nyere version. Din konfiguration i `~/.nordkraft/` røres ikke.

---

## Globale flag

Disse flag virker på alle kommandoer:

| Flag | Beskrivelse |
|------|-------------|
| `--help` | Vis hjælp |
| `--json` | Output som JSON |
| `--quiet` | Minimal output |
| `--verbose` | Detaljeret output |

---

## Exit koder

| Kode | Betydning |
|------|-----------|
| `0` | Succes |
| `1` | Generel fejl |
| `2` | Ugyldig kommando/argumenter |
| `3` | Authentication fejl |
| `4` | Netværksfejl |
| `5` | Container ikke fundet |
