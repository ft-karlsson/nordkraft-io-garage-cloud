# Persistent Storage

## Quick start

```bash
# Deploy med persistent storage
nordkraft deploy postgres:15 --port 5432 --persistence --volume-path /var/lib/postgresql/data
```

`--persistence` giver containeren én volume, der monteres på `--volume-path` og overlever
genstarter og `nordkraft upgrade`. Data ligger på den node containeren kører på.

!!! note "Mark I"
    I Mark I er storage uencrypteret. LUKS-krypteret storage kommer i Mark II.

## Backup og gendannelse

En backup er hele volumen som én fil på din egen maskine. Jeg gemmer ingen kopier på
platformen — filen er din, og du bestemmer hvor den ligger.

```bash
# Hent en komplet kopi af volumen
nordkraft backup campfire
# → ./campfire-20261026T1840.tar.zst

# Læg den tilbage i samme container
nordkraft restore campfire campfire-20261026T1840.tar.zst

# Eller start en ny container med dataene
nordkraft deploy ghcr.io/basecamp/once-campfire:latest --port 3000 \
  --persistence --volume-path /rails/storage \
  --volume-from campfire-20261026T1840.tar.zst
```

Filen er almindelig `tar` komprimeret med `zstd` og kan åbnes uden mine værktøjer:
`zstd -dc campfire-20261026T1840.tar.zst | tar -t`.

**Konsistens.** Kører noden på LVM, tager jeg kopien fra et øjebliksbillede, så der ingen
nedetid er, og databaser kommer op som efter en strømafbrydelse (SQLite og PostgreSQL klarer
det). Ellers kopieres den kørende volume, og kommandoen siger `live` — stop containeren
først, hvis du vil være helt sikker på en konsistent kopi af en database.

**Gendannelse er sikker.** Arkivet pakkes ud ved siden af den eksisterende volume og
verificeres med sha256, før mapperne byttes. Fejler upload, checksum eller udpakning, er
volumen præcis som før. Containeren stoppes imens og startes igen bagefter.

Se [CLI reference](../reference/cli.md#backup) og [API reference](../reference/api.md#volume-backup-og-gendannelse).
