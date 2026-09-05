# Deploy din egen app

Du har bygget noget. Nu skal det køre — på dit eget stykke dansk hardware, bag dit eget domæne, uden at betale pr. måned til en cloud-gigant.

Denne guide bygger en lille Node.js app fra bunden og deployer den på nordkraft.io med privat registry. Har du allerede en app, brug Dockerfile-eksemplerne og spring til [trin 3](#trin-3-byg-dit-image).

**Tidsforbrug:** Ca. 20 minutter første gang.

---

## Hvad du skal bruge

- [x] Docker eller Podman installeret lokalt
- [x] nordkraft.io CLI og VPN forbundet
- [x] `nordkraft registry init` kørt (se trin 2)

---

## Trin 1: Lav en simpel app

Vi starter med noget konkret — en lille Node.js webserver.

```bash
mkdir myapp && cd myapp
```

Opret `server.js`:

```javascript
const http = require('http');

const server = http.createServer((req, res) => {
  res.writeHead(200, { 'Content-Type': 'text/plain' });
  res.end('Hej fra nordkraft.io! 🚀\n');
});

server.listen(3000, () => {
  console.log('Server kører på port 3000');
});
```

Opret `package.json`:

```json
{
  "name": "myapp",
  "version": "1.0.0",
  "scripts": {
    "start": "node server.js"
  }
}
```

Opret `Dockerfile`:

```dockerfile
FROM node:20-alpine
WORKDIR /app
COPY package*.json ./
RUN npm install
COPY . .
EXPOSE 3000
CMD ["node", "server.js"]
```

!!! tip "Alpine images"
    Brug altid `:alpine` varianter — de er 3-5x mindre end standard images og starter hurtigere.

---

## Trin 2: Sæt privat registry op

nordkraft.io har et indbygget privat OCI-registry der kører direkte på din instans. Du behøver aldrig gøre dine images offentlige.

```bash
nordkraft registry init
```

```
✅ Registry kører på 172.21.1.3:5001
   Gemt i ~/.nordkraft/registry.json
```

!!! note "Kun én gang"
    `registry init` kører du én gang nogensinde. Registry overlever genstarter og bliver ved med at køre.

---

## Trin 3: Byg dit image

Byg image lokalt med Docker eller Podman:

=== "Docker"

    ```bash
    docker build -t myapp:v1 .
    ```

=== "Podman"

    ```bash
    podman build -t myapp:v1 .
    ```

Output ser nogenlunde sådan her ud:

```
[1/1] STEP 1/5: FROM node:20-alpine
[1/1] STEP 2/5: WORKDIR /app
[1/1] STEP 3/5: COPY package*.json ./
[1/1] STEP 4/5: RUN npm install
[1/1] STEP 5/5: CMD ["node", "server.js"]
Successfully tagged localhost/myapp:v1
```

Test at det virker lokalt inden du deployer:

=== "Docker"

    ```bash
    docker run -p 3000:3000 myapp:v1
    # Åbn http://localhost:3000
    ```

=== "Podman"

    ```bash
    podman run -p 3000:3000 myapp:v1
    # Åbn http://localhost:3000
    ```

Ser du `Hej fra nordkraft.io! 🚀`? Godt. Videre.

---

## Trin 4: Push til nordkraft.io registry

```bash
nordkraft push myapp:v1
```

nordkraft.io tager dit lokalt byggede image og pusher det til dit private registry på din instans.

Se hvad der ligger der:

```bash
nordkraft registry list
```

```
IMAGE     TAG    SIZE     PUSHED
myapp     v1     48 MB    lige nu
```

---

## Trin 5: Deploy

```bash
nordkraft deploy registry://myapp:v1 \
  -p 3000 \
  --name myapp
```

Tjek at den kører:

```bash
nordkraft container logs myapp --lines 20
# → Server kører på port 3000
```

### Med environment variabler

Har din app brug for config — database URL, API keys, secrets:

```bash
nordkraft deploy registry://myapp:v1 \
  -p 3000 \
  -e NODE_ENV=production \
  -e DATABASE_URL=postgres://user:pass@172.21.1.5:5432/mydb \
  -e SECRET_KEY=hemmelig123 \
  --name myapp
```

### Med persistent storage

Skal din app gemme filer der overlever genstarter:

```bash
nordkraft deploy registry://myapp:v1 \
  -p 3000 \
  --persistence --volume-path /app/storage \
  -e NODE_ENV=production \
  --name myapp
```

---

## Trin 6: Gør den tilgængelig

```bash
nordkraft ingress enable myapp --subdomain myapp --port 3000
```

```
✅ https://myapp.nordkraft.cloud er nu aktiv
```

Del linket. Din app kører nu på dansk hardware bag HTTPS. 🇩🇰

---

## Opdater til ny version

Koden er ændret — ny version klar:

```bash
# Byg ny version
docker build -t myapp:v2 .

# Push til registry
nordkraft push myapp:v2

# Deploy ny ved siden af den gamle (zero-downtime)
nordkraft deploy registry://myapp:v2 -p 3000 --name myapp-v2

# Flyt trafik til den nye
nordkraft ingress disable myapp
nordkraft ingress enable myapp-v2 --subdomain myapp --port 3000

# Slet den gamle når du er tilfreds
nordkraft container rm myapp
```

---

## Dockerfile eksempler

=== "Node.js"

    ```dockerfile
    FROM node:20-alpine
    WORKDIR /app
    COPY package*.json ./
    RUN npm ci --only=production
    COPY . .
    EXPOSE 3000
    CMD ["node", "server.js"]
    ```

=== "Python"

    ```dockerfile
    FROM python:3.12-slim
    WORKDIR /app
    COPY requirements.txt .
    RUN pip install --no-cache-dir -r requirements.txt
    COPY . .
    EXPOSE 8000
    CMD ["python", "-m", "uvicorn", "main:app", "--host", "0.0.0.0", "--port", "8000"]
    ```

=== "Go"

    ```dockerfile
    FROM golang:1.22-alpine AS builder
    WORKDIR /app
    COPY go.* ./
    RUN go mod download
    COPY . .
    RUN go build -o app .

    FROM alpine:latest
    COPY --from=builder /app/app /app
    EXPOSE 8080
    CMD ["/app"]
    ```

=== "Static (React/Vue/Svelte)"

    ```dockerfile
    FROM node:20-alpine AS builder
    WORKDIR /app
    COPY package*.json ./
    RUN npm ci
    COPY . .
    RUN npm run build

    FROM nginx:alpine
    COPY --from=builder /app/dist /usr/share/nginx/html
    EXPOSE 80
    ```

---

## Hurtig reference

| Handling | Kommando |
|----------|----------|
| Byg image | `docker build -t myapp:v1 .` |
| Opsæt registry | `nordkraft registry init` |
| Push image | `nordkraft push myapp:v1` |
| List images | `nordkraft registry list` |
| Deploy | `nordkraft deploy registry://myapp:v1 -p 3000` |
| Registry status | `nordkraft registry status` |
| Fjern registry | `nordkraft registry destroy` |
