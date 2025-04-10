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

pub enum Urgency {
    Low = 0,
    Normal = 1,
    Critical = 2,
}

pub struct NotificationBuilder<'a> {
    summary: &'a str,
    body: &'a str,
    icon: &'a str,
    actions: Vec<Action<'a>>,
    urgency: Urgency,
}

struct Action<'a> {
    key: &'a str,
    name: &'a str,
}

pub fn notify<'a>() -> NotificationBuilder<'a> {
    NotificationBuilder {
        body: "",
        summary: "",
        icon: "nix-snowflake",
        urgency: Urgency::Low,
        actions: Vec::new(),
    }
}

impl<'a> NotificationBuilder<'a> {
    pub fn with_summary(mut self, summary: &'a str) -> Self {
        self.summary = summary;
        self
    }

    pub fn with_body(mut self, body: &'a str) -> Self {
        self.body = body;
        self
    }

    pub fn with_urgency(mut self, urgency: Urgency) -> Self {
        self.urgency = urgency;
        self
    }

    pub fn with_action(mut self, key: &'a str, name: &'a str) -> Self {
        self.actions.push(Action { key, name });
        self
    }

    pub fn send(self) -> zbus::Result<()> {
        let conn = zbus::blocking::Connection::session()?;
        let proxy = NotificationsProxyBlocking::new(&conn)?;

        let mut hints = HashMap::new();
        hints.insert("urgency", zbus::zvariant::Value::U8(self.urgency as u8));

        let flattened_actions: Vec<_> = self
            .actions
            .into_iter()
            .flat_map(|action| [action.key, action.name])
            .collect();

        proxy.notify(
            "nh",
            0, // Let server choose id
            self.icon,
            self.summary,
            self.body,
            &flattened_actions,
            hints,
            -1, // Let server choose timeout
        )?;

        Ok(())
    }
}
