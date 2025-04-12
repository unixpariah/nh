use std::{
    collections::HashMap,
    sync::{mpsc, Arc},
    thread,
};

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

    fn get_capabilities(&self) -> zbus::Result<Box<[Box<str>]>>;

    #[zbus(signal)]
    fn action_invoked(id: u32, action_key: &str) -> zbus::Result<()>;

    #[zbus(signal)]
    fn notification_closed(id: u32, reason: u32) -> zbus::Result<()>;
}

pub enum Urgency {
    Low = 0,
    Normal = 1,
    Critical = 2,
}

#[derive(Debug)]
pub enum NotificationResponse {
    Action(Arc<str>),
    Dismissed,
}

pub struct Uninitialized;
pub struct HasSummary;

pub struct NotificationBuilder<'a, State = Uninitialized> {
    summary: &'a str,
    body: &'a str,
    icon: &'a str,
    actions: Vec<Action>,
    urgency: Urgency,
    _state: std::marker::PhantomData<State>,
}

pub struct Action {
    pub key: Arc<str>,
    pub name: Box<str>,
}

pub fn notify<'a>() -> NotificationBuilder<'a, Uninitialized> {
    NotificationBuilder {
        body: "",
        summary: "",
        icon: "nix-snowflake",
        urgency: Urgency::Normal,
        actions: Vec::new(),
        _state: std::marker::PhantomData,
    }
}

impl<'a> NotificationBuilder<'a, Uninitialized> {
    pub fn with_summary(self, summary: &'a str) -> NotificationBuilder<'a, HasSummary> {
        NotificationBuilder {
            summary,
            body: self.body,
            icon: self.icon,
            urgency: self.urgency,
            actions: self.actions,
            _state: std::marker::PhantomData,
        }
    }
}

impl<'a, S> NotificationBuilder<'a, S> {
    pub fn with_body(mut self, body: &'a str) -> Self {
        self.body = body;
        self
    }

    pub fn with_urgency(mut self, urgency: Urgency) -> Self {
        self.urgency = urgency;
        self
    }

    pub fn with_action<T, U>(mut self, key: T, name: U) -> Self
    where
        T: Into<Arc<str>>,
        U: Into<Box<str>>,
    {
        self.actions.push(Action {
            key: key.into(),
            name: name.into(),
        });
        self
    }
}

impl<'a> NotificationBuilder<'a, HasSummary> {
    pub fn send(self) -> zbus::Result<Option<NotificationResponse>> {
        let conn = zbus::blocking::Connection::session()?;
        let proxy = NotificationsProxyBlocking::new(&conn)?;

        // Check if notification server supports actions capability
        let capabilities = proxy.get_capabilities()?;
        if !capabilities.iter().any(|cap| **cap == *"actions") {
            return Ok(None);
        }

        let mut hints = HashMap::new();
        hints.insert("urgency", zbus::zvariant::Value::U8(self.urgency as u8));

        // The D-Bus notification spec expects the `actions` field to be a flat list of strings
        let flattened_actions: Vec<_> = self
            .actions
            .iter()
            .flat_map(|action| [&*action.key, &*action.name])
            .collect();

        let id = proxy.notify(
            "nh",
            0, // Let server choose id
            self.icon,
            self.summary,
            self.body,
            &flattened_actions,
            hints,
            -1, // Let server choose timeout
        )?;

        if self.actions.is_empty() {
            return Ok(None);
        }

        let (tx, rx) = mpsc::channel();

        {
            let tx = tx.clone();
            let notification_closed_stream = proxy.receive_notification_closed()?;
            thread::spawn(move || {
                notification_closed_stream.for_each(|notification_closed| {
                    if let Ok(args) = notification_closed.args() {
                        if args.id == id {
                            _ = tx.send(NotificationResponse::Dismissed);
                        }
                    }
                });
            });
        }

        let action_invoked_stream = proxy.receive_action_invoked()?;
        thread::spawn(move || {
            action_invoked_stream.for_each(|action_invoked| {
                if let Ok(args) = action_invoked.args() {
                    if args.id == id {
                        if let Some(action) = self
                            .actions
                            .iter()
                            .find(|action| &*action.key == args.action_key)
                        {
                            _ = tx.send(NotificationResponse::Action(Arc::clone(&action.key)));
                        }
                    }
                }
            });
        });

        Ok(rx.recv().ok())
    }
}
