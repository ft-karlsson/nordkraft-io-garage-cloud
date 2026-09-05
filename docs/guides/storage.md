# Persistent Storage

!!! info "Kommer snart"
    Denne guide er under udarbejdelse.

## Quick start

```bash
# Deploy med persistent storage
nordkraft deploy postgres:15 --port 5432 --persistent
```

Med `--persistent` flag'et gemmes data i `/data` mappen i containeren, og overlever genstarter.

!!! note "Mark I"
    I Mark I er storage uencrypteret. LUKS-krypteret storage kommer i Mark II.
