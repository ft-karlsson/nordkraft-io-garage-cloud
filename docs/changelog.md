# Hvad er nyt

Ændringer på platformen, nyeste først. Versionsnumrene følger CLI'en — server-binæren udgives sammen med den.

---

## September 2026 — v0.3.38 → v0.3.40

**Genskab din opsætning fra serveren**

```bash
nordkraft init <container> --from-server
```

Controller'en gemmer den config hver container blev deployet med. `--from-server` bygger din `.nk` spec ud fra den i stedet for fra den kørende container — det giver den nøjagtige konfiguration, inklusive `volume_size` som ikke kan aflæses fra runtime, og specen beholder serverens revisionsnummer.

Ny guide: [Ny maskine / mistet config](troubleshooting/recovery.md) — hvad der ligger i `~/.nordkraft`, hvad der kan genskabes hvorfra, og hvordan du kommer tilbage efter et maskinskifte.

**Rigtige ressourcegrænser**

`nordkraft init` skrev tidligere standardværdier (0.5 cpu / 512m) når den ikke kunne aflæse containerens faktiske grænser. Nu læses de dér hvor runtime rent faktisk håndhæver dem — og kan de ikke læses, siger CLI'en det i stedet for at gætte.

**Installation og opdatering**

- Installationsscriptet virker nu på en ny Apple Silicon Mac, hvor `/usr/local/bin` ikke findes i forvejen
- `nordkraft connect` og `nordkraft disconnect` virker uanset hvordan din PATH er sat op
- `nordkraft update` henter fra det rigtige repository

**Dokumentation**

Dokumentationen er flyttet ind i samme repository som koden, otte kommandoer der aldrig havde været dokumenteret er kommet med (`alias`, `registry`, `push`, `setup`, `connect`, `disconnect`, `reset`, `update`), og et par forkerte adresser i fejlfindingen er rettet.

---

## Juli 2026

Sikkerhedsopdatering i controller'en.

---

## April 2026 — v0.3.33 → v0.3.37

**Deklarative deployments**

Dine containere kan nu beskrives i en `.nk` spec, som du redigerer og anvender fra CLI'en:

```bash
nordkraft init web
nordkraft spec-set web resources.memory 1g
nordkraft diff web
nordkraft upgrade web
```

`upgrade` bevarer IP og volumes — containeren genstarter med den nye konfiguration i stedet for at blive deployet forfra.

**Privat registry**

`registry://` resolver nu til dit eget private image-registry, så dine egne images kan bruges direkte i et deploy uden at gå omvejen om en offentlig registry.

**Stabilitet**

- Deployments der fejler, bliver vist med årsagen i stedet for at forsvinde fra listen
- Logs fra agenten er tilgængelige mens et deployment kører
- Rettet en race condition i deployment-flowet

---

## Marts 2026

Platformen blev open source. CLI og controller/agent samlet i ét repository, og routing bliver nu forligt med databasen ved opstart, så ruter overlever en genstart af controller'en.

---

## Februar 2026

Mark I-testen afsluttet: zero-trust authentication med WireGuard, Kata VM-isolation, dual-stack IPv4/IPv6, persistent storage, HTTPS ingress og multi-node orkestrering via NATS.
