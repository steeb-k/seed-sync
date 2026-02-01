# S.E.E.D. - Secure Environment Exchange Daemon

A BitTorrent-based folder synchronization application that keeps folders identical across machines.

## Overview

S.E.E.D. provides secure, decentralized folder synchronization using BitTorrent technology. It supports:

- **Read/Write (RW) keys**: Full access to modify and sync files (keep secret!)
- **Read-Only (RO) keys**: Download-only access, safe to share widely
- **Ignore patterns**: Exclude files from syncing (gitignore-style)
- **Windows system tray**: Runs in background with tray icon

## Projects

| Project | Description |
|---------|-------------|
| `SeedSync.Core` | Core sync engine, key generation, and access control |
| `SeedSync.Daemon` | Background service with HTTP API on localhost:9876 |
| `SeedSync.Cli` | Command-line interface |
| `SeedSync.App` | WinUI 3 desktop application |
| `SeedSync.Tests` | Unit tests |

## Getting Started

### Prerequisites

- .NET 8.0 SDK (for Core, Daemon, CLI, Tests)
- .NET 10.0 SDK (for WinUI App)
- Windows 10 (build 19041+) for the WinUI app

### Build

```bash
dotnet build
```

### Run Tests

```bash
dotnet test
```

### Run the Daemon

```bash
dotnet run --project src/SeedSync.Daemon
```

The daemon listens on `http://127.0.0.1:9876`.

### Use the CLI

```bash
# Create a new share
dotnet run --project src/SeedSync.Cli -- create "C:\path\to\folder"

# Add an existing share
dotnet run --project src/SeedSync.Cli -- add "SEEDRO..." "C:\path\to\save"

# List shares
dotnet run --project src/SeedSync.Cli -- list

# Get status
dotnet run --project src/SeedSync.Cli -- status

# Remove a share
dotnet run --project src/SeedSync.Cli -- remove <share-id>
```

### Run the WinUI App

```bash
dotnet run --project src/SeedSync.App
```

## API Endpoints

| Endpoint | Method | Description |
|----------|--------|-------------|
| `/api/shares` | GET | List all shares |
| `/api/shares/{id}` | GET | Get share status |
| `/api/shares` | POST | Create new share |
| `/api/shares/add` | POST | Add existing share |
| `/api/shares/{id}` | DELETE | Remove share |
| `/api/health` | GET | Health check |

## Key Format

- RW keys start with `SEEDRW` followed by share ID and random data
- RO keys start with `SEEDRO` followed by share ID and random data
- Keys are cryptographically random and cannot be guessed from each other

## License

GPLv3
