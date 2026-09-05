# VPN Problemer

## Kan ikke forbinde til VPN

### Tjek 1: Er WireGuard installeret?

```bash
wg --version
```

Hvis ikke, se [Installation](../installation.md).

### Tjek 2: Er tunnel aktiv?

```bash
sudo wg show
```

Du burde se din tunnel med "latest handshake" inden for de sidste 2 minutter.

!!! info "På macOS hedder interfacet noget andet"
    `wg show nordkraft` fejler med "Unable to access interface" på macOS. Tunnelen kører som et `utun`-interface, og `nordkraft` er kun et navn wg-quick husker i `/var/run/wireguard/nordkraft.name`. Brug `sudo wg show all` i stedet. Kommandoen kræver `sudo` på macOS.

### Tjek 3: Kan du nå controller'en?

```bash
curl http://172.20.0.254:8001/api/status
```

Controller'en er den eneste adresse på management-nettet din tunnel ruter til, så det er den rigtige at teste imod.

Svarer den ikke, tjek:

- Din internetforbindelse
- Firewall blokerer ikke UDP port 51820 udgående
- Din WireGuard config er korrekt

---

## "Connection refused" fra CLI

### Tjek VPN er aktiv

```bash
sudo wg show all
```

### Tjek routing

Din tunnel ruter kun to ting: controller'en og dit eget container-subnet.

=== "macOS"

    ```bash
    netstat -rn -f inet | grep 172.2
    ```

=== "Linux"

    ```bash
    ip route | grep 172.2
    ```

Du burde se `172.20.0.254` og `172.21.<dit-slot>.0/24` peget på din tunnel (`utun…` på macOS, `nordkraft` på Linux). Ser du ingen af dem, er tunnelen nede.

### Genstart WireGuard

```bash
nordkraft disconnect
nordkraft connect
```

Eller direkte, hvis CLI'en ikke virker:

```bash
sudo wg-quick down nordkraft
sudo wg-quick up nordkraft
```

---

## Handshake fejler

Hvis `wg show` viser ingen "latest handshake":

1. **Tjek din public IP ikke har ændret sig** - Nogle ISPs giver dynamisk IP
2. **Tjek endpoint er korrekt** - Skal være `cloud.nordkraft.io:51820`
3. **Kontakt support** med din config (UDEN private key!)

---

## Stadig problemer?

Email support@nordkraft.io med:

- Output fra `sudo wg show all`
- Output fra `curl http://172.20.0.254:8001/api/status`
- Din config fil (fjern PrivateKey linjen!)

---

## "wg-quick: Version mismatch: bash 3 detected"

Rammer kun macOS med CLI ældre end 0.3.40. `wg-quick` kræver bash 4+, og macOS leverer bash 3.2 — hvis Homebrews bash ikke ligger først i din PATH, nægter scriptet at køre.

```bash
nordkraft update
```

Nyere versioner peger selv `wg-quick` på den rigtige bash. Kan du ikke opdatere, virker denne som midlertidig løsning:

```bash
brew install bash
sudo env PATH="$(brew --prefix)/bin:$PATH" wg-quick up nordkraft
```
