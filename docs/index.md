# nordkraft.io Garage Cloud

**Åbent. Bæredygtigt. sikkert.**

Velkommen til dokumentationen for nordkraft.io Garage Cloud - en dansk open source container-platform bygget på genbrugt hardware. 

!!! note "Under udvikling"
    Platformen er stadig under aktiv udvikling og signup kun tilgængelig via invitation - smid mig en mail hvis du er interesseret
    
!!! success "Mark I Testing færdig (January 2026)"
    Kernefeatures er testet og virker:
    
    - ✅ **Zero-trust authentication** - WireGuard VPN integration
    - ✅ **Kata VM isolation** - Hardware-level container security
    - ✅ **Dual-stack networking** - IPv4 + IPv6 via SLAAC
    - ✅ **Persistent storage** - Data overlever genstart (lidt sjovere)
    - ✅ **HTTPS ingress** - Automatisk Let's Encrypt TLS
    - ✅ **Environment variables** - Fuld konfiguration support
    - ✅ **Multi-node orchestration** - NATS-baseret message passing
    
    
!!! success "Mark I ½ færdig (September 2026)"
    Ikke Mark II endnu — men listen er blevet længere:

    - ✅ **Deklarative deployments** - `.nk` specs med `diff` og `upgrade` fra CLI'en
    - ✅ **Privat image-registry** - `nordkraft push` og `registry://` i dine deploys
    - ✅ **Genskab din opsætning** - `nordkraft init --from-server` henter specs fra serveren
    - ✅ **Rigtige ressourcegrænser** - aflæst fra runtime, ikke gættet
    - ✅ **Aliaser** - korte navne i stedet for UUID'er
    - ✅ **Robust installation** - virker på en frisk maskine uden håndholdning
    - ✅ **Samlet dokumentation** - bor nu sammen med koden, inkl. guide til maskinskifte

    Se hele listen i [Hvad er nyt](changelog.md).

---

## Hvad er så en Garage Cloud?

nordkraft.io's Garage Cloud er container hosting med fokus på:

- **Enkelthed** - Udgiv apps med én kommando
- **Bæredygtighed** - Kører på genbrugt hardware, grøn energi (i form af solceller) & anvendelse af eksisterende bygninger (ingen nye datacentre).
- **Sikkerhed** - WireGuard VPN, isolerede netværk, zero-trust arkitektur, brug af kata containers.
- **Gennemsigtighed** - Brug af opensource til at drive platformen, og Garage cloud er open source selv - få samme løsning til at køre derhjemme hos dig. Se open source kode her: [https://github.com/ft-karlsson/nordkraft-io-garage-cloud](https://github.com/ft-karlsson/nordkraft-io-garage-cloud) 

---

## Kom hurtigt i gang

### 1. Installer CLI

```bash
# macOS / Linux
curl -fsSL https://cloud.nordkraft.io/install.sh | sh
```

### 2. Forbind til VPN

Import din WireGuard-konfiguration (signup: [cloud.nordkraft.io](https://cloud.nordkraft.io)) og forbind.

### 3. Verificer forbindelse

```bash
nordkraft auth login
```

### 4. Deploy din første container

```bash
nordkraft deploy nginx:alpine --port 80
```

**Det var det!** Din container kører nu på dansk hardware.

[→ Fuld guide: Din første container](getting-started.md)

!!! note "Er den så tilgængelig online?"
    Nej. Den er ikke offentligt tilgængelig på internettet endnu. Du kan nå den via WireGuard på dens lokale IP-adresse.
    Du skal specifikt bede om at åbne den for "verden" med ingress. Se [Gør den tilgængelig fra internettet](getting-started.md#deploy-pacman).

### 5. Se din container

Find containerens IP-adresse:
```bash
nordkraft list
```

Output:
```
📦 Fetching containers...
🐳 1 container(s):
  NAME: app-ac75ce48-2771-462a-9b87-a0eaa46adb05
    Image: docker.io/library/nginx:alpine
    Status: Up 2 days
    IPv4: 172.21.1.34
    Ports: 80/tcp
    Created: 2 days ago
```

Åbn i browser (mens du er på VPN):
```bash
# Eller åbn direkte i browser
open http://172.21.1.34
```

🎉 **Du ser nu din container på Garage Cloud!**
---

## Populære guides

| Guide | Beskrivelse |
|-------|-------------|
| [Din første container](getting-started.md) | Fra nul til kørende container på 30 minutter |
| [Installation](installation.md) | CLI + VPN setup |
| [CLI Reference](reference/cli.md) | Alle kommandoer |
| [Databaser](guides/databases.md) | PostgreSQL, MySQL, Redis |
| [HTTPS & Domæner](guides/domains.md) | Automatisk TLS certifikat |

---

## CLI hurtigreference

| Handling | Kommando |
|----------|----------|
| Deploy container | `nordkraft deploy IMAGE --port PORT` |
| Deploy med IPv6 | `nordkraft deploy IMAGE --port PORT --ipv6` |
| Se containere | `nordkraft list` |
| Se logs | `nordkraft logs NAVN` |
| Stop container | `nordkraft stop NAVN` |
| Start container | `nordkraft start NAVN` |
| Slet container | `nordkraft rm NAVN` |
| Aktiver HTTPS | `nordkraft ingress enable NAVN --subdomain X` |
| Åbn IPv6 firewall | `nordkraft ipv6 open NAVN` |
| Se IPv6 status | `nordkraft ipv6 status NAVN` |

[→ Fuld CLI reference](reference/cli.md)

---

## Brug for hjælp?

- **Email:** frederikkarlsson@me.com
- **GitHub Issues:** [github.com/ft-karlsson/nordkraft-io-garage-cloud/issues](https://github.com/ft-karlsson/nordkraft-io-garage-cloud/issues)

---

<small>
nordkraft.io Garage Cloud - Bygget i Ry, Danmark 🇩🇰<br>
Kører på genbrugt / refurbished Dell og Raspberry Pi hardware.
</small>
