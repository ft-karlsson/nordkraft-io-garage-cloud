# Installation

Sådan kommer du i gang med NordKraft.io på under 5 minutter.

---

## Sådan fungerer det

NordKraft.io bruger et **token-baseret onboarding flow**: når du signer up, får du en engangs-token (starter med `NKINVITE-`). Du kører ét kommando i terminalen, og resten sker automatisk:

1. CLI'en bliver installeret
2. En ny WireGuard-nøgle genereres **lokalt på din maskine** (private key forlader aldrig din computer)
3. Token bliver claimed, og din WireGuard peer bliver provisioneret på controller'en
4. WireGuard-tunnelen bliver sat op og aktiveret
5. Du er forbundet og klar til at deploye containere

Du behøver ikke installere WireGuard manuelt eller importere en `.conf` fil.

---

## Forudsætninger

- En NordKraft.io konto — sign up på [cloud.nordkraft.io](https://cloud.nordkraft.io)
- macOS eller Linux
- Terminal/kommandolinje adgang
- `sudo` rettigheder (til at konfigurere WireGuard-tunnelen)

!!! info "Windows support"
    Windows er ikke officielt understøttet endnu — onboarding-scriptet kræver bash og `wg-quick`. Windows-brugere kan bruge WSL2 eller kontakte os for manuel opsætning.

---

## Trin 1: Sign up og få din token

1. Gå til [cloud.nordkraft.io](https://cloud.nordkraft.io) og opret en konto
2. Efter signup får du en token der ser sådan her ud:
   ```
   NKINVITE-3f7a2c1e-8b4d-4e9f-a123-456789abcdef
   ```
3. Kopiér tokenet — du skal bruge det i næste trin

!!! warning "Tokenet er engangsbrug"
    Hver token kan kun claims én gang. Skal du flytte til en ny maskine, så læs [Ny maskine / mistet config](troubleshooting/recovery.md) først — har du stadig din private nøgle, behøver du ikke en ny token.

---

## Trin 2: Kør installationen

Kør dette i din terminal og erstat `NKINVITE-...` med dit eget token:

```bash
curl -fsSL https://cloud.nordkraft.io/install.sh | sh -s NKINVITE-3f7a2c1e-8b4d-4e9f-a123-456789abcdef
```

Scriptet gør følgende automatisk:

- Henter den rigtige CLI binary til din platform (macOS arm64/amd64, Linux amd64)
- Installerer den i `/usr/local/bin/nordkraft`
- Installerer WireGuard hvis det mangler (via Homebrew på macOS, apt/dnf på Linux)
- Genererer en ny WireGuard nøgle-par lokalt
- Sender **kun den offentlige nøgle** til controller'en og claimer tokenet
- Skriver WireGuard config til `~/.nordkraft/wg.conf` og kopierer den til systemets WireGuard-mappe (`/etc/wireguard/` på Linux, `/usr/local/etc/wireguard/` på macOS)
- Aktiverer tunnelen med `wg-quick up nordkraft`
- Skriver din konfiguration til `~/.nordkraft/connection.json`

Forventet output ved succes:

```
✓ CLI installeret i /usr/local/bin/nordkraft
✓ WireGuard nøgle genereret
✓ Token claimed — peer provisioneret
✓ WireGuard tunnel aktiveret
✓ Sikker forbindelse etableret til Dit Navn!

Kom i gang med: nordkraft --help
```

---

## Trin 3: Verificér at alt virker

```bash
nordkraft auth status
```

Forventet output:

```
✓ Forbundet som: Dit Navn
  Plan: founder-plan
  Controller: 172.20.0.254:8001
```

Test at du kan liste containere (tom liste er forventet første gang):

```bash
nordkraft list
```

---

## Næste skridt

Du er klar! 🚀

- [Din første container](getting-started.md) — deploy en app på 30 sekunder
- [CLI reference](reference/cli.md) — alle tilgængelige kommandoer
- [Webapp guide](guides/webapp.md) — deploy en rigtig webapplikation med HTTPS

---

## Avanceret: Manuel installation

Hvis du vil installere CLI'en uden at claim en token (fx på en anden maskine hvor du allerede har en fungerende WireGuard forbindelse), kan du hente binary'en direkte fra GitHub Releases.

Download fra [GitHub Releases](https://github.com/ft-karlsson/nordkraft-io-garage-cloud/releases/latest):

| Platform | Fil |
|----------|-----|
| macOS (Apple Silicon) | `nordkraft-darwin-arm64.tar.gz` |
| macOS (Intel) | `nordkraft-darwin-amd64.tar.gz` |
| Linux (AMD64) | `nordkraft-linux-amd64.tar.gz` |
| Linux (ARM64) | `nordkraft-linux-arm64.tar.gz` |

```bash
tar -xzf nordkraft-*.tar.gz
sudo mv nordkraft /usr/local/bin/
nordkraft --version
```

Du skal stadig have en fungerende WireGuard forbindelse til `172.20.0.254:8001` for at CLI'en kan snakke med controller'en.

---

## Opdatering af CLI'en

```bash
nordkraft update
```

Denne kommando henter den nyeste release fra GitHub og erstatter binary'en. Din WireGuard konfiguration og dine specs i `~/.nordkraft/` bliver ikke rørt.

---

## Fejlfinding

### Installationsscriptet fejler med "token already claimed"

Hver `NKINVITE-` token kan kun bruges én gang. Hvis du allerede har kørt scriptet på en anden maskine med samme token, skal du bede om en ny. Kontakt <frederikkarlsson@me.com>.

### "Command not found: nordkraft" efter installation

CLI'en er ikke i din PATH. Tjek hvor den blev installeret:

```bash
ls -la /usr/local/bin/nordkraft
```

Hvis den ligger der men ikke findes af din shell, tilføj `/usr/local/bin` til din PATH i `~/.zshrc` eller `~/.bashrc`:

```bash
export PATH="$PATH:/usr/local/bin"
```

### WireGuard tunnel er ikke aktiv efter installation

Tjek status:

=== "macOS"

    ```bash
    sudo wg show nordkraft
    ```

=== "Linux"

    ```bash
    sudo wg show nordkraft
    sudo systemctl status wg-quick@nordkraft
    ```

Hvis tunnelen er nede, prøv at starte den manuelt:

```bash
sudo wg-quick down nordkraft
sudo wg-quick up nordkraft
```

### "Connection refused" ved `nordkraft auth status`

Dette betyder typisk at WireGuard-tunnelen er nede eller at controller'en ikke er tilgængelig.

1. Tjek tunnel: `sudo wg show nordkraft` — du skal se en "latest handshake" inden for de sidste minutter
2. Test HTTP direkte: `curl http://172.20.0.254:8001/api/status`
3. Tjek din firewall ikke blokerer UDP port 51820 udgående (dette er den hyppigste årsag — nogle virksomhedsnetværk og cafeer blokerer WireGuard)
4. Prøv fra et andet netværk

### Ingen handshake i WireGuard

Hvis `sudo wg show` ikke viser en "latest handshake" eller den er mere end et par minutter gammel:

- Din firewall blokerer sandsynligvis UDP port 51820 udgående
- Prøv fra en mobil hotspot for at udelukke netværksproblemer
- Kontakt <frederikkarlsson@me.com> hvis problemet vedvarer

### Jeg vil afinstallere NordKraft.io CLI'en

Nemmest er at lade CLI'en gøre det:

```bash
nordkraft reset
```

Den lukker tunnelen og fjerner al lokal konfiguration. Vil du gøre det i hånden:

```bash
# Stop og fjern WireGuard tunnelen
sudo wg-quick down nordkraft
sudo rm -f /etc/wireguard/nordkraft.conf /usr/local/etc/wireguard/nordkraft.conf

# Fjern CLI binary
sudo rm /usr/local/bin/nordkraft

# (Valgfrit) Fjern lokal konfiguration og specs
rm -rf ~/.nordkraft
```

!!! warning "Før du afinstallerer"
    `~/.nordkraft/aliases.json` findes kun på din maskine og kan ikke genskabes — tag en kopi hvis du vil beholde dine aliaser. Se [Ny maskine / mistet config](troubleshooting/recovery.md).

    Hvis du har kørende containere på NordKraft.io, bliver de ikke stoppet af at afinstallere CLI'en lokalt. Kør `nordkraft rm <container>` for hver container først, eller kontakt os for at lukke din konto helt.
