# Container Problemer

## Container starter ikke

### Tjek logs

```bash
nordkraft logs CONTAINER_NAVN
```

### Almindelige årsager

| Fejl | Løsning |
|------|---------|
| Image not found | Tjek image navn er korrekt og public |
| Port already in use | Brug en anden port |
| Out of memory | Øg `--memory` limit |
| Permission denied | Image kræver muligvis root (ikke supporteret) |

### Test lokalt først

```bash
podman run -it IMAGE_NAVN
```

---

## "Image not found"

### Tjek image er public

Hvis du bruger GitHub Container Registry:

1. Gå til `github.com/USERNAME?tab=packages`
2. Klik på dit package
3. Tjek visibility er "Public"

### Tjek image navn

```bash
# Korrekt format
nordkraft deploy ghcr.io/username/image:tag --port 80

# IKKE
nordkraft deploy username/image:tag --port 80
```

---

## Container crasher

### Tjek logs

```bash
nordkraft logs CONTAINER_NAVN --lines 200
```

### Almindelige årsager

- **Exit code 137** - Out of memory, øg `--memory`
- **Exit code 1** - App fejl, tjek logs
- **Exit code 127** - Command not found i container

---

## Kan ikke nå container

### Tjek container kører

```bash
nordkraft list
```

Status skal være "running".

### Tjek IP

```bash
curl http://CONTAINER_IP:PORT
```

### Tjek port

Er du sikker på containeren lytter på den port du angav?

---

## Stadig problemer?

Email support@nordkraft.io med:

- Container navn
- Image navn
- Output fra `nordkraft logs`
- Hvad du forventede vs hvad der skete
