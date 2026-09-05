# Environment Variabler

!!! info "Kommer snart"
    Denne guide er under udarbejdelse.

## Quick start

```bash
nordkraft deploy myapp:v1 --port 3000 \
  --env NODE_ENV=production \
  --env DATABASE_URL=postgres://user:pass@172.21.5.16:5432/mydb \
  --env API_KEY=secret123
```

## Brug en env-fil

Hvis du har mange environment variabler, kan du samle dem i en fil:
```bash
cat campfire.env
```
```
SECRET_KEY_BASE=ec0f55a7d3...
DATABASE_URL=postgres://user:pass@172.21.1.36:5432/campfire
REDIS_URL=redis://172.21.1.37:6379
RAILS_ENV=production
RAILS_LOG_TO_STDOUT=true
```

Deploy med `--env-file`:
```bash
nordkraft deploy ghcr.io/username/campfire:latest --port 3000 \
  --env-file campfire.env
```

!!! warning "Husk"
    Tilføj `.env` filer til `.gitignore` så du ikke committer secrets til git!
