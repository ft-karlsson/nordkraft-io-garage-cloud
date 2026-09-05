# Databaser

!!! info "Kommer snart"
    Denne guide er under udarbejdelse.

## Quick start

```bash
# PostgreSQL
nordkraft deploy postgres:15-alpine --port 5432 --persistent \
  --env POSTGRES_PASSWORD=mysecretpassword

# MySQL
nordkraft deploy mysql:8 --port 3306 --persistent \
  --env MYSQL_ROOT_PASSWORD=mysecretpassword

# Redis
nordkraft deploy redis:alpine --port 6379
```

`--persistent` sikrer at data gemmes selvom containeren genstarter.
