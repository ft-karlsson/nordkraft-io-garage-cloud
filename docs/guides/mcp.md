# Lad en AI-agent styre dine containere (MCP)

`nordkraft` er bygget til at blive kørt af et menneske i en terminal. Men mere
og mere arbejde bliver gjort af agenter: Claude Code, Claude Desktop, Cursor
og lignende værktøjer, der selv finder ud af hvilke kommandoer der skal køres.
De taler alle MCP, *Model Context Protocol*, en lille standard for hvordan en
agent opdager og kalder værktøjer.

CLI'en har derfor en MCP-server indbygget: `nordkraft mcp`. Den kører på din
egen maskine, startes af agenten og taler med platformen gennem den
WireGuard-tunnel du allerede har. Der er ingen API-nøgle at oprette eller
passe på. Agenten kan præcis det du selv kan med `nordkraft`, og kun mens
din tunnel er oppe.

!!! info "Én binær, én opførsel"
    Agenten og du kalder de samme funktioner inde i CLI'en. En container deployet
    af agenten får den samme `.nk`-spec og det samme alias som en du selv
    deployer, og `nordkraft update` opdaterer MCP-serveren sammen med resten.

## Kom i gang

Du skal have kørt `nordkraft setup` først. Så:

```bash
nordkraft mcp install
```

Kommandoen finder de agenter der er installeret på maskinen (Claude Desktop,
Claude Code, Cursor) og skriver en `nordkraft`-indgang i deres MCP-konfiguration.
Der tages en `.bak`-kopi af filen før den ændres. Genstart appen, eller start en
ny Claude Code-session, og prøv:

> *Vis mine Nordkraft-containere*

Går noget galt, viser `nordkraft mcp doctor` i ét hug om setup er lavet, om
tunnelen og API'et svarer, og om hver agent har indgangen:

```text
🩺 Nordkraft MCP doctor

   ✓ setup            dig@example.com (~/.nordkraft/connection.json)
   ✓ api              http://172.20.0.254:8001/api answers (controller 0.3.53)
   ✓ binary           /usr/local/bin/nordkraft
   ✓ Claude Desktop   configured (~/Library/Application Support/Claude/claude_desktop_config.json)
   ✓ Claude Code      configured (~/.claude.json)
   ✓ Cursor           not installed on this machine
```

### Manuel opsætning

Bruger du en anden agent, er indgangen den samme overalt. `nordkraft mcp
install --print` skriver den ud; den ser sådan her ud:

```json
{
  "mcpServers": {
    "nordkraft": {
      "command": "/usr/local/bin/nordkraft",
      "args": ["mcp"]
    }
  }
}
```

Stien er absolut med vilje: programmer startet fra Finder ser ikke din shells
`PATH`, så `nordkraft` alene ville ikke blive fundet af Claude Desktop.

## Hvad agenten kan

Værktøjerne falder i tre grupper. De første to ændrer intet på platformen og
kan agenten kalde frit.

**Læse**

| Værktøj | Gør |
|---------|-----|
| `list_containers` | Dine containere med status, image, IP'er, porte og watt, plus alias-tabellen |
| `inspect_container` | Alt om én container: node, runtime, porte med URL'er, grænser, volumes, miljø |
| `get_logs` | De sidste linjer af loggen |
| `get_usage` | Planens grænser, hvad der er brugt, og forholdet |
| `get_events` | Deploy-hændelser, nyeste først; her står årsagen når noget fejlede |
| `list_ingress`, `ingress_status` | HTTPS-ruter |
| `suggest_port` | Den port et kendt image plejer at lytte på |

**Planlægge i `.nk`-specs**

| Værktøj | Gør |
|---------|-----|
| `list_specs`, `read_spec` | De gemte specs og én spec som JSON |
| `spec_set`, `spec_unset` | Sæt eller fjern et felt: `resources.memory`, `network.ports +443`, `env.PORT` … |
| `diff_spec` | Hvad der er forskelligt mellem spec og den kørende container |
| `init_spec` | Lav en spec fra en kørende container, når der ikke findes én lokalt |

**Ændre**

| Værktøj | Gør |
|---------|-----|
| `deploy`, `deploy_from_spec` | Ny container, direkte eller fra en spec |
| `upgrade` | Anvend specen på den kørende container |
| `start_container`, `stop_container` | Som `nordkraft start` og `stop` |
| `enable_ingress`, `disable_ingress` | Giv containeren en HTTPS-adresse, eller tag den væk |
| `remove_container` | Fjern container og volume |

Tre regler gælder for den sidste gruppe:

- **Porte er påkrævede ved deploy.** En container uden åben port kan ikke nås
  gennem ingress, og agenten ville ellers kunne rapportere en URL der ikke
  svarer. Mangler porten, afvises kaldet med et hint om `suggest_port`, og
  intet sendes til platformen.
- **`dry_run`.** `deploy`, `deploy_from_spec`, `upgrade` og `enable_ingress`
  kan kaldes med `dry_run: true` og svarer så med præcis den forespørgsel de
  ville have sendt, uden at sende den. Det er sådan agenten viser dig planen
  først.
- **`remove_container` kræver `confirm: true`.** Uden den afvises kaldet, og
  agenten bliver bedt om at spørge dig.

Resten af CLI'en, `setup`, `connect`, `disconnect`, `reset`, `registry
destroy`, `update` og alt under `account`, findes ikke som værktøjer. Det er
ting et menneske gør.

## Sådan ser et forløb ud

> *Deploy Vaultwarden med persistent storage og giv den et subdomæne*

Agenten kalder `suggest_port` (eller ved at Vaultwarden lytter på 80), så
`deploy` med `persistence: true` og `volume_path: /data`, derefter
`enable_ingress` med et subdomæne, og svarer med adressen. Specen ligger nu i
`~/.nordkraft/deployments/`, som havde du kørt `nordkraft deploy` selv.

> *Giv den 1 GB hukommelse*

`spec_set` sætter `resources.memory = 1g`, `diff_spec` viser at det er den
ene ændring, og `upgrade` anvender den. Revisionen tælles op i specen, og
`nordkraft specs` i din terminal viser det samme.

## Når tunnelen er nede

Alle værktøjer fejler på samme måde, med den besked CLI'en selv giver:

```text
Cannot reach the API. Is WireGuard connected? (nordkraft connect)
```

Agenten kan ikke selv forbinde; det er din nøgle og din tunnel. Kør
`nordkraft connect`, og prøv igen.

## Hvor serveren kører

`nordkraft mcp` kører på din maskine og læser `~/.nordkraft/` der. Det er
hele adgangsmodellen: den agent der må starte programmer på din maskine, må
også styre dine containere. Alt det den gør, står i `nordkraft events` og i
specs på disken, nøjagtig som dine egne kommandoer.
