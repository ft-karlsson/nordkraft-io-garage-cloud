# Custom domæner & HTTPS

Der er to måder at få dit projekt online med HTTPS: et subdomæne under
`nordkraft.cloud` (hurtigst), eller **dit eget domæne** som `mitfirma.dk`.

---

## Subdomæne — hurtigst i gang

```bash
nordkraft ingress enable myapp --subdomain coolsite

# Resultat: https://coolsite.nordkraft.cloud
```

Automatisk TLS-certifikat, klar med det samme. Godt til test, demoer og
projekter der ikke behøver eget navn.

---

## Dit eget domæne

Peg dit eget domæne — fx `mitfirma.dk` — direkte på en container. Du får:

- **Automatisk HTTPS** med et rigtigt Let's Encrypt-certifikat
- **Automatisk fornyelse** — certifikatet fornys 30 dage før udløb, uden at du gør noget
- **Automatisk omdirigering** fra `http://` til `https://`
- Løbende overvågning: peger dit domæne pludselig et andet sted hen, opdager platformen det

### Trin 1 — registrér domænet

```bash
nordkraft domain add mitfirma.dk --container myapp
```

Peger din app på en anden port end 80, tilføjer du `--port 3000`.

Kommandoen svarer med det samme og viser præcis de to DNS-records, du skal
oprette:

```
✅ Domain registered!

   TXT  _nordkraft-challenge.mitfirma.dk
        value: nk-verify=<din-unikke-kode>

   A    mitfirma.dk
        value: <platformens IP>
```

### Trin 2 — opret DNS-records hos din udbyder

Log ind hos din domæneudbyder (one.com, Cloudflare, Simply.com …) og opret:

| Type | Navn | Værdi |
|------|------|-------|
| TXT | `_nordkraft-challenge` | `nk-verify=...` (koden fra kommandoen) |
| A | `@` (roden af domænet) | IP-adressen fra kommandoen |

Bruger du et **subdomæne** (fx `app.mitfirma.dk`), kan du oprette en CNAME
til dit `*.nordkraft.cloud`-navn i stedet for A-recorden.

!!! warning "Klassisk fælde: udbyderen tilføjer selv domænet"
    De fleste DNS-paneler sætter selv `.mitfirma.dk` efter det, du skriver i
    navnefeltet. Skriv altså **kun** `_nordkraft-challenge` — ikke
    `_nordkraft-challenge.mitfirma.dk` — ellers ender recorden på
    `_nordkraft-challenge.mitfirma.dk.mitfirma.dk`. For A-recorden lader du
    typisk navnefeltet stå **tomt**.

    Har din udbyder en standard-A-record der peger på deres egen parkeringsside,
    skal den slås fra.

### Trin 3 — følg med

Verificering og certifikat kører helt automatisk. Tjek fremdriften:

```bash
nordkraft domain status mitfirma.dk
```

Vil du ikke vente på næste automatiske tjek:

```bash
nordkraft domain verify mitfirma.dk
```

Den viser med ✓/✗ om begge records er fundet — direkte fra dit domænes
autoritative navneservere, så du behøver ikke vente på global DNS-udbredelse.

Typisk er domænet live **5–30 minutter** efter dine records er oprettet.

**Status-oversigt:**

| Status | Betyder |
|--------|---------|
| `pending_dns` | Venter på dine DNS-records |
| `dns_verified` | DNS bekræftet — certifikat bestilles |
| `issuing_cert` | Let's Encrypt udsteder certifikatet |
| `activating` | Routing sættes op og efterprøves |
| `active` | Live — `https://mitfirma.dk` virker |
| `degraded` | Dit domæne peger ikke længere på platformen |

### www og roddomæne

`mitfirma.dk` og `www.mitfirma.dk` er to forskellige navne — tilføj dem hver
for sig, hvis du vil have begge:

```bash
nordkraft domain add www.mitfirma.dk --container myapp
```

### Overblik og fjernelse

```bash
nordkraft domain list
nordkraft domain remove mitfirma.dk
```

`remove` rydder alt op igen: routing, certifikat og registrering.

### Sådan er det sikret

- Ingen trafik routes, før du har **bevist ejerskab** af domænet (TXT-koden
  tjekkes mod domænets autoritative navneservere — og Let's Encrypt
  efterprøver det uafhængigt, før certifikatet udstedes).
- Dit domæne matches **præcist** — aldrig med wildcards. Der er ingen måde,
  et andet domæne kan ende i din container.
- Holder dit domæne op med at pege på platformen, fjernes routing og
  certifikat automatisk efter en periode — en senere ejer af domænet kan
  aldrig modtage din trafik.

---

## IPv6 direkte adgang

Vil du helt udenom proxy og certifikater:

```bash
nordkraft ipv6 open myapp

# Resultat: http://[2a05:f6c3:444e::xxxx]/
```

Global IPv6-adresse uden NAT. Containeren står selv for evt. TLS.

---

## Fejlfinding

**Hænger i `pending_dns`?** Kør `nordkraft domain verify` og se hvilken
record der mangler. Langt de fleste gange er det fælden fra advarslen
ovenfor — recorden er endt med domænet to gange.

**`degraded`?** Dit domæne svarer ikke længere med platformens IP. Ret
A-recorden hos din udbyder, så aktiveres domænet automatisk igen.
