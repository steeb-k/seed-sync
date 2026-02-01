# End-to-End Test Procedure

This document describes how to manually test S.E.E.D. synchronization between two instances.

## Prerequisites

- Two Windows machines (or VMs) on the same network
- .NET 8.0 and .NET 10.0 SDK installed on both
- Firewall configured to allow inbound connections on dynamic ports (or temporarily disabled for testing)

## Test Setup

### Machine A (Creator/RW)

1. Clone and build the project:
   ```powershell
   git clone <repo-url> seed-sync
   cd seed-sync
   dotnet build
   ```

2. Create a test folder with some files:
   ```powershell
   mkdir C:\SeedSyncTest\SharedFolder
   echo "Hello from Machine A" > C:\SeedSyncTest\SharedFolder\test.txt
   echo "Another file" > C:\SeedSyncTest\SharedFolder\data.txt
   ```

3. Start the daemon:
   ```powershell
   dotnet run --project src/SeedSync.Daemon
   ```

4. In another terminal, create a share:
   ```powershell
   dotnet run --project src/SeedSync.Cli -- create "C:\SeedSyncTest\SharedFolder"
   ```

5. **Save the output** - you'll get two keys:
   - `SEEDRW...` (Read/Write key - keep secret)
   - `SEEDRO...` (Read-Only key - share with Machine B)

### Machine B (Joiner/RO)

1. Clone and build the project (same as Machine A)

2. Create destination folder:
   ```powershell
   mkdir C:\SeedSyncTest\ReceivedFolder
   ```

3. Start the daemon:
   ```powershell
   dotnet run --project src/SeedSync.Daemon
   ```

4. Join the share using the RO key from Machine A:
   ```powershell
   dotnet run --project src/SeedSync.Cli -- add "SEEDRO<key-from-machine-a>" "C:\SeedSyncTest\ReceivedFolder"
   ```

## Test Cases

### Test 1: Initial Sync

**Expected**: Files from Machine A appear in Machine B's folder

1. On Machine B, check the received folder:
   ```powershell
   dir C:\SeedSyncTest\ReceivedFolder
   ```

2. Verify `test.txt` and `data.txt` exist with correct content

**Result**: [ ] PASS / [ ] FAIL

### Test 2: Live File Addition

**Expected**: New files on Machine A sync to Machine B

1. On Machine A, create a new file:
   ```powershell
   echo "New content" > C:\SeedSyncTest\SharedFolder\newfile.txt
   ```

2. Wait 5-10 seconds for sync

3. On Machine B, verify the file appeared:
   ```powershell
   type C:\SeedSyncTest\ReceivedFolder\newfile.txt
   ```

**Result**: [ ] PASS / [ ] FAIL

### Test 3: Live File Modification

**Expected**: Modified files on Machine A sync to Machine B

1. On Machine A, modify an existing file:
   ```powershell
   echo "Modified content" >> C:\SeedSyncTest\SharedFolder\test.txt
   ```

2. Wait 5-10 seconds for sync

3. On Machine B, verify the modification:
   ```powershell
   type C:\SeedSyncTest\ReceivedFolder\test.txt
   ```

**Result**: [ ] PASS / [ ] FAIL

### Test 4: File Deletion

**Expected**: Deleted files on Machine A are removed from Machine B

1. On Machine A, delete a file:
   ```powershell
   del C:\SeedSyncTest\SharedFolder\data.txt
   ```

2. Wait 5-10 seconds for sync

3. On Machine B, verify the file is gone:
   ```powershell
   dir C:\SeedSyncTest\ReceivedFolder
   ```

**Result**: [ ] PASS / [ ] FAIL

### Test 5: Read-Only Restriction

**Expected**: Local changes on RO peer do NOT propagate back

1. On Machine B (RO), create a local file:
   ```powershell
   echo "RO local file" > C:\SeedSyncTest\ReceivedFolder\ro-local.txt
   ```

2. Wait 10 seconds

3. On Machine A, verify the file did NOT appear:
   ```powershell
   dir C:\SeedSyncTest\SharedFolder
   # Should NOT contain ro-local.txt
   ```

**Result**: [ ] PASS / [ ] FAIL

### Test 6: GUI Application

**Expected**: WinUI app works correctly

1. On Machine A, run the GUI:
   ```powershell
   dotnet run --project src/SeedSync.App
   ```

2. Verify:
   - [ ] Window opens with S.E.E.D. title
   - [ ] Existing share appears in list
   - [ ] Tray icon appears in system tray
   - [ ] Closing window hides to tray
   - [ ] Double-clicking tray icon restores window
   - [ ] Right-click tray menu is dark (if system dark mode)
   - [ ] Quit from tray menu closes the app

**Result**: [ ] PASS / [ ] FAIL

### Test 7: Share Status

**Expected**: Status API returns correct information

1. Check share status via CLI:
   ```powershell
   dotnet run --project src/SeedSync.Cli -- status
   ```

2. Verify output shows:
   - Share ID
   - Status (Syncing/UpToDate)
   - Connected peers count
   - Progress percentage

**Result**: [ ] PASS / [ ] FAIL

## Cleanup

After testing, clean up:

```powershell
# Stop daemons (Ctrl+C in terminal windows)

# Remove test folders
Remove-Item -Recurse -Force C:\SeedSyncTest
```

## Troubleshooting

### Peers not connecting

1. Check Windows Firewall - may need to allow the app through
2. Ensure both machines are on the same network
3. Check daemon is running: `curl http://127.0.0.1:9876/api/health`

### Files not syncing

1. Check share status: `dotnet run --project src/SeedSync.Cli -- status`
2. Look for errors in daemon console output
3. Verify torrent was created: check `%LOCALAPPDATA%\SeedSync` for `.torrent` files

### GUI not starting

1. Ensure .NET 10.0 SDK is installed
2. Try building explicitly: `dotnet build src/SeedSync.App`
3. Check for XAML errors in build output

## Test Summary

| Test | Description | Result |
|------|-------------|--------|
| 1 | Initial Sync | |
| 2 | Live File Addition | |
| 3 | Live File Modification | |
| 4 | File Deletion | |
| 5 | Read-Only Restriction | |
| 6 | GUI Application | |
| 7 | Share Status | |

**Overall Result**: _____ / 7 tests passed

**Tester**: ________________  
**Date**: ________________  
**Version**: ________________
