# Kendte Problemer og Status

Denne side viser kendte bugs og deres status fra den seneste Mark I test (Januar 2026).

---

## 🐛 Aktive Bugs

### Bug #1: CPU og Memory Limits Anvendes Ikke
**Severity:** 🔴 HØJ  
**Status:** Identificeret, fix i gang  
**Opdaget:** 24. Januar 2026

**Problem:**

```bash
nordkraft deploy postgres:alpine --cpu 1.0 --memory 1g
# Resultat: Resource limits sættes IKKE på containeren
```

**Årsag:** Container API sender ikke `--cpus` og `--memory` flag til nerdctl.

**Workaround:** Ingen for nu - alle containers får standard ressourcer (0.5 CPU, 512MB RAM).

**Fix ETA:** Næste release (v0.3.0)

---

### Bug #2: IPv6 Adresse Vises Forkert Efter Deploy
**Severity:** 🟡 MEDIUM  
**Status:** Identificeret, fix i gang  
**Opdaget:** 24. Januar 2026

**Problem:**

Deploy response viser forkert IPv6 adresse (fra database allocation, ikke den rigtige SLAAC adresse).

```bash
nordkraft deploy nginx:alpine --ipv6
# Viser: IPv6: 2a05:f6c3:444e:0:1:29:: ❌ (forkert)
# Rigtig: 2a05:f6c3:444e:0:8c2d:a3ff:fe77:9180 ✅
```

**Workaround:** Brug `nordkraft list` for at se den rigtige IPv6 adresse.

**Fix:** Deploy response vil ikke længere vise IPv6 - kun meddelelse om at bruge `nordkraft list`.

**Fix ETA:** Næste release (v0.3.0)

---

### Bug #3: CLI Mangler `restart` Kommando
**Severity:** 🟢 LAV  
**Status:** Identificeret, fix i gang  
**Opdaget:** 24. Januar 2026

**Problem:**

```bash
nordkraft restart myapp
# error: unrecognized subcommand 'restart'
```

**Workaround:**

```bash
nordkraft stop myapp
sleep 5
nordkraft start myapp
```

**Fix:** CLI får `restart` kommando (stop + wait + start).

**Fix ETA:** Næste release (v0.3.0)

---

## ✅ Verificerede Features (95% Pass Rate)

Følgende features er testet og virker:

### Authentication & Security
- ✅ Zero-trust auth via WireGuard
- ✅ IP → Public Key → User mapping
- ✅ Kata Containers VM isolation (kernel 6.18.5)
- ✅ Network isolation per user

### Container Lifecycle
- ✅ Deploy containers
- ✅ List containers
- ✅ Start containers
- ✅ Stop containers  
- ✅ Remove containers
- ✅ View logs
- ✅ Alias system

### Networking
- ✅ IPv4 private networking (172.21.x.x)
- ✅ IPv6 dual-stack via SLAAC
- ✅ Direct container access over VPN
- ✅ Port exposure

### Advanced Features
- ✅ Environment variables
- ✅ Persistent volumes
- ✅ Data persistence across restarts
- ✅ Volume cleanup on removal
- ✅ HTTPS ingress (*.nordkraft.cloud)
- ✅ Let's Encrypt TLS certificates
- ✅ IPv6 firewall control
- ✅ Multi-node orchestration (NATS)

### Tested Workloads
- ✅ Web servers (nginx)
- ✅ Databases (PostgreSQL)
- ✅ Custom images
- ✅ Multi-port applications

---

## 📊 Test Statistik

**Test udført:** 24. Januar 2026  
**Total features testet:** 20  
**Fungerer korrekt:** 19  
**Kendte bugs:** 3  
**Success rate:** 95%

**Test miljø:**
- Hardware: Dell OptiPlex (x86_64) + Raspberry Pi 4 (controller)
- Runtime: nerdctl + Kata Containers 6.18.5
- Netværk: Dual-stack (IPv4 + IPv6 macvlan)
- Firewall: pfSense Netgate 4200

---

## 🔄 Release Roadmap

### v0.3.0 (Februar 2026) - Bug Fixes
- 🔧 Fix CPU/memory limits
- 🔧 Fix IPv6 display
- 🔧 Add restart command
- ✨ Improved error messages

### v0.4.0 (Marts 2026) - Volume Backups
- ✨ Volume backup management
- ✨ Backup download API
- ✨ Restore from backup

### v1.0.0 (Q2 2026) - Mark I Production Ready
- ✨ All features stable
- ✨ Comprehensive documentation
- ✨ Production SLAs
- ✨ Multi-garage federation

---

## 💬 Rapporter Bugs

Fundet en ny bug? Hjælp os med at forbedre platformen:

**Email:** frederikkarlsson@me.com  
**GitHub:** [github.com/ft-karlsson/nordkraft-io-garage-cloud/issues](https://github.com/ft-karlsson/nordkraft-io-garage-cloud/issues)

Inkluder gerne:
- CLI version (`nordkraft --version`)
- Fuld kommando du kørte
- Output/fejlbesked
- Forventet resultat

---

<small>
Sidste opdatering: 24. Januar 2026
</small>
