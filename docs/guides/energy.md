# Se watt og CO₂ per container

Hver node i garagen sidder bag en strømmåler. Noden fortæller hvor meget
CPU-tid hver container har brugt, og de to ting lægges sammen: din container
får sin andel af det, maskinen reelt trak fra væggen i det minut, og CO₂
regnes med Energinets tal for strømmen i netop det øjeblik.

```bash
nordkraft energy
```

```text
⚡ Energy

   Now: 12.3 W
   Last 24h: 287 Wh  ·  31 g CO₂
   Grid now: 43 g CO₂/kWh in DK1

   campfire                  9.1 W     201 Wh   22 g CO₂
   uptime-kuma               3.2 W      86 Wh    9 g CO₂
```

Én container ad gangen, med timer:

```bash
nordkraft energy campfire --since 7d
```

Og i `nordkraft ps` står `Power: 9.1 W` på hver container, når noden er målt.

## Hvad tallene er

**Målt, ikke gættet.** Watt-tallet for noden kommer fra en måler i
stikkontakten. Når måleren tier, bliver der ikke regnet noget ud: så mangler
der et minut, og det kan du se, i stedet for at et tal bliver opfundet.

**Din containers andel.** En måler måler maskinen, ikke containeren. Derfor
fordeles maskinens watt sådan:

- Det maskinen trækker i tomgang deles efter reserveret vCPU (`--cpus` ved
  deploy). En container der sover, bærer stadig sin del af at maskinen er
  tændt.
- Resten deles efter den CPU-tid hver container faktisk brugte i minuttet.

Det er en fordeling, og CLI'en siger det hver gang. Summen af alle
containere på en node er aldrig mere end det, noden trak.

**CO₂.** Energinet udgiver hvert femte minut hvor mange gram CO₂ et kWh
koster i DK1 (vest for Storebælt) og DK2. Hvert minut prissættes med det
tal. En vindblæst nat ligger det under 50 g/kWh; en stille vinteraften kan
det være over 200. Det er derfor to containere med samme forbrug kan have
forskelligt CO₂: det afhænger af hvornår de arbejdede.

## Vinduer

`--since` tager `90m`, `24h`, `7d`, `30d`. Op til syv dage læses minut for
minut; længere vinduer læses fra timeopgørelsen, som gemmes for altid.

## Når der ikke står noget

- **"no reading"**: noden har ingen frisk måling i de sidste tre minutter.
  Måleren eller noden er stille. Det retter sig selv når de melder igen.
- **"CO₂ n/a"**: Energinet kunne ikke nås, så minuttet er gemt uden CO₂.
  CLI'en skriver hvor mange intervaller det gælder.
- **Ingen linjer endnu**: tallene kommer et par minutter efter at både
  måler og node har meldt første gang.

!!! info "Helt ærligt"
    Watt per container er en andel af en rigtig måling, ikke en måling af
    containeren selv. Metoden står i hvert svar fra API'et (`method`), så du
    kan se præcis hvad der er regnet.

## API

```bash
curl http://172.20.0.1:8080/api/energy?since=24h
curl http://172.20.0.1:8080/api/containers/campfire/energy?since=7d
```

Begge svarer med `now`, `total` (`wh`, `co2_g`), `grid` (g CO₂/kWh lige nu)
og `method`. Container-svaret har også `hourly[]`.
