//! Windows service integration (compiled only on Windows).
//!
//! `run_as_service` is the SCM entry point: it registers a control handler and
//! runs the same [`crate::serve`] loop as console mode, stopping gracefully on a
//! Stop/Shutdown control. `manage` implements the install/uninstall/start/stop
//! subcommands via the service manager.
//!
//! The service is installed to run as LocalSystem; the GUI runs as the logged-in
//! user. They meet over the IPC pipe: both derive its path from the machine-wide
//! data dir (`seed_ipc::machine_data_dir`, `crate::default_data_dir`), and the
//! pipe is created with a permissive DACL (`seed_ipc::transport`) so the user can
//! open a pipe the service created. Seeds the daemon stores live in the service
//! account's credential vault, which is fine — only the daemon needs them.

use std::ffi::{OsStr, OsString};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Notify;
use windows_service::{
    define_windows_service,
    service::{
        ServiceAccess, ServiceAction, ServiceActionType, ServiceControl, ServiceControlAccept,
        ServiceErrorControl, ServiceExitCode, ServiceFailureActions, ServiceFailureResetPeriod,
        ServiceInfo, ServiceStartType, ServiceState, ServiceStatus, ServiceType,
    },
    service_control_handler::{self, ServiceControlHandlerResult},
    service_dispatcher,
    service_manager::{ServiceManager, ServiceManagerAccess},
    Result as WsResult,
};

const SERVICE_NAME: &str = "SeedSyncDaemon";
const SERVICE_DISPLAY: &str = "SEED Sync";
const SERVICE_TYPE: ServiceType = ServiceType::OWN_PROCESS;

/// SCM entry point (invoked when started by `seed-daemon service`).
pub fn run_as_service() -> WsResult<()> {
    service_dispatcher::start(SERVICE_NAME, ffi_service_main)
}

define_windows_service!(ffi_service_main, service_main);

fn service_main(_arguments: Vec<OsString>) {
    if let Err(e) = run_service() {
        tracing::error!("service exited with error: {e}");
    }
}

fn run_service() -> WsResult<()> {
    let shutdown = Arc::new(Notify::new());

    let handler_shutdown = shutdown.clone();
    let event_handler = move |control: ServiceControl| -> ServiceControlHandlerResult {
        match control {
            ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
            ServiceControl::Stop | ServiceControl::Shutdown => {
                handler_shutdown.notify_one();
                ServiceControlHandlerResult::NoError
            }
            _ => ServiceControlHandlerResult::NotImplemented,
        }
    };
    let status_handle = service_control_handler::register(SERVICE_NAME, event_handler)?;

    let set_state = |state: ServiceState, accepts: ServiceControlAccept| ServiceStatus {
        service_type: SERVICE_TYPE,
        current_state: state,
        controls_accepted: accepts,
        exit_code: ServiceExitCode::Win32(0),
        checkpoint: 0,
        wait_hint: Duration::default(),
        process_id: None,
    };

    status_handle.set_service_status(set_state(
        ServiceState::Running,
        ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN,
    ))?;

    // We are LocalSystem here: make sure the host lets this binary be reached
    // and restarts us if we give up (both idempotent, both best-effort).
    provision_host();

    // Run the daemon until the SCM asks us to stop.
    let data_dir = crate::default_data_dir();
    let socket = crate::default_socket(&data_dir);
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    let result = rt.block_on(async move {
        crate::serve(data_dir, socket, async move {
            shutdown.notified().await;
        })
        .await
    });
    if let Err(e) = result {
        tracing::error!("daemon serve error: {e}");
    }

    status_handle.set_service_status(set_state(
        ServiceState::Stopped,
        ServiceControlAccept::empty(),
    ))?;
    Ok(())
}

/// Name of the inbound firewall rule the service maintains for itself.
const SELF_FIREWALL_RULE: &str = "SEED Sync daemon (self)";

/// Host provisioning the service does for itself every start, as LocalSystem:
///
/// 1. An inbound Windows Firewall allow rule for **this exact binary** on
///    **every** profile. The MSI installs rules too, but they proved fragile in
///    the field: the 0.7.3 rollout's two same-named exceptions collapsed into
///    one (private only), and any adapter Windows classifies as *Public* — a VPN
///    or overlay adapter, a hotel network — was left with no rule at all, so
///    unsolicited inbound QUIC from a member over that path was silently
///    dropped (2026-09 two-member outage, known-issues #36). A service never
///    gets the interactive firewall prompt, and a per-program rule only admits
///    traffic to this daemon's own authenticated QUIC socket, so allowing it on
///    every profile is the right default. Re-checked on every start, so an
///    updated install path or a deleted rule heals itself without an MSI.
/// 2. Service failure actions: restart after 5 s / 30 s / 60 s, reset daily,
///    and treat a non-zero exit as a failure. This is what makes rung 3 of the
///    transport-repair ladder (`std::process::exit(3)`) a restart rather than
///    a dead service.
///
/// Every step logs and continues on error — provisioning must never stop the
/// daemon from starting.
pub fn provision_host() {
    provision_firewall();
    provision_failure_actions();
}

