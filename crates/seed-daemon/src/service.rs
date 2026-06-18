//! Windows service integration (compiled only on Windows).
//!
//! `run_as_service` is the SCM entry point: it registers a control handler and
//! runs the same [`crate::serve`] loop as console mode, stopping gracefully on a
//! Stop/Shutdown control. `manage` implements the install/uninstall/start/stop
//! subcommands via the service manager.
//!
//! NOTE (validate on Windows): the service is installed to run as LocalSystem; a
//! GUI running as the logged-in user must be able to reach the IPC pipe and the
//! daemon must reach user-chosen folders — review the named-pipe naming
//! (interprocess `GenericFilePath` vs `GenericNamespaced`) and DACLs here.

use std::ffi::{OsStr, OsString};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Notify;
use windows_service::{
    define_windows_service,
    service::{
        ServiceAccess, ServiceControl, ServiceControlAccept, ServiceErrorControl, ServiceExitCode,
        ServiceInfo, ServiceStartType, ServiceState, ServiceStatus, ServiceType,
    },
    service_control_handler::{self, ServiceControlHandlerResult},
    service_dispatcher,
    service_manager::{ServiceManager, ServiceManagerAccess},
    Result as WsResult,
};

const SERVICE_NAME: &str = "SeedSyncDaemon";
const SERVICE_DISPLAY: &str = "Seed Sync";
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
            service.set_description("Seed Sync P2P mirrored-folder sync daemon")?;
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
