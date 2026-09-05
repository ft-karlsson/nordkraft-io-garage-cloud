# Din første container

**Tidsforbrug:** Under 5 minutter.

---

## Før du starter

Tre ting skal være på plads:

1. **nordkraft.io konto** — opret på [cloud.nordkraft.io](https://cloud.nordkraft.io)
2. **Installer CLI + forbind** — kør det kommando du fik i din velkomst-email:
```bash
curl -fsSL https://cloud.nordkraft.io/install.sh | sh -s NKINVITE-...
```
3. **Tjek at alt virker:**
```bash
nordkraft auth login
```

Ser du `✓ Sikker forbindelse etableret`? Perfekt — videre.

---

## Deploy Pacman 🕹️

Et par kommandoer — og du spiller Pacman fra din egen cloud.

```bash
# 1. Deploy
nordkraft deploy ghcr.io/piebro/pacman-on-a-webpage:latest \
  -p 8080 \
  --name pacman

# 2. Giv den et subdomain (vælg dit eget hvis pacman er optaget)
nordkraft ingress enable pacman --subdomain pacman --port 8080
```

Åbn **https://pacman.nordkraft.cloud** — og spil. 🎮

!!! tip "Subdomain optaget?"
    Vælg dit eget — fx `--subdomain frederik-pacman`. Subdomainet skal bare være unikt på tværs af alle brugere.

Det er alt. Din første container kører på dansk hardware bag HTTPS.

---

## Hvad skete der?

```
nordkraft deploy  →  Kata VM startet på ry-optiplex-1
                      Container-IP: 172.21.x.x (privat, kun via VPN)

ingress enable    →  HAProxy-regel oprettet
                      Let's Encrypt wildcard-cert aktiveret
                      https://pacman.nordkraft.cloud live
```

---

## De vigtigste kommandoer

```bash
nordkraft list                    # Se dine containere
nordkraft container logs pacman   # Se hvad der sker inde i containeren
nordkraft container stop pacman   # Pause
nordkraft container start pacman  # Start igen
nordkraft container rm pacman     # Slet permanent
```

---

## Hvad nu?

- [Deploy din egen app](guides/webapp.md) — Dockerfile → privat registry → live på 20 minutter
- [Chat-server](guides/chat.md) — Campfire eller Mattermost på 3 kommandoer
- [Overvågning](guides/monitoring.md) — Uptime Kuma holder øje med det hele
- [Databaser](guides/databases.md) — PostgreSQL, Redis med persistent storage

---

Problemer? Se [Troubleshooting](troubleshooting/vpn.md) eller skriv til support@nordkraft.io.
