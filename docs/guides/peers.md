# Giv andre adgang til dine containere (peers)

Din egen WireGuard-nøgle er din identitet: den giver adgang til API'et, til
`nordkraft`-kommandoerne og til alle dine containere. Den skal du aldrig dele.

En **peer** er en ekstra WireGuard-nøgle, du udsteder til en telefon, en
kollegas laptop eller en CI-runner. Den får sin egen adresse i dit private
netværk og kan nå dine containere, alle eller udvalgte, og intet andet.

!!! info "Hvad en peer kan"
    - Nå dine containere direkte på deres `172.21.x.y`-adresser, som du selv gør
    - Begrænses til bestemte containere og bestemte porte
    - Lukkes eller slettes med én kommando, med øjeblikkelig virkning

!!! warning "Hvad en peer ikke kan"
    - Bruge `nordkraft` eller API'et: en peer er en netværksadgang, ikke en bruger

## Opret en peer

```bash
nordkraft peer add telefon --all --qr
```

Kommandoen laver et nøglepar på din maskine, sender kun den offentlige nøgle
til noden og skriver en WireGuard-konfiguration til
`~/.nordkraft/peers/telefon.conf`. Med `--qr` tegnes den også som QR-kode i
terminalen: åbn WireGuard-appen på telefonen, vælg *Tilføj tunnel → Opret ud
fra QR-kode*, scan, og slå tunnelen til. Så kan telefonen åbne
`http://172.21.<dit-slot>.<container>` med det samme.

Til en laptop eller en server importerer du filen i stedet, eller kører
`sudo wg-quick up ~/.nordkraft/peers/telefon.conf`.

Den private nøgle forlader aldrig din maskine, og noden ser den aldrig. Har
modtageren selv lavet et nøglepar, sender du blot den offentlige nøgle med:

```bash
nordkraft peer add anna --all --pubkey 'K7f…='
```

## Begræns hvad en peer må

`--all` giver adgang til alle dine containere, også dem du laver senere. Vil
du give mindre, nævner du containerne, og eventuelt porte:

```bash
nordkraft peer add ci --container web --ports 80,443 --container db
```

Adgangen følger containerens **navn**, ikke dens IP. Deployer du `web` igen
og den får en ny adresse, virker peerens adgang stadig. Findes der lige nu
ingen kørende container med navnet, viser `nordkraft peer show` den som
*dangling*, og adgangen kommer tilbage, når containeren kører igen.

Du kan ændre scope når som helst, uden at enheden skal gøre noget:

```bash
nordkraft peer allow ci --container cache
nordkraft peer revoke ci --container db
nordkraft peer set ci --all
nordkraft peer set ci --only web,db
```

## Lad dine containere kalde en peer

Som udgangspunkt går trafikken én vej: en peer må åbne forbindelser til dine
containere, og containerne må svare – men de må ikke selv ringe op til
peeren. Har du en maskine, dine containere *skal* kunne kalde, fx en
GPU-maskine med en sprogmodel bag et OpenAI-kompatibelt API, slår du det til
for netop den peer:

```bash
nordkraft peer add gpu-box --all --callable --callable-ports 11434
# eller bagefter:
nordkraft peer callable gpu-box --ports 11434
nordkraft peer callable gpu-box --off
```

Så kan en container nå `http://172.20.<slot>.<n>:11434` (adressen står i
`nordkraft peer show gpu-box`). Det gælder kun dine egne containere og kun den
ene peer; udelader du `--ports`, gælder det alle porte. Andre peers og andre
kunder rører det ikke.

## Se hvem der er på

```bash
nordkraft peer ls
nordkraft peer show telefon
```

`show` viser peerens adresse, seneste handshake, hvor den kobler på fra, hvor
meget trafik der er gået, og præcis hvilke containere den kan nå lige nu.

## Luk, slet eller skift nøgle

```bash
nordkraft peer disable telefon   # adgangen lukkes med det samme, peeren bevares
nordkraft peer enable telefon
nordkraft peer rotate telefon --qr   # nyt nøglepar, samme adresse, ny konfiguration
nordkraft peer rm telefon
```

Alle ændringer slår igennem, før kommandoen svarer. Roterer du nøglen,
mister den gamle konfiguration forbindelsen i samme sekund.

## Sådan hænger det sammen

| Hvad | Adresse |
|------|---------|
| Din egen nøgle | `172.20.0.<slot>` |
| Dine peers | `172.20.<slot>.2` til `172.20.<slot>.254` |
| Dine containere | `172.21.<slot>.0/24` |

Reglerne på noden tillader *peer → dine containere* (hele området eller de
containere, du har nævnt) og kun svar den anden vej. Med `--callable` kommer
der én regel mere for den peer: *dine containere → peeren*, kun TCP og kun på
de porte, du har angivet. Porte er ikke valgfri: en callable peer er aldrig
åben på alle porte, så SSH og admin-API'er på din maskine forbliver uden for
rækkevidde af en container, der kører fremmed kode.

### Peerens egen firewall

Noden slipper trafikken igennem, men maskinen i den anden ende skal også tage
imod den. Kører peeren fx Ubuntu med `ufw`, så åbn kun den port, det drejer
sig om, og kun fra dit containerområde på WireGuard-interfacet:

```bash
sudo ufw allow in on <wg-interface> from 172.21.<slot>.0/24 to any port 11434 proto tcp
```

Lad inference-API'et sidde bag en nøgle eller en lille proxy, hvis det ikke
selv kan kræve login. Det er dine containere, der kalder, og de kan køre kode,
du ikke har skrevet.

Konfigurationen, du giver videre, ruter kun dit containerområde ind i
tunnelen. Det er bekvemmelighed, ikke sikkerhed: det er noden, der håndhæver,
hvad en peer må, med regler der genereres ud fra det, du har sat op. En peer
kan ikke udgive sig for en anden adresse, for WireGuard afviser pakker, der
ikke kommer fra den adresse, nøglen har fået.

Din plan sætter et loft for antallet af peers. Rammer du det, siger
`nordkraft peer add` det selv.

Se også [CLI-referencen](../reference/cli.md#peer-kommandoer-ekstra-wireguard-adgang)
og [API-referencen](../reference/api.md#peers-ekstra-wireguard-adgang-til-dine-containere).
