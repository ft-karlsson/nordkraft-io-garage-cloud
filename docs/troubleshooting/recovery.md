# Ny maskine eller mistet lokal konfiguration

Ny computer, formateret disk, eller en `~/.nordkraft` mappe der er blevet slettet ved et uheld? Det meste kan genskabes.

Grunden er hvordan NordKraft er bygget: **din identitet lever på controller'en** (din WireGuard public key er din bruger), og **dine containere lever i runtime på noderne**. Næsten alt lokalt er afledt af de to — og afledt tilstand kan bygges op igen.

---

## Hvad ligger lokalt

Alt hvad CLI'en gemmer på din maskine ligger i `~/.nordkraft/`:

| Fil | Indhold | Kan genskabes fra |
|-----|---------|-------------------|
| `wg.conf` | WireGuard config med din **private nøgle** | Kun hvis du har nøglen — ellers ny token |
| `connection.json` | Navn, plan, VPN IP, server-nøgle | `nordkraft auth login` + din `wg.conf` |
| `deployments/*.nk` | Dine deployment specs | Controller'en eller den kørende container |
| `registry.json` | Adresse på dit private registry | Din registry-container |
| `aliases.json` | Dine korte navne for containere | **Ingen steder** |

!!! warning "aliases.json kan ikke genskabes"
    Aliaser findes kun på din egen maskine. Hverken controller'en eller runtime kender dem. Mister du filen, skal navnene sættes igen i hånden.

### Tag en sikkerhedskopi nu

```bash
tar -czf ~/nordkraft-backup-$(date +%F).tar.gz -C ~ .nordkraft
```

!!! danger "Filen indeholder din private nøgle"
    Backup'en indeholder `wg.conf` med din private WireGuard-nøgle. Læg den et sted du ville lægge en SSH-nøgle — ikke i et delt drev eller et git-repo.

---

## Trin 1: Installér CLI'en igen

```bash
curl -fsSL https://cloud.nordkraft.io/install.sh | sh
```

Uden token. Scriptet installerer binary'en og WireGuard-værktøjerne, men rører ikke din konto.

!!! info "Kør ikke `nordkraft setup` endnu"
    `nordkraft setup` genererer et **nyt** nøglepar og claimer en token. Har du stadig din private nøgle, skal du ikke bruge setup — se A nedenfor.

---

## Trin 2: Få forbindelse igen

### A — Du har stadig din private nøgle

Så mangler du kun at skrive konfigurationen igen. Ud over nøglen skal du bruge tre værdier: din VPN IP, controller'ens public key, og dine AllowedIPs.

```bash
mkdir -p ~/.nordkraft && chmod 700 ~/.nordkraft
cat > ~/.nordkraft/wg.conf <<'EOF'
[Interface]
PrivateKey = <din private nøgle>
Address = 172.20.0.<dit-slot>/32

[Peer]
PublicKey = <controller public key>
Endpoint = cloud.nordkraft.io:51820
AllowedIPs = 172.20.0.254/32, 172.21.<dit-slot>.0/24
PersistentKeepalive = 25
EOF
chmod 600 ~/.nordkraft/wg.conf

nordkraft connect
nordkraft auth login
```

Alt er udledt af dit **slot-nummer**: din VPN IP er `172.20.0.<slot>` og dit container-subnet er `172.21.<slot>.0/24`. Har du et gammelt output fra `nordkraft auth login` liggende, står slot og VPN IP der. Ellers skriv til <frederikkarlsson@me.com> med din **public key** — den kan du regne ud af din private nøgle:

```bash
echo '<din private nøgle>' | wg pubkey
```

`connection.json` behøver du ikke skrive selv — CLI'en falder tilbage til controller'en på `172.20.0.254:8001` uden den. Den bliver skrevet igen næste gang du kører `nordkraft setup`, og indtil da viser `nordkraft auth login` de samme oplysninger.

### B — Du har ikke nøglen længere

Skriv til <frederikkarlsson@me.com>. Du får en ny engangs-token, og så er det den normale installation:

```bash
nordkraft setup NKINVITE-...
```

Din konto, dit slot, dine containere, dine volumes og dine data er uberørte. Det eneste der sker, er at din gamle nøgle holder op med at være en gyldig peer, og den nye tager over. Er den gamle maskine mistet eller stjålet, er det præcis den effekt du vil have.

---

## Trin 3: Genskab dine `.nk` specs

Dine specs kan bygges op igen fra den config containeren blev deployet med:

```bash
nordkraft --json list | jq -r '.[].name' | while read -r n; do nordkraft init "$n" --from-server; done
```

`--from-server` henter den gemte deploy-config fra controller'en. Det er den nøjagtige konfiguration — inklusive `volume_size`, som ikke kan aflæses af den kørende container — og specen beholder serverens revisionsnummer.

Containere der blev deployet **før** config-tracking blev indført, findes ikke på serveren. Dem bygger du fra den kørende container i stedet:

```bash
nordkraft init <container>
```

Kør altid en diff bagefter, før du ændrer noget:

```bash
nordkraft diff <container>
```

!!! warning "Tjek ressourcer før du kører upgrade"
    Bygger du en spec fra en kørende container, og CLI'en ikke kan aflæse cpu eller hukommelse, skriver den en advarsel og bruger standardværdier (0.5 cpu / 512m). Kører containeren med mere end det, vil et `upgrade` skrue den **ned**. Ret værdien i specen først.

---

## Trin 4: Registry og aliaser

### Private registry

Kender CLI'en ikke dit registry, kan `nordkraft push` ikke bruges — og `nordkraft registry init` vil deploye et **nyt** registry ved siden af det du allerede har.

Adressen står i dine egne image-navne (`nordkraft list` viser fx `172.21.1.3:5001/min-app:v3`), og container-navnet finder du samme sted:

```bash
cat > ~/.nordkraft/registry.json <<'EOF'
{
  "address": "172.21.<slot>.<ip>:5001",
  "container_name": "app-...",
  "container_alias": "registry"
}
EOF

nordkraft registry status
```

Viser den `online` og dine images, er den rigtig.

### Aliaser

De skal sættes i hånden:

```bash
nordkraft alias set web app-3ada50d2-5e43-4863-b0fc-eec1f0fd56cc
```

Kan du ikke huske hvilken container der er hvad, hjælper `nordkraft list` (image-navnet) og `nordkraft ingress list` (subdomænet).

---

## Trin 5: Verificér

```bash
nordkraft auth login       # navn, plan, VPN IP, slot
nordkraft list             # dine containere
nordkraft specs            # dine .nk specs
nordkraft alias list       # dine aliaser
nordkraft registry status  # dit registry
```

Svarer alle fem, er du helt tilbage.

---

## Hvad du bør sikkerhedskopiere fremover

To ting er værd at gemme, fordi de ikke kan genskabes fra serveren:

1. **Din private WireGuard-nøgle** (`~/.nordkraft/wg.conf`) — uden den kræver en ny maskine en ny token
2. **`~/.nordkraft/aliases.json`** — findes kun hos dig

Resten — specs, registry-config, forbindelsesinfo — kan altid bygges op igen med kommandoerne ovenfor.
