//! Windows notifications, which an unpackaged app can only send under an app id registered for the user.

use std::path::Path;

use windows::core::{Result, HSTRING};
use windows::Data::Xml::Dom::XmlDocument;
use windows::Win32::System::Registry::{RegSetKeyValueW, HKEY_CURRENT_USER, REG_SZ};
use windows::Win32::UI::Shell::SetCurrentProcessExplicitAppUserModelID;
use windows::UI::Notifications::{NotificationSetting, ToastNotification, ToastNotificationManager, ToastNotifier};

pub fn key(id: &str) -> String {
    format!(r"Software\Classes\AppUserModelId\{id}")
}

/// Names the app and its icon in notifications and gives this process the id they're sent under.
pub fn register(id: &str, name: &str, icon: &Path) -> Result<()> {
    let key = HSTRING::from(key(id));
    for (value, data) in [("DisplayName", name.to_string()), ("IconUri", icon.display().to_string())] {
        let data = HSTRING::from(data);
        unsafe {
            RegSetKeyValueW(
                HKEY_CURRENT_USER,
                &key,
                &HSTRING::from(value),
                REG_SZ.0,
                Some(data.as_ptr().cast()),
                ((data.len() + 1) * 2) as u32,
            )
        }
        .ok()?;
    }
    unsafe { SetCurrentProcessExplicitAppUserModelID(&HSTRING::from(id)) }
}

/// Clicking the notification or its Open button opens `open` in its default app.
pub fn show(id: &str, title: &str, body: &str, open: Option<&Path>, popup: bool) -> std::result::Result<(), String> {
    let notifier =
        ToastNotificationManager::CreateToastNotifierWithId(&HSTRING::from(id)).map_err(|e| e.to_string())?;
    // windows drops notifications it won't show without an error, so say why up front
    let off = match notifier.Setting().map_err(|e| e.to_string())? {
        NotificationSetting::Enabled => return send(&notifier, title, body, open, popup).map_err(|e| e.to_string()),
        NotificationSetting::DisabledForUser => "notifications are off in Windows settings",
        NotificationSetting::DisabledForApplication => "notifications from clip are off in Windows settings",
        NotificationSetting::DisabledByGroupPolicy => "notifications are turned off by policy",
        _ => "Windows won't show notifications from clip",
    };
    Err(off.into())
}

fn send(notifier: &ToastNotifier, title: &str, body: &str, open: Option<&Path>, popup: bool) -> Result<()> {
    let (launch, action) = match open {
        Some(path) => {
            let uri = escape(&file_uri(path));
            (
                format!(r#" launch="{uri}" activationType="protocol""#),
                format!(r#"<actions><action content="Open" activationType="protocol" arguments="{uri}"/></actions>"#),
            )
        }
        None => Default::default(),
    };
    let xml = format!(
        r#"<toast{launch}><visual><binding template="ToastGeneric"><text>{}</text><text>{}</text></binding></visual>{action}</toast>"#,
        escape(title),
        escape(body)
    );
    let doc = XmlDocument::new()?;
    doc.LoadXml(&HSTRING::from(xml))?;
    let toast = ToastNotification::CreateToastNotification(&doc)?;
    toast.SetSuppressPopup(!popup)?;
    notifier.Show(&toast)
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&apos;")
}

fn file_uri(path: &Path) -> String {
    let mut uri = String::from("file:///");
    for byte in path.to_string_lossy().replace('\\', "/").bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' | b':' => uri.push(byte as char),
            _ => uri.push_str(&format!("%{byte:02X}")),
        }
    }
    uri
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::System::Registry::RegDeleteTreeW;

    #[test]
    fn a_clip_path_becomes_a_file_uri() {
        assert_eq!(
            file_uri(Path::new(r"D:\Videos\clip\Half & Half\2026-11-07 10-00-00 é.mp4")),
            "file:///D:/Videos/clip/Half%20%26%20Half/2026-11-07%2010-00-00%20%C3%A9.mp4"
        );
        assert_eq!(escape(r#"<a href="x">'&'</a>"#), "&lt;a href=&quot;x&quot;&gt;&apos;&amp;&apos;&lt;/a&gt;");
    }

    // sends a real notification without popping it up, then clears it and its app id
    #[test]
    #[ignore]
    fn windows_keeps_a_sent_notification() {
        let id = "lt.rot.clip-toast-test";
        register(id, "clip test", &std::env::temp_dir().join("none.png")).unwrap();
        let sent = show(id, "Clip saved", "Test & check", Some(Path::new(r"C:\Windows\win.ini")), false);
        let history = ToastNotificationManager::History().unwrap();
        // the notification lands in the history a moment after it's sent
        let kept = (0..20)
            .map(|_| {
                std::thread::sleep(std::time::Duration::from_millis(100));
                history.GetHistoryWithId(&HSTRING::from(id)).and_then(|toasts| toasts.Size())
            })
            .find(|kept| kept.as_ref().map_or(true, |&n| n > 0));
        let _ = history.ClearWithId(&HSTRING::from(id));
        let _ = unsafe { RegDeleteTreeW(HKEY_CURRENT_USER, &HSTRING::from(key(id))) };
        sent.unwrap();
        assert!(kept.expect("never kept").unwrap() >= 1);
    }
}
