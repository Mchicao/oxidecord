//! Windows native desktop toast notifications via `Windows.UI.Notifications`.
//!
//! Oxidecord toasts always use Oxidecord's own `AppUserModelID`
//! ([`APP_USER_MODEL_ID`]) and never impersonate system binaries.
//! Nothing is registered or touched at app startup while notifications are
//! opted out. When the user opts in, [`PlatformNotifier::ensure_opt_in_ready`]
//! sets the process AUMID and ensures a minimal Start Menu shortcut carrying
//! `System.AppUserModel.ID = "Oxidecord.Desktop"` exists in the user's Start
//! Menu folder so Windows allows unpackaged desktop toasts.
//!
//! Showing a toast never activates or foregrounds the app window. Only when
//! the user explicitly clicks the toast does the `Activated` callback grant
//! foreground permission (`AllowSetForegroundWindow`) and forward the
//! [`NotificationTarget`] to the GPUI foreground receiver.

use std::collections::VecDeque;
use std::fs;
use std::mem::ManuallyDrop;
use std::os::windows::ffi::OsStrExt as _;
use std::path::PathBuf;

use futures::channel::mpsc::UnboundedSender;
use windows::Data::Xml::Dom::XmlDocument;
use windows::Foundation::TypedEventHandler;
use windows::UI::Notifications::{
    NotificationSetting, ToastNotification, ToastNotificationManager, ToastNotifier,
};
use windows::Win32::Foundation::PROPERTYKEY;
use windows::Win32::System::Com::StructuredStorage::{
    PROPVARIANT, PROPVARIANT_0, PROPVARIANT_0_0, PROPVARIANT_0_0_0,
};
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx,
    CoUninitialize, IPersistFile,
};
use windows::Win32::System::Variant::VT_LPWSTR;
use windows::Win32::UI::Shell::PropertiesSystem::IPropertyStore;
use windows::Win32::UI::Shell::{IShellLinkW, SetCurrentProcessExplicitAppUserModelID, ShellLink};
use windows::Win32::UI::WindowsAndMessaging::{ASFW_ANY, AllowSetForegroundWindow};
use windows::core::{GUID, HSTRING, IInspectable, Interface, PCWSTR, PWSTR};

use super::{NotificationTarget, ToastPayload, render_toast_xml};

/// Explicit AppUserModelID for Oxidecord desktop notifications.
pub const APP_USER_MODEL_ID: &str = "Oxidecord.Desktop";

/// Relative path under `%APPDATA%` where per-user Start Menu shortcuts live.
const START_MENU_SHORTCUT_REL: &str = "Microsoft\\Windows\\Start Menu\\Programs\\Oxidecord.lnk";

/// Maximum number of active `ToastNotification` COM objects retained so their
/// `Activated` click handlers remain registered while visible in Action Center.
const MAX_ACTIVE_TOASTS: usize = 16;

/// `System.AppUserModel.ID` (`{9F4C2855-9F79-4B39-A8D0-E1D42DE1D5F3}, 5`).
const PKEY_APP_USER_MODEL_ID: PROPERTYKEY = PROPERTYKEY {
    fmtid: GUID::from_u128(0x9f4c2855_9f79_4b39_a8d0_e1d42de1d5f3),
    pid: 5,
};

pub(super) struct PlatformNotifier {
    opt_in_registered: bool,
    active_toasts: VecDeque<ToastNotification>,
}

impl PlatformNotifier {
    pub fn new() -> Self {
        // Do not touch process AUMID or Start Menu at startup; wait until
        // notifications are explicitly opted in.
        Self {
            opt_in_registered: false,
            active_toasts: VecDeque::with_capacity(MAX_ACTIVE_TOASTS),
        }
    }

    pub fn is_supported(&self) -> bool {
        true
    }

