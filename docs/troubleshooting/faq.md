# FAQ

## Generelt

### Hvad er Garage Cloud?

En dansk container-platform bygget på genbrugt hardware. Deploy dine apps med én kommando, sikret med WireGuard VPN.

### Hvor kører mine containere?

På fysisk hardware i Ry, Danmark. Primært genbrugte Dell OptiPlex maskiner og Raspberry Pi's.

### Er det sikkert?

Ja. Alt trafik er krypteret via WireGuard VPN. Containere kører isoleret med Kata Containers. Se [Sikkerhed](../concepts/security.md).

---

## Priser & Planer

### Hvad koster det?

Se aktuelle priser på [nordkraft.io](https://nordkraft.io).

### Er der en gratis prøveperiode?

Kontakt os for at høre om muligheder.

---

## Teknisk

### Hvilke images kan jeg bruge?

Alle public Docker/OCI images. Private registries kommer i en senere version.

### Kan jeg bruge Docker?

Ja, vi bruger OCI-kompatible images. Du kan bygge med Docker eller Podman.

### Hvor meget ressourcer får jeg?

Afhænger af din plan. Default limits er 0.5 CPU og 512MB RAM per container.

### Kan jeg køre databaser?

Ja! Brug `--persistent` flag for at gemme data.

```bash
nordkraft deploy postgres:15 --port 5432 --persistent
```

### Understøtter I Windows containers?

Nej, kun Linux containers.

### Kan jeg SSH ind i mine containere?

Nej, men du kan se logs med `nordkraft logs`.

---

## Netværk

### Hvordan får jeg HTTPS?

```bash
nordkraft ingress enable myapp --subdomain mitsite
```

Automatisk TLS certifikat.

### Kan jeg bruge mit eget domæne?

Kommer i en senere version. Lige nu kun `*.nordkraft.cloud` subdomains.

### Hvad er IPv6 direct access?

Din container får en global IPv6 adresse uden NAT. Tilgængelig fra hele verden.

---

## Support

### Hvordan får jeg hjælp?

- Email: support@nordkraft.io
- GitHub Issues: [github.com/ft-karlsson/nordkraft-io-garage-cloud/issues](https://github.com/ft-karlsson/nordkraft-io-garage-cloud/issues)

### Hvem svarer på support?

Mennesker. Ingen chatbots.
