Notes from the AI:

Next steps to consider:
[] Add actual file watching to detect local changes
[] Add toast notifications for sync events
[] Test peer-to-peer sync between multiple machines
[] Implement installer and Windows service for the daemon
[] Implement the startup registration (registry Run key)


# S.E.E.D.
## Secure Environment Exchange Daemon

## What it should be
* Open sourced under GPLv3
* BitTorrent-based syncing agent
* Function like Resilio sync, with a master key for read/write access and read-only access
* Allows the initial creator of the sync to select default folder locations for the clients that can be overwritten
* NOT intended for backup -- we don't need much in the way of resiliency, this is to make sure folders are identical across machines at all times

## What it should look like
* For now, focusing on Windows - Linux and macOS may be stretch goals, so we may want a good terminal version while we flesh out the UI version which will be it's primary focus
* WinUI -- with nice acrylic/mica effects in all
* Dead simple GUI -- the main interface should basically just have a + button in the top left that has 2 options -- add an existing share or creating new share. Below, just a list of active shares already setup on the device. 
** Creating a new share should have the user select the folder they want to share, set a default directory for clients, and the ability to set up an ignore list for files not to sync. Keys should be a randomly generated string that can't be remade on any other machine. RW and RO keys should be distinctive and distinguishable. 
** Adding an existing share should prompt for the key and auto-populate the default save location with the ability to change it. Users should be notified if they have entered a RW key and warned that any changes they make will affect all users.
* Toast notifications for errors and successful initial syncs.

## Stretch goals (we don't need to focus on these too hard right now)
* Linux (GTK) and macOS native-looking UIs