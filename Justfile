# Justfile for twitch-1337

# Default recipe - show available commands
default:
    @just --list

# Build the Docker image
build:
    docker build -t chronophylos/twitch-1337:latest .

# Build with no cache (force full rebuild)
build-no-cache:
    docker build --no-cache -t chronophylos/twitch-1337:latest .

# Push the image to docker host
push:
   podman save localhost/chronophylos/twitch-1337:latest | ssh docker.homelab 'docker load'

# Restart container on docker host
restart:
  ssh docker.homelab 'docker compose --ansi always --project-directory twitch up -d'

# Deploy image and restart pod
deploy: build push restart