    /// Performs minimal Oxidecord AUMID registration when the user opts in to
    /// desktop notifications, and verifies that Windows allows notifications
    /// for `Oxidecord.Desktop`.
    pub fn ensure_opt_in_ready(&mut self) -> Result<(), String> {
        if cfg!(test) {
            self.opt_in_registered = true;
            return Ok(());
        }

        let aumid = HSTRING::from(APP_USER_MODEL_ID);
        unsafe {
            SetCurrentProcessExplicitAppUserModelID(&aumid).map_err(|err| {
                format!("Failed to set Oxidecord AppUserModelID ({APP_USER_MODEL_ID}): {err}")
            })?;
        }

        if !self.opt_in_registered {
            register_start_menu_shortcut()?;
            self.opt_in_registered = true;
        }

        let _ = create_oxidecord_notifier()?;
        Ok(())
    }

    pub fn show(
        &mut self,
        payload: &ToastPayload,
        click_tx: UnboundedSender<NotificationTarget>,
    ) -> Result<(), String> {
        // Never fire real OS desktop toasts during automated unit tests.
        if cfg!(test) {
            return Ok(());
        }

        self.ensure_opt_in_ready()?;
        let notifier = create_oxidecord_notifier()?;

        let xml_text = render_toast_xml(payload);
        let xml_doc = XmlDocument::new().map_err(|err| err.to_string())?;
        xml_doc
            .LoadXml(&HSTRING::from(xml_text.as_str()))
            .map_err(|err| err.to_string())?;

        let toast =
            ToastNotification::CreateToastNotification(&xml_doc).map_err(|err| err.to_string())?;

        let _ = toast.SetTag(&HSTRING::from(payload.tag.as_str()));
        let _ = toast.SetGroup(&HSTRING::from(payload.group.as_str()));

        let launch_arg = payload.launch_arg.clone();
        let fallback_target = payload.target;
        let handler =
            TypedEventHandler::<ToastNotification, IInspectable>::new(move |_sender, _args| {
                // Only when the user explicitly clicks the toast do we allow
                // this process to claim foreground focus on the main thread.
                unsafe {
                    let _ = AllowSetForegroundWindow(ASFW_ANY);
                }
                let target =
                    NotificationTarget::decode_launch_arg(&launch_arg).unwrap_or(fallback_target);
                let _ = click_tx.unbounded_send(target);
                Ok(())
            });
        toast.Activated(&handler).map_err(|err| err.to_string())?;

        notifier.Show(&toast).map_err(|err| {
            format!(
                "Windows refused toast for {APP_USER_MODEL_ID} (verify Start Menu shortcut and notification permissions): {err}"
            )
        })?;

        while self.active_toasts.len() >= MAX_ACTIVE_TOASTS {
            self.active_toasts.pop_front();
        }
        self.active_toasts.push_back(toast);

        Ok(())
    }

    pub fn clear(&mut self) {
        if cfg!(test) {
            self.active_toasts.clear();
            return;
        }

        let aumid = HSTRING::from(APP_USER_MODEL_ID);
        if let Ok(notifier) = ToastNotificationManager::CreateToastNotifierWithId(&aumid) {
            while let Some(toast) = self.active_toasts.pop_front() {
                let _ = notifier.Hide(&toast);
            }
        } else {
            self.active_toasts.clear();
        }
        if let Ok(history) = ToastNotificationManager::History() {
            let _ = history.ClearWithId(&aumid);
        }
    }
}

fn create_oxidecord_notifier() -> Result<ToastNotifier, String> {
    let aumid = HSTRING::from(APP_USER_MODEL_ID);
    let notifier = ToastNotificationManager::CreateToastNotifierWithId(&aumid).map_err(|err| {
        format!(
            "Desktop notifications require a registered Start Menu shortcut for {APP_USER_MODEL_ID}: {err}"
        )
    })?;

    match notifier.Setting() {
        Ok(NotificationSetting::Enabled) => Ok(notifier),
        Ok(NotificationSetting::DisabledForApplication) => {
            Err("Desktop notifications are disabled for Oxidecord in Windows Settings.".into())
        }
        Ok(NotificationSetting::DisabledForUser) => {
            Err("Desktop notifications are disabled for the current Windows user.".into())
        }
        Ok(NotificationSetting::DisabledByGroupPolicy) => {
            Err("Desktop notifications are disabled by Windows Group Policy.".into())
        }
        Ok(NotificationSetting::DisabledByManifest) => {
            Err("Desktop notifications are disabled by application manifest.".into())
        }
        Ok(_) => Ok(notifier),
        Err(err) => Err(format!(
            "Windows toast notifier is not ready for {APP_USER_MODEL_ID}: {err}"
        )),
    }
}

