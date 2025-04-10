use std::collections::HashMap;

#[zbus::proxy(
    interface = "org.freedesktop.Notifications",
    default_service = "org.freedesktop.Notifications",
    default_path = "/org/freedesktop/Notifications"
)]
trait Notifications {
    #[allow(clippy::too_many_arguments)]
    fn notify(
        &self,
        app_name: &str,
        replaces_id: u32,
        app_icon: &str,
        summary: &str,
        body: &str,
        actions: &[&str],
        hints: HashMap<&str, zbus::zvariant::Value<'_>>,
        expire_timeout: i32,
    ) -> zbus::Result<u32>;
}

pub struct NotificationBuilder<'a> {
    proxy: NotificationsProxyBlocking<'a>,
    summary: &'a str,
    body: &'a str,
    icon: &'a str,
}

pub fn notify<'a>(summary: &'a str, body: &'a str) -> zbus::Result<NotificationBuilder<'a>> {
    let conn = zbus::blocking::Connection::session()?;
    let proxy = NotificationsProxyBlocking::new(&conn)?;

    Ok(NotificationBuilder {
        proxy,
        body,
        summary,
        icon: "nix-snowflake",
    })
}

impl NotificationBuilder<'_> {
    pub fn send(self) -> zbus::Result<()> {
        let mut hints = HashMap::new();
        hints.insert("urgency", zbus::zvariant::Value::U8(0));

        self.proxy.notify(
            "nh",
            0, // Let server choose id
            self.icon,
            self.summary,
            self.body,
            &[],
            hints,
            -1, // Let server choose timeout
        )?;

        Ok(())
    }
}
