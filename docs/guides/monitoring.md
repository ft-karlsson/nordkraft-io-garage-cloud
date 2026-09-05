# Overvåg dine apps med Uptime Kuma

[Uptime Kuma](https://github.com/louislam/uptime-kuma) er et simpelt, self-hosted monitoring-værktøj. Sæt det op én gang, og få besked hvis noget af det du kører på nordkraft.io går ned.

**Hvad du får:**

- ✅ Overvåg HTTP, TCP, ping — alt hvad du deployer
- ✅ Notifikationer via email, Slack, Telegram, og mange andre
- ✅ Pænt status-dashboard du kan dele med andre
- ✅ Response time historik og uptime statistik

---

## Deploy Uptime Kuma

Alt er i ét container — ingen database, ingen dependencies.

```bash
# 1. Deploy Uptime Kuma
nordkraft deploy louislam/uptime-kuma:latest \
  -p 3001 \
  --persistence --volume-path /app/data \
  --name uptime-kuma

# 2. Aktiver HTTPS ingress
nordkraft ingress enable uptime-kuma --subdomain uptime --port 3001

# 3. Tjek at den starter
nordkraft container logs uptime-kuma --lines 20
```

Åbn **https://uptime.nordkraft.cloud** og opret admin-bruger første gang.

---

## Tilføj dine services til overvågning

Når du er logget ind, klik **"Add New Monitor"**.

### Overvåg en HTTPS ingress

| Felt | Værdi |
|------|-------|
| Monitor Type | HTTP(s) |
| Friendly Name | Campfire |
| URL | `https://campfire.nordkraft.cloud` |
| Heartbeat Interval | 60 sekunder |

### Overvåg en intern container direkte

Uptime Kuma kører på samme VPN-netværk som dine containere, så den kan nå dem direkte på deres interne IP — uden at gå omvejen om ingress.

```bash
# Find din containers interne IP
nordkraft container inspect campfire --json | jq -r '.container_ip'
# → 172.21.1.3
```

| Felt | Værdi |
|------|-------|
| Monitor Type | HTTP(s) |
| Friendly Name | Campfire (intern) |
| URL | `http://172.21.1.3:80` |

Dette er mere direkte og fanger fejl selv hvis ingress har problemer.

---

## Notifikationer

Klik **"Settings" → "Notifications"** og tilføj din foretrukne kanal.

Populære valg:

- **Email** — simpelt og altid tilgængeligt
- **Slack / Mattermost** — hvis du allerede kører Mattermost på nordkraft.io 😄
- **Telegram** — hurtigt og gratis
- **Webhook** — til custom integrations

---

## Status-side (valgfrit)

Uptime Kuma kan generere en offentlig status-side — nyttigt hvis du vil vise andre at dine services kører.

Klik **"Status Page" → "New Status Page"**, vælg hvilke monitors der skal vises, og del linket.

---

!!! tip "Hold det simpelt"
    Start med at overvåge dine vigtigste services via HTTPS URL. Du kan altid tilføje flere monitors senere.
