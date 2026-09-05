# Hvordan Garage Cloud Virker

!!! info "Kommer snart"
    Denne guide er under udarbejdelse.

## Arkitektur oversigt

```
Din maskine → WireGuard VPN → API Server → host node (dell f.eks. optiplex) → Container Runtime → Din container -> source kode
```

## Komponenter

### WireGuard VPN
Krypteret tunnel mellem dig og Garage Cloud. Din (wireguard) IP-adresse sammen med din public key bliver din identitet.

### API Server
Modtager kommandoer fra CLI og orkestrerer containere. Den vælger samtidig også én hvilke noder (host maskiner) som skal udgives containere på garage cloud. 

### Container Runtime
Kører dine containere isoleret med Kata Containers. (Under udvikling, nuværende er runc runtime)

### Netværk
Hver bruger får sin egen private subnet (172.21.x.0/24). Strenge firewall regler sikre isolation mellem subnets. Kun eksplicitte routes, hvis ingress er tilføjet mellem HAproxy loadbalancer og container ip-addresser. 

