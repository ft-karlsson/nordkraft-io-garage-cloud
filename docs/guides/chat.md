# Deploy din egen chat-server

Træt af at betale for Slack måned efter måned? Her deployer du din egen private chat-server på nordkraft.io — i fuld ejerskab, på dansk hardware.

Vi gennemgår to muligheder:

| | Campfire | Mattermost |
|--|---------|-----------|
| **Kompleksitet** | ⭐ Simpel | ⭐⭐⭐ Mere kompleks |
| **Størrelse** | ~150 MB, ét container | ~400 MB + Postgres |
| **Database** | SQLite (indbygget) | PostgreSQL (separat container) |
| **Features** | Chat, DMs, rum, fil-deling | Chat + kanaler, integrationer, plugins |
| **Bedst til** | Små teams, hurtig opsætning | Teams der vil have Slack-oplevelsen |

---

## Option A: Campfire (anbefalet til de fleste)

[Campfire](https://github.com/basecamp/once-campfire) er lavet af Basecamp. Alt er i ét container — ingen separat database, ingen kompliceret opsætning.

### Deploy

```bash
# 1. Deploy Campfire
nordkraft deploy ghcr.io/basecamp/once-campfire:latest \
  -p 80 \
  --persistence --volume-path /rails/storage \
  -e SECRET_KEY_BASE=$(openssl rand -hex 64) \
  --name campfire

# 2. Aktiver HTTPS ingress
nordkraft ingress enable campfire --subdomain campfire --port 80

# 3. Tjek at den starter korrekt
nordkraft container logs campfire --lines 20
```

Åbn **https://campfire.nordkraft.cloud** og opret første bruger — det er din admin.

!!! tip "SECRET_KEY_BASE"
    `openssl rand -hex 64` genererer en unik nøgle til at signere sessions. Kør kommandoen én gang og gem værdien — hvis den ændres, logges alle brugere ud.

---

## Option B: Mattermost (fuld Slack-alternativ)

[Mattermost](https://mattermost.com) er det tætteste du kommer på Slack som self-hosted løsning. Det kræver en PostgreSQL database — så vi deployer to containers.

### Trin 1 — Deploy PostgreSQL

```bash
nordkraft deploy postgres:16-alpine \
  -p 5432 \
  --persistence --volume-path /var/lib/postgresql/data \
  -e POSTGRES_USER=mmuser \
  -e POSTGRES_PASSWORD=mmpassword \
  -e POSTGRES_DB=mattermost \
  --name mattermost-db
```

### Trin 2 — Verificér at Postgres er klar

Kig efter `database system is ready to accept connections`:

```bash
nordkraft container logs mattermost-db --lines 20
```

### Trin 3 — Hent Postgres IP

```bash
POSTGRES_IP=$(nordkraft container inspect mattermost-db --json | jq -r '.container_ip')
echo "Postgres kører på: $POSTGRES_IP"
```

### Trin 4 — Deploy Mattermost

```bash
nordkraft deploy mattermost/mattermost-team-edition:latest \
  -p 8065 \
  --persistence --volume-path /mattermost/data \
  -e MM_SQLSETTINGS_DRIVERNAME=postgres \
  -e "MM_SQLSETTINGS_DATASOURCE=postgres://mmuser:mmpassword@${POSTGRES_IP}/mattermost?sslmode=disable" \
  -e MM_SERVICESETTINGS_SITEURL=https://mattermost.nordkraft.cloud \
  --name mattermost
```

### Trin 5 — Aktiver HTTPS ingress

```bash
nordkraft ingress enable mattermost --subdomain mattermost --port 8065
```

### Trin 6 — Verificér

Kig efter `Server is listening on :8065`:

```bash
nordkraft container logs mattermost --lines 20
```

Åbn **https://mattermost.nordkraft.cloud** og opret admin-bruger.

!!! note "Plugin-advarsler ved opstart"
    Mattermost logger et par fejl om plugins der kræver Professional license (Playbooks, Boards). Det er normalt og ufarligt — de springes blot over.

---

## Ressourceforbrug

| Container | RAM | Disk (image) |
|-----------|-----|--------------|
| Campfire | ~150 MB | ~150 MB |
| Mattermost | ~400 MB | ~400 MB |
| Postgres | ~80 MB | ~60 MB |

Begge løsninger passer fint inden for nordkraft.io Test tier (4 GB RAM).