/// Ensures `%APPDATA%\Microsoft\Windows\Start Menu\Programs\Oxidecord.lnk`
/// exists with `PKEY_AppUserModel_ID = "Oxidecord.Desktop"`. Called only when
/// the user explicitly opts in to desktop notifications.
fn register_start_menu_shortcut() -> Result<(), String> {
    let appdata = std::env::var_os("APPDATA")
        .ok_or_else(|| "APPDATA environment variable is missing; cannot register Start Menu shortcut for desktop notifications.".to_string())?;
    let shortcut_path = PathBuf::from(appdata).join(START_MENU_SHORTCUT_REL);
    if shortcut_path.exists() {
        return Ok(());
    }
    if let Some(parent) = shortcut_path.parent() {
        fs::create_dir_all(parent).map_err(|err| {
            format!("Could not create Start Menu directory for Oxidecord shortcut: {err}")
        })?;
    }

    let exe_path = std::env::current_exe()
        .map_err(|err| format!("Could not resolve Oxidecord executable path: {err}"))?;
    let exe_wide = to_wide_null(exe_path.as_os_str());
    let workdir_wide = exe_path
        .parent()
        .map(|p| to_wide_null(p.as_os_str()))
        .unwrap_or_else(|| vec![0]);
    let desc_wide = to_wide_null(std::ffi::OsStr::new("Oxidecord"));
    let shortcut_wide = to_wide_null(shortcut_path.as_os_str());
    let mut aumid_wide = to_wide_null(std::ffi::OsStr::new(APP_USER_MODEL_ID));

    unsafe {
        let hr = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let should_uninit = hr.is_ok();
        let result = (|| -> Result<(), String> {
            let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER)
                .map_err(|err| format!("CoCreateInstance(ShellLink) failed: {err}"))?;
            link.SetPath(PCWSTR(exe_wide.as_ptr()))
                .map_err(|err| format!("IShellLinkW::SetPath failed: {err}"))?;
            link.SetWorkingDirectory(PCWSTR(workdir_wide.as_ptr()))
                .map_err(|err| format!("IShellLinkW::SetWorkingDirectory failed: {err}"))?;
            link.SetDescription(PCWSTR(desc_wide.as_ptr()))
                .map_err(|err| format!("IShellLinkW::SetDescription failed: {err}"))?;

            let store: IPropertyStore = link.cast().map_err(|err| {
                format!("QueryInterface(IPropertyStore) on ShellLink failed: {err}")
            })?;
            let propvar = ManuallyDrop::new(PROPVARIANT {
                Anonymous: PROPVARIANT_0 {
                    Anonymous: ManuallyDrop::new(PROPVARIANT_0_0 {
                        vt: VT_LPWSTR,
                        wReserved1: 0,
                        wReserved2: 0,
                        wReserved3: 0,
                        Anonymous: PROPVARIANT_0_0_0 {
                            pwszVal: PWSTR(aumid_wide.as_mut_ptr()),
                        },
                    }),
                },
            });
            store
                .SetValue(&PKEY_APP_USER_MODEL_ID, &*propvar)
                .map_err(|err| format!("IPropertyStore::SetValue(AUMID) failed: {err}"))?;
            store
                .Commit()
                .map_err(|err| format!("IPropertyStore::Commit failed: {err}"))?;

            let persist: IPersistFile = link
                .cast()
                .map_err(|err| format!("QueryInterface(IPersistFile) failed: {err}"))?;
            persist
                .Save(PCWSTR(shortcut_wide.as_ptr()), true)
                .map_err(|err| format!("IPersistFile::Save shortcut failed: {err}"))?;
            Ok(())
        })();

        if should_uninit {
            CoUninitialize();
        }
        result
    }
}

fn to_wide_null(s: &std::ffi::OsStr) -> Vec<u16> {
    s.encode_wide().chain(std::iter::once(0)).collect()
}