fn provision_firewall() {
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let exe = exe.to_string_lossy().to_string();
    let name_arg = format!("name={SELF_FIREWALL_RULE}");

    // Already present for this exact path? (`verbose` prints the Program line.)
    if let Ok(out) = std::process::Command::new("netsh")
        .args([
            "advfirewall",
            "firewall",
            "show",
            "rule",
            &name_arg,
            "verbose",
        ])
        .output()
    {
        let text = String::from_utf8_lossy(&out.stdout).to_lowercase();
        if out.status.success()
            && text.contains(&exe.to_lowercase())
            && text.contains("profiles:")
            && text.contains("any")
        {
            return;
        }
    }
    // Replace whatever is there under our name (stale path, wrong profile).
    let _ = std::process::Command::new("netsh")
        .args(["advfirewall", "firewall", "delete", "rule", &name_arg])
        .output();
    let program_arg = format!("program={exe}");
    match std::process::Command::new("netsh")
        .args([
            "advfirewall",
            "firewall",
            "add",
            "rule",
            &name_arg,
            "dir=in",
            "action=allow",
            &program_arg,
            "enable=yes",
            "profile=any",
            "description=Inbound QUIC for the SEED Sync daemon (maintained by the service itself).",
        ])
        .output()
    {
        Ok(out) if out.status.success() => {
            tracing::info!("firewall: installed inbound rule '{SELF_FIREWALL_RULE}' for {exe}")
        }
        Ok(out) => tracing::warn!(
            "firewall: could not install inbound rule '{SELF_FIREWALL_RULE}': {}{}",
            String::from_utf8_lossy(&out.stdout).trim(),
            String::from_utf8_lossy(&out.stderr).trim()
        ),
        Err(e) => tracing::warn!("firewall: netsh unavailable: {e}"),
    }
}

fn provision_failure_actions() {
    let result = (|| -> WsResult<()> {
        let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)?;
        // ChangeServiceConfig2 rejects a RESTART action unless the handle also
        // carries SERVICE_START (observed on the 0.7.4 rollout as "IO error in
        // winapi call"; the firewall half of provisioning had already succeeded).
        let service = manager.open_service(
            SERVICE_NAME,
            ServiceAccess::CHANGE_CONFIG | ServiceAccess::QUERY_CONFIG | ServiceAccess::START,
        )?;
        service.update_failure_actions(ServiceFailureActions {
            reset_period: ServiceFailureResetPeriod::After(Duration::from_secs(24 * 3600)),
            reboot_msg: None,
            command: None,
            actions: Some(vec![
                ServiceAction {
                    action_type: ServiceActionType::Restart,
                    delay: Duration::from_secs(5),
                },
                ServiceAction {
                    action_type: ServiceActionType::Restart,
                    delay: Duration::from_secs(30),
                },
                ServiceAction {
                    action_type: ServiceActionType::Restart,
                    delay: Duration::from_secs(60),
                },
            ]),
        })?;
        service.set_failure_actions_on_non_crash_failures(true)?;
        Ok(())
    })();
    if let Err(e) = result {
        tracing::warn!("service recovery: could not set failure actions: {e}");
    }
}

/// install / uninstall / start / stop via the service control manager.
pub fn manage(cmd: &str) -> WsResult<()> {
    let access = ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE;
    let manager = ServiceManager::local_computer(None::<&str>, access)?;

    match cmd {
        "install" => {
            let exe = std::env::current_exe().expect("current exe path");
            let info = ServiceInfo {
                name: OsString::from(SERVICE_NAME),
                display_name: OsString::from(SERVICE_DISPLAY),
                service_type: SERVICE_TYPE,
                start_type: ServiceStartType::AutoStart,
                error_control: ServiceErrorControl::Normal,
                executable_path: exe,
                launch_arguments: vec![OsString::from("service")],
                dependencies: vec![],
                account_name: None, // LocalSystem
                account_password: None,
            };
            let service = manager
                .create_service(&info, ServiceAccess::CHANGE_CONFIG | ServiceAccess::START)?;
            service.set_description("SEED Sync P2P mirrored-folder sync daemon")?;
            println!("installed service '{SERVICE_NAME}'");
        }
        "uninstall" => {
            let access = ServiceAccess::QUERY_STATUS | ServiceAccess::STOP | ServiceAccess::DELETE;
            let service = manager.open_service(SERVICE_NAME, access)?;
            if service.query_status()?.current_state != ServiceState::Stopped {
                let _ = service.stop();
            }
            service.delete()?;
            println!("uninstalled service '{SERVICE_NAME}'");
        }
        "start" => {
            let service = manager.open_service(SERVICE_NAME, ServiceAccess::START)?;
            service.start::<&OsStr>(&[])?;
            println!("started service '{SERVICE_NAME}'");
        }
        "stop" => {
            let service = manager.open_service(SERVICE_NAME, ServiceAccess::STOP)?;
            service.stop()?;
            println!("stopped service '{SERVICE_NAME}'");
        }
        other => anyhow_like(other),
    }
    Ok(())
}

fn anyhow_like(cmd: &str) {
    tracing::warn!("unknown service command: {cmd}");
}
