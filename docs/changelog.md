# Hvad er nyt

Ændringer på platformen, nyeste først. Versionsnumrene følger CLI'en — server-binæren udgives sammen med den.

---

## Oktober 2026 — v0.3.44

**`exec` holder sin timeout — også på tværs af noder**

En `exec` mod en container på en anden node end controlleren ignorerede `timeout_s`; `sleep 10`
med timeout 2 kørte alle 10 sekunder. Rettet, og e2e-testen kører nu også relæ-vejen.

**`deploy` med `"wait": true`**

Svarer først når containeren kører og `exec` virker. Uden flaget er deploy som før: svar med det
samme, hent status med `nordkraft list`. Fundet med den første Mastra-provider mod garagen.

---

## Oktober 2026 — v0.3.43

**Shell og exec i dine containere**

`nordkraft shell myapp` giver dig en rigtig terminal i containeren, `nordkraft exec myapp -- cmd`
kører én kommando og afslutter med dens exit-kode. Begge er bygget som `kubectl exec`:
én strøm over WireGuard, som controlleren sender uændret videre til den node containeren
kører på, og som noden kobler direkte på containerd. Ingen ekstra login — netværket er
adgangskontrollen. Virker ens for Kata og runc.

Til scripts og orkestratorer findes det samme som `POST /containers/{id}/exec` (bufferet,
med `cwd`, `env`, `stdin`, `timeout_s`) og som WebSocket på `/containers/{id}/stream`.

**Livscyklus i spec'en**

En container kan nu rydde op efter sig selv. I `.nk`-spec'en eller som flag på `deploy`:

```toml
[lifecycle]
restart      = "never"   # always | unless-stopped | on-failure | never
auto_stop    = "15m"     # stop efter 15 min uden shell/exec-aktivitet
auto_delete  = "1h"      # slet 1 time efter stop
max_lifetime = "24h"     # stop senest 24 timer efter oprettelse

[labels]
workspace = "ws-42"
```

`nordkraft list --label workspace=ws-42` finder dem igen, og `nordkraft wait myjob` venter på
at et job bliver færdigt og giver dig dets exit-kode. Se [Sandboxes](guides/sandboxes.md).

**Til dig der bygger selv:** `protobuf-compiler` er nu en build-afhængighed (containerd's gRPC-bindings).

---

## Oktober 2026 — v0.3.42

**Privat registry med adgangskode**

`nordkraft registry init` opretter nu et registry der kræver brugernavn og adgangskode.
CLI'en genererer dem, gemmer dem i `~/.nordkraft/registry.json` og bruger dem selv ved
`push` og `deploy registry://…`. Når platformen henter et image fra dit registry, sker
det med dine credentials og kun dem — intet andet registry får dem præsenteret.

Har du et registry fra før? Det er stadig åbent og virker som hidtil. Vil du have det
beskyttet: `nordkraft registry destroy`, `nordkraft registry init`, og push dine images
igen.

**Hvor må et image komme fra?**

Et deploy accepterer nu kun images fra dit eget registry (`registry://`) eller fra
offentlige registries angivet med hostnavn. IP-adresser uden for dit eget
container-subnet afvises, før noget bliver rørt. Platformen henter dermed kun images
på dine vegne fra steder du selv råder over.

---

## September 2026 — v0.3.41

**Brug dit eget domæne**

Peg dit eget domæne direkte på en container. Du opretter to DNS-records hos
din udbyder — resten sker automatisk: ejerskabet bekræftes mod domænets
autoritative navneservere, et rigtigt Let's Encrypt-certifikat udstedes og
fornys af sig selv, `http://` omdirigeres til `https://`, og platformen
holder løbende øje med at routing og certifikat er som de skal være.

```bash
nordkraft domain add mitfirma.dk --container myapp
nordkraft domain status mitfirma.dk   # følg med: pending_dns → … → active
```

Domænet matches præcist (aldrig wildcards), intet routes før ejerskabet er
bevist, og holder domænet op med at pege på platformen, ryddes der
automatisk op. Typisk live 5–30 minutter efter dine DNS-records er oprettet.

Ny guide: [Custom domæner & HTTPS](guides/domains.md). Første domæne i
drift: [garagecloud.dk](https://garagecloud.dk) — som selvfølgelig selv
kører på platformen.

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
