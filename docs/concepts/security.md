# Sikkerhed

!!! info "Kommer snart"
    Denne guide er under udarbejdelse.

## Zero-Trust Arkitektur

Garage Cloud bygger på zero-trust principper:

- **Ingen direkte adgang** - Alt går gennem VPN
- **Kryptografisk identitet** - Din WireGuard public key er din identitet
- **Isolerede netværk** - Hver bruger har sit eget subnet
- **Container isolation** - Kata Containers giver VM-niveau isolation (UNDER udvikling - nuværende er runc )

## Sikkerhedslag

| Lag | Beskyttelse |
|-----|-------------|
| Netværk | WireGuard VPN, isolerede subnets |
| Container | Kata Containers, sikre isolation som en VM |
| Filsystem | Read-only root som default, tmpfs for temp data |
| Ressourcer | CPU/memory limits, PID limits |
